# Postgres pools

Cells reach Postgres through a pool the host runs. A cell never holds a
socket or a password. The node keeps the connections, and the URL lives in
a secret. A cell waiting on the database is paused and costs no CPU.

## The manifest

A pool is a process of type `pool`, with `module = "combd:postgres"`:

```toml
[secrets]
PG_URL = { secret = "pg-url", type = "url" }

[vars]
POOL_MAX = { type = "int", min = 1, default = 8 }

[http.api]
module = "x"
routes = ["/api/*"]

[pool.main]
module = "combd:postgres"
url = "$PG_URL"
max = "$POOL_MAX"
callers = ["api"]
```

Code names the pool `main`. Only processes in the required
`callers` list may use it. A pool has no routes.

| Setting | Default | What it does |
|---|---|---|
| `url` | required | The database URL, of type `url`, usually `$SECRET`. Add `sslmode=require` for TLS. |
| `max` | 8 (1 to 10000) | The most connections, for the whole fleet. |
| `wait_ms` | 5000 (1 to 600000) | At `max`, how long an ask waits in line before it fails. |
| `lease_ms` | 60000 (1 to 86400000) | The longest a session may hold a connection before it's taken back. |
| `idle_ms` | 60000 (0 to 86400000) | A connection free this long closes. |
| `session_idle_ms` | 60000 (1 to 86400000) | An object's session unused this long is released. |
| `statement_timeout_ms` | 5000 (1 to 3600000) | Every statement's timeout, lock waits included. |
| `connection.timeout_ms` | 5000 (1 to 600000) | How long to connect. |
| `connection.retries` | 3 (0 to 10) | Connect attempts after the first, with backoff. |
| `on_manager_loss` | `"finish"` | When the pool's manager changes, older connections `finish` their lease or `stop` now. |

## Pooled statements

A pooled statement runs on whichever connection is free, as its own
transaction. In Rust:

```rust
use combd_sdk::{json, pg, Response};

match pg::query("main", "SELECT id, body FROM posts WHERE author = $1", &[json!("ann")]) {
    Ok(rows) => Response::json(200, &json!(rows)),
    Err(e) => Response::text(500, &e),
}
```

Parameters are `$1`, `$2`, .... The answer is the same JSON as an
object's own SQLite, `{"rows": [[...]], "changes": n}`, or `{"error":
"...", "code": "<SQLSTATE>"}`. In Rust an error is the SQLSTATE, then the
message, like `23505 duplicate key value ...`.

A pooled statement's connection is not reset after it, as with
PgBouncer's transaction mode and the pools apps keep themselves, like
Postgrex, Rails, and Prisma. What a statement changes in the session, like
a `SET`, a session advisory lock, or a temp table, stays for whichever
statement uses that connection next. Undo it in the app, or use a session.

## Sessions

A session leases one connection, so everything session-scoped
works on it: transactions over many statements, row locks, advisory locks,
`SET`. The host calls are `pg_open` for a handle, `pg_query` on it, and
`pg_close`. The Rust SDK doesn't wrap them yet. See
[Other languages](languages.md#the-host-abi).

- An `http` request's sessions end with the request.
- A durable object's sessions last across its requests, until it closes
  them, it crashes, it leaves memory, or a session sits unused for
  `session_idle_ms`. A durable object can hold a transaction open across
  many messages.
- A WebSocket session (`ws`) can't open one. It uses pooled statements.
- An event cell uses `pg_open_async` and friends.

When a session is closed, crashes, or passes `lease_ms`, its connection
is reset with a rollback, then what `DISCARD ALL` does. Every lock is
released. The holder's next call after `lease_ms` gets an error.

## How the pool behaves

- It starts a connection when it's asked and none is free, up to `max`.
- At `max`, an ask waits in line, and fails after `wait_ms`. A session
  waiting for a whole connection goes ahead of pooled statements.
- A connection free for `idle_ms` closes.
- "Too many clients" from the database makes an ask wait, not fail.

## One pool for the fleet

A pool is one for the whole fleet, not one per node. Run 1000 nodes
against a Postgres that takes 100 connections, set `max = 100`, and it
never sees more than 100.

- Every node takes part in every pool. One node is the pool's manager,
  which keeps the books. The catalog names it, and a new one takes over
  if its node goes quiet.
- A connection starts on the node of the cell that asked. At `max`, a
  free connection on the nearest node is leased instead, and its
  statements go over the wire.
- Once a second, each node tells the manager what its connections are
  doing, so a lost message can't leak one.
- When the manager changes, older connections finish their lease or stop,
  as `on_manager_loss` says. Until they're gone, the database may see more
  than `max` for a moment.

## TLS

With `sslmode=require` in the URL, a connection is TLS or nothing. The
server's certificate is checked against the public roots. libpq's
`require` doesn't check that. A database with a
private CA can't be checked yet. Without `sslmode=require`, connections are plain.

## Example

[`examples/x`](https://github.com/jtwebman/combd/tree/main/examples/x) is
an X clone on Postgres: users, posts, follows, likes, a home timeline. It
has one `http` process and one pool.
