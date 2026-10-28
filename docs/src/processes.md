# Processes

Every process in an app runs cells, small sandboxed instances of one of
its modules. What differs is how many there are, how long they live, and
what they keep.

## Requests and answers

A cell gets one line of text, and gives back one:

```text
<kind> <METHOD> <path>[ <body>]
```

- `kind` is `api` for an `http` process and `do` for a durable object or a
  WebSocket session.
- `METHOD` is the HTTP method or an event. combd takes `GET`, `HEAD`,
  `POST`, `PUT`, `PATCH`, `DELETE`, `OPTIONS`, and refuses others. The
  events are `ALARM`, `TICK`, `WSOPEN`, `WSMSG`, `WSBIN`, `WSCLOSE`,
  `HIBERNATE`, `WAKE`.
- For an `http` process, `path` is the whole path, with its query string.
  For a durable object, it's what comes after the id in its route, with
  the query string. `/counter/:id/*` and `/counter/demo/incr?n=2` give
  `/incr?n=2`.
- `body` is the request's body, if any.

The cell doesn't see request headers.

The answer is `<status> <body>`, or with headers:

```text
200
content-type: text/html; charset=utf-8
cache-control: no-store

<p>hello</p>
```

combd passes on only these headers: `content-type`, `cache-control`,
`location`, `set-cookie`, `etag`, `last-modified`, `vary`, and
`access-control-*`. [`combd-sdk`](rust-sdk.md) reads and writes all of
this.

## HTTP workers

An `http` process is stateless. Each request takes a warm cell from a pool
on its event loop, runs, and gives the cell back. A burst grows the pool,
which then shrinks to what's been needed lately. A cell that crashes is
dropped, and the next request gets a fresh one.

With `fresh = true`, every request gets a new cell, so nothing one
leaves in memory reaches the next.

### Event cells

An `http` process whose module exports `on_request` is an event cell. One
instance takes many requests at once, like a Node process, and runs them
while they wait on timers, Postgres, or calls. Thirty requests that each
wait 400 ms finish in about 400 ms on one instance.

- `max_concurrent` (default 32) is how many requests one instance takes
  before combd starts another.
- A request may be answered early with `respond`, and work go on after
  it, like `waitUntil`.
- A request gets 30 s to be answered, then a `504`, and 60 s in all, work
  after the answer included.
- The cost: a crash fails every unanswered request on that instance, and
  work after an answer is best effort.

