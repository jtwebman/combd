// The X clone (examples/x) as a Workers app: the same routes, SQL, and
// answers. Postgres through postgres.js over `cloudflare:sockets`. A
// socket can't outlive its request on Workers (or celld), so each request
// opens a connection and closes it after. On Cloudflare that connection is
// to Hyperdrive, a pooler beside the database (`env.HYPERDRIVE.connectionString`);
// here it's PgBouncer in transaction mode in its place, inside the
// Worker's own CPU and memory, as combd's and Postgrex's pools are inside
// theirs (DATABASE_URL), or Postgres itself, unpooled. The tables are
// examples/x/schema.sql (the race seeds them first).
import postgres from "postgres";

const PAGE = 50;
const MAX = 9223372036854775807n;
// A home timeline's post ids (as examples/x): each author's newest from
// their index, then the newest of those.
const TIMELINE = `SELECT t.id FROM (SELECT followee FROM follows WHERE follower = $1 UNION ALL SELECT $1) f (a),
  LATERAL (SELECT id FROM posts WHERE author = f.a AND id < $2 ORDER BY id DESC LIMIT 50) t
  ORDER BY t.id DESC LIMIT 50`;
const POST = `p.id, u.handle, p.body, (extract(epoch FROM p.created_at) * 1000)::bigint,
  (SELECT count(*) FROM likes l WHERE l.post = p.id),
  EXISTS (SELECT 1 FROM likes l WHERE l.post = p.id AND l.who = $1)
  FROM posts p JOIN users u ON u.id = p.author`;

class Http {
  constructor(status, error) {
    this.status = status;
    this.error = error;
  }
}

export default {
  async fetch(request, env, ctx) {
    // int8 (ids, counts) as BigInt, so an id goes back to Postgres exactly;
    // JSON gets numbers.
    // Statements are prepared, as combd's pool and Postgrex do: PgBouncer
    // (1.21+, max_prepared_statements) carries them across its connections.
    const sql = postgres(env.DATABASE_URL, { max: 1, fetch_types: false, types: { bigint: postgres.BigInt } });
    let status, body;
    try {
      [status, body] = await route(request, sql);
    } catch (e) {
      if (e instanceof Http) [status, body] = [e.status, { error: e.error }];
      else
        [status, body] = {
          23505: [409, { error: "already taken" }],
          23503: [404, { error: "no such post or user" }],
          23514: [400, { error: "not allowed" }],
          22001: [400, { error: "not allowed" }],
        }[e.code] || [500, { error: "database error" }];
    }
    ctx.waitUntil(sql.end({ timeout: 1 }));
    return new Response(JSON.stringify(body, (_, v) => (typeof v === "bigint" ? Number(v) : v)), {
      status,
      headers: { "content-type": "application/json" },
    });
  },
};

async function route(request, sql) {
  const url = new URL(request.url);
  const parts = url.pathname.replace(/\/$/, "").split("/").slice(2);
  const m = request.method;
  const body = m === "POST" || m === "DELETE" ? await request.json().catch(() => ({})) : {};
  const q = (text, params = []) => sql.unsafe(text, params).values();
  const userId = async (h) => {
    const rows = await q("SELECT id FROM users WHERE handle = $1", [h]);
    if (!rows.length) throw new Http(404, "no such user");
    return rows[0][0];
  };
  const as = url.searchParams.get("as") ?? body.as;
  const viewer = async () => (as == null ? null : userId(as));
  const need = async (what) => (await viewer()) ?? Promise.reject(new Http(400, `${what} as whom? ("as")`));
  const b = url.searchParams.get("before");
  const before = b && /^\d+$/.test(b) ? BigInt(b) : MAX;
  const posts = (rows) => [200, { posts: rows.map(([id, author, body, at, likes, liked]) => ({ id, author, body, at, likes, liked })) }];
  const [a, h, c] = parts;

  if (m === "POST" && a === "users" && parts.length === 1) {
    const handle = body.handle ?? "";
    const [[id]] = await q("INSERT INTO users (handle) VALUES ($1) RETURNING id", [handle]);
    return [201, { id, handle }];
  }
  if (m === "GET" && a === "users" && parts.length === 2) {
    const rows = await q(
      `SELECT (SELECT count(*) FROM posts WHERE author = u.id), (SELECT count(*) FROM follows WHERE followee = u.id),
       (SELECT count(*) FROM follows WHERE follower = u.id) FROM users u WHERE handle = $1`,
      [h],
    );
    if (!rows.length) throw new Http(404, "no such user");
    const [p, f, g] = rows[0];
    return [200, { handle: h, posts: p, followers: f, following: g }];
  }
  if (m === "GET" && a === "users" && c === "posts" && parts.length === 3) {
    const author = await userId(h);
    return posts(await q(`SELECT ${POST} WHERE p.author = $2 AND p.id < $3 ORDER BY p.id DESC LIMIT ${PAGE}`, [await viewer(), author, before]));
  }
  if (m === "GET" && a === "timeline" && parts.length === 1) {
    const me = await viewer();
    if (me == null) throw new Http(400, "timeline: whose? (?as=handle)");
    return posts(
      await q(`SELECT ${POST} WHERE p.id IN (${TIMELINE}) ORDER BY p.id DESC`, [me, before]),
    );
  }
  if (m === "POST" && a === "posts" && parts.length === 1) {
    const me = await need("post");
    const text = body.body ?? "";
    const [[id, at]] = await q("INSERT INTO posts (author, body) VALUES ($1, $2) RETURNING id, (extract(epoch FROM created_at) * 1000)::bigint", [me, text]);
    return [201, { id, body: text, at }];
  }
  if ((m === "POST" || m === "DELETE") && a === "users" && c === "follow" && parts.length === 3) {
    const me = await need("follow");
    const them = await userId(h);
    if (m === "POST") await q("INSERT INTO follows (follower, followee) VALUES ($1, $2) ON CONFLICT DO NOTHING", [me, them]);
    else await q("DELETE FROM follows WHERE follower = $1 AND followee = $2", [me, them]);
    return [200, { following: m === "POST" }];
  }
  if ((m === "POST" || m === "DELETE") && a === "posts" && c === "like" && parts.length === 3) {
    const me = await need("like");
    if (!/^\d+$/.test(h)) throw new Http(404, "no such post");
    const id = BigInt(h);
    if (m === "POST") await q("INSERT INTO likes (post, who) VALUES ($1, $2) ON CONFLICT DO NOTHING", [id, me]);
    else await q("DELETE FROM likes WHERE post = $1 AND who = $2", [id, me]);
    const [[n]] = await q("SELECT count(*) FROM likes WHERE post = $1", [id]);
    return [200, { liked: m === "POST", likes: n }];
  }
  throw new Http(404, "not found");
}
