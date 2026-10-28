# Other languages

A combd module is a WebAssembly module that speaks the host's small ABI.
Rust has an SDK, [`combd-sdk`](rust-sdk.md). Any other language that
compiles to Wasm can write a module against the ABI below.

## What's been run

| Language | Toolchain | Status |
|---|---|---|
| Rust | `wasm32-unknown-unknown` | Apps, with `combd-sdk`. |
| C | `zig cc -target wasm32-wasi -mexec-model=reactor` | The test cell. |
| Zig | `zig build-exe -target wasm32-wasi -fno-entry -rdynamic` | The test cell. |
| Go | TinyGo, `-target=wasip1 -buildmode=c-shared` | The test cell. |
| AssemblyScript | `asc --exportStart _initialize` | The test cell. |
| JavaScript | | Not yet. A Workers-compatible library over combd's host, compiled to Wasm, is next. |

[`langs/`](https://github.com/jtwebman/combd/tree/main/langs) has the
same small cell in each language, built in Docker with
`docker build -f langs/Dockerfile -o langs/out langs`. Each exports
`alloc` and `handle`, calls one host function, prints, spins forever, and
panics. `cargo run --release -- langs`, in `combd/`, checks each on
combd's engines.

## The host ABI

### What a module exports

| Export | Signature | What it does |
|---|---|---|
| `alloc` | `(len: u32) -> ptr` | Memory for a request of `len` bytes. The host writes the request there. |
| `handle` | `(ptr, len: u32) -> u64` | Handle one request. Returns the answer as `(ptr << 32) \| len`. |
| `release` | `(ptr, len: u32)` | Optional. Frees an answer once the host has read it. |
| `handle_batch` | `(ptr, len: u32) -> u64` | Optional, for durable objects. Many requests at once. |
| `_initialize` | `()` | Optional. A reactor's start, run once first. |
| `on_request`, `on_event` | | An event cell, in place of `handle`. |

The request is the line `<kind> <METHOD> <path>[ <body>]`. The answer is
`<status> <body>`, or `<status>\n<name>: <value>\n...\n\n<body>` with
headers. See [Processes](processes.md#requests-and-answers).

`handle_batch` gets each request as its length in decimal, a newline,
then the request, one after another. It answers each the same way, in
order, all in one transaction.

### What the host gives it

Imports from the `combd` module. A call that waits, like Postgres or
`call`, pauses the cell and costs no CPU meanwhile.

| Import | What it does |
|---|---|
| `sql(ptr, len) -> u32` | Run one statement, as `{"q", "p"}` JSON, on the object's SQLite. Returns the answer's length. |
| `sql_read(ptr)` | Copy that answer into the cell. |
| `now_ms() -> i64` | The wall clock, in ms since the epoch. |
| `alarm_set(at_ms: i64)` | Set the object's alarm. 0 or less clears it. |
| `alarm_get() -> i64` | The alarm, or -1 for none. |
| `tick_every(ms: u64)` | A `TICK` every `ms`, 10 to 60000. 0 stops it. |
| `ws_send(id: u64, ptr, len) -> u32` | Queue a text message to a socket. 0 OK, 1 no such socket, 2 too big or not UTF-8. |
| `ws_send_binary(id: u64, ptr, len) -> u32` | The same, for bytes. |
| `ws_close(id: u64, code) -> u32` | Close a socket with code 1000, or 3000 to 4999. 0 OK, 1 no such socket. |
| `ws_list(ptr, cap) -> u32` | Write the object's socket ids, at most `cap`, as little-endian u64s. Returns how many there are. |
| `ws_msg(ptr)` | Copy this event's bytes in, a `WSBIN` message or a `WAKE`'s state. |
| `call(ptr, len) -> u32` | Ask another process, in JSON, and wait. Returns the answer's length. `pg_read` copies it. |
| `send(ptr, len) -> u32` | Tell another process, if this request succeeds. 1 queued, 0 too much queued. |
| `pg_open(ptr, len) -> u64` | Lease a Postgres connection with `{"db": "<pool>"}`. Returns a handle, or 0 with why in `pg_read`. |
| `pg_query(handle: u64, ptr, len) -> u32` | One statement, `{"db", "q", "p"}`, on a handle, or pooled with handle 0. Returns the answer's length. |
| `pg_close(handle: u64) -> u32` | Done with a handle. It rolls back, releases its locks, and goes back. |
| `pg_read(ptr, cap) -> u32` | Copy the last answer of `pg_*` or `call`, at most `cap` bytes. Returns how many. |

`env.abort` crashes the cell, as does a trap.

A small slice of WASI preview 1, from `wasi_snapshot_preview1`:

- `fd_write` to stdout and stderr only, into a 64 KB buffer that combd
  logs when a cell crashes.
- `fd_read`. Stdin is always empty.
- `clock_time_get` and `clock_res_get`, rounded to 1 ms on purpose.
- `random_get`.
- `environ_sizes_get` and `environ_get`, with the app's vars and secrets,
  and `COMBD_ORG`, `COMBD_APP`, `COMBD_PROCESS`, `COMBD_NODE`.
- `args_sizes_get`, with no args, and `sched_yield`.
- `proc_exit`, which crashes the cell.

Anything else answers "not supported", or "not capable" for files. There
are no files, no sockets, and no sleeping.

### Event cells

An `http` process whose module exports `on_request` is an event cell. One
instance takes many requests at once.

- `on_request(id: u64, ptr, len)` is a new request, its text at `ptr`
  from `alloc`.
- `respond(req: u64, ptr, len) -> u32` answers request `req`, now or
  later. Work may go on after it.
- `timer_async(req, ms)`, `pg_open_async(req, ptr, len)`,
  `pg_query_async(req, handle, ptr, len)`, and `call_async(req, ptr, len)`
  each return a ticket at once, 0 for an unknown request. Timers max out
  at 30 s.
- `on_event(ticket: u64, len)` says a ticket's wait is done.
  `ev_read(ptr, cap)` reads its data.
- `pg_close_async(req, handle)` and `send_async(req, ptr, len)` don't wait.

[`guest-ev/`](https://github.com/jtwebman/combd/tree/main/guest-ev) is a
whole one in Rust.

## Hand-written modules in the repo

The examples use the ABI directly where the SDK doesn't reach yet:

- [`examples/million`](https://github.com/jtwebman/combd/tree/main/examples/million),
  a `ws` process with `hibernate = "app"`, `handle_batch`, `tick_every`
- [`examples/arena`](https://github.com/jtwebman/combd/tree/main/examples/arena),
  WebSockets on durable objects, ticks, `call` with `"node"`
