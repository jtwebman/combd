# Overload and limits

When a node gets more than it can do, it says so instead of falling over.
It keeps serving what it has, and turns the rest away early, where a
client can go somewhere else.

## The memory guard

A node watches two things:

- **Its own memory**, against its budget: `COMBD_MEMORY_BUDGET`, in bytes
  or as `512M` or `4G`, else the container's cgroup v2 limit, else the
  machine's RAM.
- **The whole machine's free memory.** combd's own count misses what the
  kernel holds for its connections, and a node that swaps stops
  answering. A tenth of the machine is kept back.

| Pressure | Own memory | Machine | What the node does |
|---|---|---|---|
| 0 | under 80% | under 85% | Normal. |
| 1 | 80% of the budget | 85% | Reclaims. Idle sessions sleep at once, idle objects go after 10 s, not a minute, and warm cells sooner. |
| 2 | 90% | 95% | Refuses new connections and sockets until back under 85%, or 90% for the machine. |

The machine's percentages are of the machine less its tenth kept back.
The node measures every 64 connections as they pour in, not only once a
second, since a burst outran the once-a-second check.

## Refusing at TCP

A full node stops listening, so the kernel refuses a new connection, and
a browser tries the name's next address by itself. An accepted connection
could only be hung up on. Full means any of these:

- memory pressure 2
- `COMBD_MAX_CONNS` connections open, 200,000 by default
- every event loop behind, as below

Each state holds for at least a second, so it doesn't flap.
`tcp_refusing` in the stats is 1 meanwhile. A listener on 127.0.0.1
stays, and the node never refuses its own machine, so its own checks can
see how full it is. A connection that gets in anyway is closed if TLS,
or gets `503` with `retry-after: 1` if plain HTTP.

On the live fleet, a node at about 29,500 clicking pages refused the rest
at TCP and stayed up. 439 of 441 requests to the name went to another
node by themselves (`combd/RESULTS.md`, "On the fleet").

## At most so many cells at work

`COMBD_MAX_AWAKE`, 1,000 a loop, caps the cells at work at once, running
or waiting on a host call. A sleeping session with work waits its turn
asleep, so not everyone wakes at once. While that line is as long
as the cap, the loop is behind. The node takes no new connections, and
each session's mailbox is 8, not 1,000. `sessions_ready` and
`sessions_waited` in the stats show it working.

Without it, 40,000 users ran a 1 GB node out of memory. Every message to
a sleeping viewer woke it, and once the loop fell behind, more and more
were awake at once. With it, the node held about 30,000, refused the
rest, and lost 0.5% of their clicks (`combd/RESULTS.md`, "Overload,
without Docker").

## Mailboxes and batches

A durable object takes one request at a time. The ones behind it wait in
its mailbox, up to `mailbox`, 1000. Past that, a request is refused at
once with `503 {"error":"overloaded"}`, not queued forever.
`mailbox_refused` counts them.

A module that exports `handle_batch` takes what's waiting at once, up to
`batch`, 100, in one transaction. Batches form only from a
queue, so they grow with the load ([Processes](processes.md#batches)).

## Cold starts

In a fleet, `COMBD_RESTORES_AT_ONCE` caps the objects restored from the
bucket at once, 4 a loop, since each holds its segments in memory. Under
pressure, an idle object goes after 10 s, not 1 s, so it isn't let go
while its neighbors are still coming back.

## Limits per request

What a client may send, node-wide:

| Limit | Default | Past it |
|---|---|---|
| Body (`COMBD_MAX_BODY`) | 1 MiB | 413 |
| Request line and headers (`COMBD_MAX_HEADER_BYTES`) | 16 KiB | 431 |
| Headers (`COMBD_MAX_HEADERS`) | 64 | 431 |
| WebSocket message (`COMBD_MAX_WS_MESSAGE`) | 1 MiB | close 1009 |
| Time to send a request (`COMBD_READ_TIMEOUT_MS`) | 30 s | 408 |
| An idle kept connection (`COMBD_KEEPALIVE_MS`) | 75 s | closed |
| A TLS handshake (`COMBD_TLS_HANDSHAKE_MS`) | 10 s | closed |

A client that reads its WebSocket too slowly is closed once 8 MB is
waiting for it. Bodies come only with a `content-length`. A request with
chunked `Transfer-Encoding` gets a 400.

What a cell may use:

- **CPU.** Cells run a slice at a time and take turns, so a busy cell
  can't hold its loop. One call of a compiled cell is killed after
  `COMBD_CPU_LIMIT_TICKS`, 100,000,000 loop iterations, about 100 ms of
  tight loop.
- **Host time.** A request's host calls, like SQL and sends, may take 1 s
  in all. Past that, they crash the cell.
- **Memory.** `memory_kb` per process, if set.
- **SQL.** At most 100 ms a statement, 2 MB a value, and 1 GB an
  object's database.
- **Output.** stdout and stderr are kept up to 64 KB a request.
- **Sends.** A request may queue 16 MB of WebSocket messages and `send`s
  in all.

## Disk

A node takes no new objects, and answers `507`, when its free disk is
less than a tenth of the data disk or 2 GB, whichever is less.
`disk_low` in the stats is 1 meanwhile.

## What a $6 node does

One 1 vCPU, 1 GB droplet, with combd under systemd and million compiled,
measured 2026-10-22, from the README:

- **About 58,000 idle WebSockets.** Then it turns new ones away and
  stays fast, pings 4 to 20 ms, no swapping.
- **10,000 people clicking at once.** Nothing lost, p50 echo under
  420 ms, 62% CPU. At 20,000, 0.04% of clicks lost.
- **Past about 30,000 active users**, it turns the rest away and keeps
  serving the ones it has.

The full runs, and what they don't cover, are in
[`combd/RESULTS.md`](https://github.com/jtwebman/combd/blob/main/combd/RESULTS.md).