Event cells are an ABI-level feature today. The Rust SDK doesn't wrap
them. See [Other languages](languages.md#event-cells) and `guest-ev/` in
the repo.

## Durable objects

A `durable` process has one object per id. The id comes from the route's
`:id` segment, or from a `call` that names it. Each object:

- runs **one request at a time**, in a warm cell, with its own SQLite
  database.
- makes each request **one transaction**. If the cell crashes from a panic
  or a trap, the request's writes roll back and the cell is dropped. The
  next request gets a fresh one that loads from storage. An answer with an
  error status is still an answer, so its writes commit.
- answers only once its writes are safe. See
  [Storage and durability](storage.md).
- leaves memory when idle, and comes back from storage when needed.

Requests that arrive while an object is busy wait in its mailbox, up to
`mailbox` (default 1000). Past it, new ones get `503
{"error":"overloaded"}` at once. A busy object sheds load instead of
piling it up.

### Batches

A module that exports `handle_batch` gets the requests waiting behind the
first one all at once, up to `batch` (default 100). That is one run, one
transaction, and an answer each. Each request is framed as its length, a
newline, then itself. The answers come back framed the same way. Batches
form only from a queue, so they grow with the load. WebSocket events,
alarms, and ticks always go one at a time.

### Alarms

An object can set one alarm, a time in ms since the epoch. Setting it
changes the object's storage, so it commits with the request. A request
that crashes doesn't set it. When it's due, the object gets an `ALARM`
request, `do ALARM / <try>`. The alarm is cleared as it fires, and the
handler can set the next one.

An alarm whose handler crashes is tried again after 500 ms, then twice as
long each time, up to 6 tries. Then it's dropped. In a fleet, an index in
the bucket finds due alarms of objects that aren't running anywhere. The
fleet fires them as ordinary requests, routed to the object's owner. A
fire from the index that keeps failing backs off, up to a minute between
tries.

### Ticks

`tick_every(ms)` gives an object a beat, a `TICK` request every `ms`, from
10 ms to 60 s. 0 stops it. It's in memory only. A crash, or a move to
another node, stops it until the object asks again. It's for game loops.
Use an alarm for work that must happen.

### WebSockets on an object

A route to a durable object can take a WebSocket handshake. The object
gets `WSOPEN`, with the path and the new socket's id, and accepts it by
answering `101`. Then:

| Event | The request |
|---|---|
| a text message | `do WSMSG /<socket> <text>` |
| a binary message | `do WSBIN /<socket> <length>`, the bytes by `ws_msg` |
| the socket closed | `do WSCLOSE /<socket> <code>` |

The object holds many sockets, like a chat room. `ws_list` gives their
ids, `ws_send` and `ws_send_binary` send to one, and `ws_close` closes
one. What a request sends goes out only if the request succeeds, and only
once its writes are safe. The object's sockets are kept in its storage.
When it goes quiet and leaves memory, its sockets stay open, and the next
message wakes it. Pings never wake it.

The Rust SDK doesn't wrap the WebSocket calls yet. See
[Other languages](languages.md#the-host-abi).

## WebSocket sessions

A `ws` process has one session per WebSocket. It starts when a socket
connects on its route, lives in memory on the event loop that holds the
socket, and ends when the socket does. It has no storage.

It gets the same events: `WSOPEN`, `WSMSG`, `WSBIN`, `WSCLOSE`. Answering
`WSOPEN` with `101` accepts it. A crash ends the session and
closes its socket.

**Sleeping.** A session idle for `hibernate_ms` (default 5 s) sleeps:

- `hibernate = "auto"`, the default. The host keeps what its cell
  changed, and wakes the same cell where it was.
- `hibernate = "app"`. The session gets `do HIBERNATE /` and answers
  `200 <state>`, at most `state_kb`. Its cell goes. The next event first
  brings `do WAKE / <length>`, with the state by `ws_msg`.
- `hibernate = "off"`. It stays awake.

With `on_close = "drop"`, a closed socket's session ends without
waking. That's cheap when everyone leaves at once, as in a deploy or a
network blip.

**Reaching a session.** Another process reaches a session with `call` or
`send`, naming its socket id as `"id"`. With `"ws": "<text>"` instead of
a request, the text goes straight to the session's socket without waking
it. `"ids": [...]` sends one text to many sessions' sockets at once.
That's how One Million Checkboxes' chunks tell their watchers about a
change ([`examples/million`](https://github.com/jtwebman/combd/tree/main/examples/million)).

## Calls between processes

A cell can ask another process of its app and wait, with `call`, or tell
it and go on, with `send`. The message is JSON:

```json
{"to": "player", "id": "ann", "method": "POST", "path": "/won", "body": "{\"by\":3}"}
```

| Field | What it is |
|---|---|
| `to` | The process. It must list the caller in its `callers`. |
| `id` | The object's id (durable), or the session's socket id (`ws`). |
| `method` | Default `POST`. |
| `path` | Default `/`. |
| `body` | Text. |
| `timeout_ms` | How long to wait, at most the target's own `timeout_ms`. |
| `node` | For a durable object in a fleet, start it on that node, or reach it there. |
| `ws`, `ids` | For a `ws` process, text straight to sockets. |

`call` gets back `{"status", "body"}`, or `{"error"}` if denied or timed
out. A waiting cell is paused and costs no CPU.

`send` goes out only if the request that sends it succeeds. For an
object, that's once its writes are safe. It's refused when too much is
queued, past 1 MB a message or 16 MB a request in all. A process may list
itself in its own `callers`, to send itself a message after it answers.
