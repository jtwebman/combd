# The Rust SDK

`combd-sdk` is a small crate for writing a combd app in Rust: a request
in, a response out, plus SQLite, Postgres, alarms, the clock,
env vars, and calls between processes. It lives in the repo at
`sdk/rust/`. `combd new` puts it in your `Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
combd-sdk = { git = "https://github.com/jtwebman/combd" }

[profile.release]
opt-level = "s"
lto = true
panic = "abort"
strip = true
```

Build for `wasm32-unknown-unknown`, added with `rustup target add
wasm32-unknown-unknown`. `combd build` runs `cargo build --release
--target wasm32-unknown-unknown` and copies the result to
`modules/<package>.wasm`, where `module = "<package>"` finds it.

## A handler

```rust
use combd_sdk::{cell, json, sql, Request, Response};

fn handle(req: Request) -> Response {
    match (req.kind, req.method, req.route()) {
        ("api", "GET", "/") => Response::html("<h1>hi</h1>"),
        ("do", "POST", "/incr") => {
            sql::run("CREATE TABLE IF NOT EXISTS n (v INTEGER)", &[]).unwrap();
            sql::run("INSERT INTO n VALUES (1)", &[]).unwrap();
            let rows = sql::run("SELECT count(*) FROM n", &[]).unwrap();
            Response::json(200, &json!({ "count": rows[0][0] }))
        }
        _ => Response::text(404, "not found"),
    }
}

cell!(handle);
```

`cell!(handle)` wraps your function in the exports the host calls,
`alloc`, `release`, and `handle`. Any name works. It also sends a panic's
message to stderr, which combd logs beside the crash. With
`panic = "abort"`, a cell would otherwise die without a word.

One module can serve many processes. Match on `req.kind`, or read
`COMBD_PROCESS` with `env`.

## `Request`

| Field or method | What it is |
|---|---|
| `kind` | `"api"` (an `http` process) or `"do"` (a durable object or a WebSocket session). |
| `method` | `"GET"`, `"POST"`, ..., or an event: `"ALARM"`, `"TICK"`, `"WSOPEN"`, `"WSMSG"`, `"WSCLOSE"`, ... |
| `path` | The path, with its query string, if any. |
| `body` | The body, as text. |
| `route()` | The path without its query string. |
| `query(name)` | A query parameter, as written, not decoded. |
| `json()` | The body as JSON, or `Value::Null` if it isn't. |

See [Processes](processes.md#requests-and-answers) for what each kind of
process gets.

## `Response`

```rust
Response::text(404, "not found")          // text/plain; charset=utf-8
Response::json(200, &json!({ "ok": true }))
Response::html("<p>hi</p>")               // 200, text/html; charset=utf-8
Response::json(201, &v).header("location", "/posts/7")
```

The host passes on only `content-type`, `cache-control`, `location`,
`set-cookie`, `etag`, `last-modified`, `vary`, and `access-control-*`.

## `sql`

The durable object's own SQLite. Each request is one transaction.

```rust
let rows = sql::run("SELECT title FROM posts WHERE id = ?", &[json!(7)]); // Result<Vec<Vec<Value>>, String>
```

`run` takes one statement, with `?` for each parameter, and returns the
rows (`Vec<Vec<Value>>`) or the error. Only durable objects have storage.
See [Storage and durability](storage.md).

## `pg`

A statement on a pooled Postgres connection, with `$1`, `$2`... for
parameters:

```rust
let rows = pg::query("main", "SELECT body FROM posts WHERE author = $1", &[json!("ann")]);
```

`"main"` is the pool's name in the manifest, and this process must be in
its `callers`. An error starts with its SQLSTATE. Each statement is its
own transaction, and its connection isn't reset after it. See
[Postgres pools](postgres.md).

## `alarm`

```rust
alarm::set(combd_sdk::now_ms() + 60_000);   // in a minute
alarm::get();                               // Some(ms), or None
alarm::clear();
```

One alarm per object, in ms since the epoch. It commits with the request,
and fires as an `ALARM` request.

## `call` and `send`

```rust
let answer = combd_sdk::call(&json!({ "to": "player", "id": "ann", "path": "/stats" }));
// {"status": 200, "body": "..."} or {"error": "..."}

combd_sdk::send(&json!({ "to": "player", "id": "ann", "path": "/won", "body": "{}" }));
```

`call` waits for the answer. `send` doesn't, and goes out only if this
request succeeds. It returns `false` if too much is queued. The target
must list this process in its `callers`. The fields are in
[Processes](processes.md#calls-between-processes).

## `env` and `now_ms`

```rust
let level = combd_sdk::env("LOG_LEVEL").unwrap_or("info".into());
let me = combd_sdk::env("COMBD_PROCESS");
let now = combd_sdk::now_ms();   // ms since the epoch
```

`env` reads the app's vars and secrets, and the host's `COMBD_ORG`,
`COMBD_APP`, `COMBD_PROCESS`, and `COMBD_NODE`. The clock is coarse on
purpose, to 1 ms.

`json!`, `Value`, and `to_json` come from `serde_json`.

## Not in the SDK yet

These work at the host ABI level, covered in
[Other languages](languages.md#the-host-abi), and the repo's examples use
them directly:

- WebSockets: `ws_send`, `ws_send_binary`, `ws_close`, `ws_list`,
  `ws_msg`
- `tick_every`
- Postgres sessions: `pg_open`, `pg_query` with a handle, `pg_close`
- event cells: `on_request`, `respond`, ...
- `handle_batch`
