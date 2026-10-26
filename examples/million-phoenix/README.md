# One Million Checkboxes, on the BEAM

[million](../million/) in Elixir, for the click race (`bench/click`). Same
page (`../million/client/index.html`), same protocol, same limits: 64
chunks of 15,625 boxes, `range` and `set` in, `all`, `set`, `sets` and
`slow` out, at most 4 chunks watched and 20 sets a second per socket,
changes sent on a 150 ms beat and only once they're on disk,
subscriptions renewed every 20 s and forgotten after 60. So `ws-click`
drives it unchanged.

It runs on Bandit, Plug and WebSock: the HTTP server, interface and
WebSocket layer a Phoenix app runs on. There's no Phoenix router or
Channel: Channels speak their own envelope (`[join_ref, ref, topic,
event, payload]`), not million's protocol, and one raw socket at `/ws`
needs no router. A Phoenix endpoint upgrading the same socket from a
controller (`WebSockAdapter.upgrade`) would run this same handler in the
same Bandit process.

| million on combd | Here |
|---|---|
| `chunk`, durable | `Million.Chunk`, a GenServer per chunk, with its own SQLite file (exqlite) |
| `boxes`, durable | `Million.Boxes`, one GenServer, with SQLite |
| `web`, http | `Million.Router` (Plug) |
| `viewer`, a session per socket | `Million.Socket`: Bandit's process per connection, running this WebSock handler |
| a chunk sends a change straight to its watchers' sockets | a chunk sends each beat's message, encoded once, to every watcher's process (the same binary, shared, not copied); the process writes it out |

The fan-out is Phoenix's "fastlane" idea without PubSub: a chunk holds
its watchers' pids (monitored, so a page that leaves is forgotten at
once, and expiring 60 s after their last renewal as million's do) and
sends to them directly; no process in between decodes or re-encodes.

**Persistence matches million's**: each chunk's checked boxes in SQLite
(`on_ (i INTEGER PRIMARY KEY)`, one file per chunk, WAL), each beat's
changes in one transaction with `synchronous = FULL` (an fsync a commit),
and the beat's message sent only after the commit, as million's host
holds a beat's sends until its write is on disk. The count goes to
`Boxes` at most once a second per chunk.

## Settings, and why

In `rel/vm.args.eex`, the Dockerfile, and the code:
- `+S 1:1`: one scheduler, for one CPU (the race's box).
- `+sbwt none` (and for the dirty schedulers): no busy-waiting when idle;
  spinning schedulers spend the CPU quota.
- `+P 262144`, `+Q 262144`: processes (one per socket) and ports
  (sockets) sized to the box. Both tables are allocated whole at boot;
  the defaults are 1,048,576 processes (too many: 16 MB more with
  `+P 2000000`) and 65,536 ports (too few for the idle test).
- `RELEASE_MODE=interactive`: modules load as first used, not all at
  boot: 26 MB less memory at start.
- `RELEASE_DISTRIBUTION=none`: one node; no epmd, no distribution.
- Each socket's receive buffer (`transport_options: [buffer: 2048]`): the
  default is 9 KB a socket (sized from the kernel's buffers); million's
  pages send ~40-byte messages.
- `fullsweep_after: 0` on the socket processes, and a collection after
  each `range` (every 20 s a page): Bandit can't hibernate a socket's
  process (Phoenix's channel processes hibernate after 15 s idle), so
  this keeps an idle page's heap at what it keeps.
- No compression (`compress: false`); a 120 s socket timeout (pages
  renew every 20 s).
- ThousandIsland's connection cap (100 acceptors × 16,384) is left alone:
  past what a GB holds.

What they did, with 5,000 idle pages: the BEAM's own memory went from 294
MB (defaults, `+P 2000000 +Q 1100000`) to 114 MB with the buffer, heap
and table settings, and 26 MB less again with interactive mode. In the
race's idle ladder: 22,000 pages (37 KB a page) before, 68,000 (14.5 KB)
after, in both runs.

## Run it

```sh
cp ../million/client/index.html priv/index.html
mix deps.get && MIX_ENV=prod mix release && _build/prod/rel/million/bin/million start
```

Then open http://localhost:8080. `../../bench/click/Dockerfile` builds it
in `elixir:1.20.4-otp-28` (Elixir 1.20.4, OTP 28), with the versions in
`mix.lock` (Bandit 1.13.1, ThousandIsland 1.6.0, WebSock 0.5.3,
WebSockAdapter 0.6.0, Plug 1.20.3, exqlite 0.43.0). `MILLION_DATA` is where
the SQLite files go (`/tmp/data`), `PORT` the port (8080).
