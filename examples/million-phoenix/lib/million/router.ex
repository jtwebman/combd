defmodule Million.Router do
  @moduledoc "The page, the count of checked boxes, and the socket."
  use Plug.Router

  plug :match
  plug :dispatch

  get "/" do
    conn
    |> put_resp_header("content-type", "text/html; charset=utf-8")
    |> put_resp_header("cache-control", "no-cache")
    |> send_resp(200, :persistent_term.get(:million_index))
  end

  get "/api/count" do
    conn
    |> put_resp_header("content-type", "application/json")
    |> send_resp(200, ~s({"checked":#{Million.Boxes.total()}}))
  end

  # No compression (million's page doesn't need it, and a deflate context
  # is hundreds of KB a socket). Pages renew every 20 s, so a socket quiet
  # for two minutes is gone. Every collection a full one (no old heap
  # kept beside the young: a socket's process stays small).
  get "/ws" do
    conn
    |> WebSockAdapter.upgrade(Million.Socket, nil, compress: false, timeout: 120_000, fullsweep_after: 0)
    |> halt()
  end

  match _ do
    conn
    |> put_resp_header("content-type", "application/json")
    |> send_resp(404, ~s({"error":"not found"}))
  end
end
