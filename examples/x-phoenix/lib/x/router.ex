defmodule X.Router do
  @moduledoc """
  The X clone (examples/x), on the BEAM: the same routes, SQL, and answers.
  The tables are examples/x/schema.sql (the race seeds them first).
  """
  use Plug.Router

  plug :match
  plug Plug.Parsers, parsers: [:json], json_decoder: Jason, pass: ["*/*"]
  plug :dispatch

  @page 50
  @max 9_223_372_036_854_775_807
  # A home timeline's post ids (as examples/x): each author's newest from
  # their index, then the newest of those.
  @timeline """
  SELECT t.id FROM (SELECT followee FROM follows WHERE follower = $1 UNION ALL SELECT $1) f (a),
  LATERAL (SELECT id FROM posts WHERE author = f.a AND id < $2 ORDER BY id DESC LIMIT 50) t
  ORDER BY t.id DESC LIMIT 50
  """
  @post """
  p.id, u.handle, p.body, (extract(epoch FROM p.created_at) * 1000)::bigint,
  (SELECT count(*) FROM likes l WHERE l.post = p.id),
  EXISTS (SELECT 1 FROM likes l WHERE l.post = p.id AND l.who = $1)
  FROM posts p JOIN users u ON u.id = p.author
  """

  post "/api/users" do
    run(conn, fn ->
      h = conn.params["handle"] || ""
      [[id]] = q("INSERT INTO users (handle) VALUES ($1) RETURNING id", [h])
      {201, %{id: id, handle: h}}
    end)
  end

  get "/api/users/:h" do
    run(conn, fn ->
      case q(
             """
             SELECT (SELECT count(*) FROM posts WHERE author = u.id), (SELECT count(*) FROM follows WHERE followee = u.id),
             (SELECT count(*) FROM follows WHERE follower = u.id) FROM users u WHERE handle = $1
             """,
             [h]
           ) do
        [[p, f, g]] -> {200, %{handle: h, posts: p, followers: f, following: g}}
        [] -> throw({:http, 404, "no such user"})
      end
    end)
  end

  get "/api/users/:h/posts" do
    run(conn, fn ->
      author = user_id(h)
      posts(q("SELECT #{@post} WHERE p.author = $2 AND p.id < $3 ORDER BY p.id DESC LIMIT #{@page}", [viewer(conn), author, before(conn)]))
    end)
  end

  get "/api/timeline" do
    run(conn, fn ->
      me = viewer(conn) || throw({:http, 400, "timeline: whose? (?as=handle)"})

      posts(
        q(
          "SELECT #{@post} WHERE p.id IN (#{@timeline}) ORDER BY p.id DESC",
          [me, before(conn)]
        )
      )
    end)
  end

  post "/api/posts" do
    run(conn, fn ->
      me = viewer(conn) || throw({:http, 400, "post as whom? (\"as\")"})
      text = conn.params["body"] || ""
      [[id, at]] = q("INSERT INTO posts (author, body) VALUES ($1, $2) RETURNING id, (extract(epoch FROM created_at) * 1000)::bigint", [me, text])
      {201, %{id: id, body: text, at: at}}
    end)
  end

  match "/api/users/:h/follow", via: [:post, :delete] do
    run(conn, fn ->
      me = viewer(conn) || throw({:http, 400, "follow as whom? (\"as\")"})
      them = user_id(h)

      if conn.method == "POST",
        do: q("INSERT INTO follows (follower, followee) VALUES ($1, $2) ON CONFLICT DO NOTHING", [me, them]),
        else: q("DELETE FROM follows WHERE follower = $1 AND followee = $2", [me, them])

      {200, %{following: conn.method == "POST"}}
    end)
  end

  match "/api/posts/:id/like", via: [:post, :delete] do
    run(conn, fn ->
      me = viewer(conn) || throw({:http, 400, "like as whom? (\"as\")"})

      id =
        case Integer.parse(id) do
          {n, ""} -> n
          _ -> throw({:http, 404, "no such post"})
        end

      if conn.method == "POST",
        do: q("INSERT INTO likes (post, who) VALUES ($1, $2) ON CONFLICT DO NOTHING", [id, me]),
        else: q("DELETE FROM likes WHERE post = $1 AND who = $2", [id, me])

      [[n]] = q("SELECT count(*) FROM likes WHERE post = $1", [id])
      {200, %{liked: conn.method == "POST", likes: n}}
    end)
  end

  match _ do
    run(conn, fn -> throw({:http, 404, "not found"}) end)
  end

  defp q(sql, params), do: Postgrex.query!(X.DB, sql, params).rows

  defp user_id(handle) do
    case q("SELECT id FROM users WHERE handle = $1", [handle]) do
      [[id]] -> id
      [] -> throw({:http, 404, "no such user"})
    end
  end

  defp viewer(conn), do: conn.params["as"] && user_id(conn.params["as"])

  defp before(conn) do
    case Integer.parse(conn.params["before"] || "") do
      {n, ""} -> n
      _ -> @max
    end
  end

  defp posts(rows) do
    {200, %{posts: Enum.map(rows, fn [id, a, b, at, n, liked] -> %{id: id, author: a, body: b, at: at, likes: n, liked: liked} end)}}
  end

  # A broken rule is the request's fault; anything else the server's, and
  # not explained (as examples/x).
  defp run(conn, fun) do
    {status, body} =
      try do
        fun.()
      catch
        {:http, s, m} -> {s, %{error: m}}
      rescue
        e in Postgrex.Error ->
          case e.postgres && e.postgres.pg_code do
            "23505" -> {409, %{error: "already taken"}}
            "23503" -> {404, %{error: "no such post or user"}}
            c when c in ["23514", "22001"] -> {400, %{error: "not allowed"}}
            _ -> {500, %{error: "database error"}}
          end
      end

    conn |> put_resp_content_type("application/json") |> send_resp(status, Jason.encode!(body))
  end
end
