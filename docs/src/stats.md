# Stats and monitoring

## `/_combd/stats`

Every node answers `GET /_combd/stats` on its main port with what it's
doing, as JSON. It isn't behind a token, so anyone who can reach the port
can read it. Counters count up from the node's start. The rest are now.

```sh
curl -s https://n0.arena.combd.dev/_combd/stats
```

**Objects**

| Field | What it is |
|---|---|
| `objects_live` | Durable objects in memory now. |
| `objects_opened` | Objects started or brought back from storage. |
| `evictions` | Objects that left memory, idle or squeezed. |
| `alarms_fired`, `alarms_failed` | Alarms that ran, and ones whose handler crashed. |
| `forwarded` | Requests sent to another loop or node that owns their object. |
| `mailbox_refused` | Requests refused because a mailbox was full, `503 overloaded`. |
| `batched` | Requests that ran in a `handle_batch` batch. |

**Writes (D1)**

| Field | What it is |
|---|---|
| `commits` | Commits that wrote something. |
| `fsyncs` | The fsyncs that made them durable. Far fewer than `commits` means the group commit is working. |
| `checkpoints` | WAL checkpoints. |
| `fsyncs_slow`, `fsync_max_us` | fsyncs of 10 ms or more, and the slowest, in µs. |

**A fleet**

| Field | What it is |
|---|---|
| `peer_acks` | Writes acknowledged by peers, for D3. |
| `bucket_acks` | Writes acknowledged by the bucket, for D2. |
| `uploads` | Segments sent to the bucket, retries too. |
| `fences` | Takeovers that fenced an older owner. |
| `fence_waits` | Fences told to wait out a peer's promise, when a read lease held off a takeover. |
| `tail_entries` | Entries recovered from peers in takeovers. |
| `lease_reads` | Reads answered under a read lease, with no round. |
| `gc_deleted` | Bucket keys deleted by compaction. |
| `moves` | Objects handed to another node by placement. |

**WebSockets and sessions**

| Field | What it is |
|---|---|
| `sockets_open` | WebSockets open now. |
| `ws_messages` | Messages in from them. |
| `ws_frames`, `ws_flushes` | Frames sent, and the writes that sent them. A socket's frames can share one write. |
| `ws_handoffs` | Handshakes one loop took and handed to the loop of their object. |
| `sessions_live` | `ws` sessions now. |
| `sessions_hibernated` | Of those, asleep. |
| `sessions_woken` | Times a session woke. |
| `sessions_ready` | Sleeping sessions with work, waiting for room under `COMBD_MAX_AWAKE`. |
| `sessions_waited` | Times one had to wait. |

**Connections and memory**

| Field | What it is |
|---|---|
| `conns_open` | Connections open now. |
| `conns_refused` | Connections accepted, then refused for being full or over `COMBD_MAX_CONNS`. |
| `tcp_refusing` | 1 while the node is full and not listening. |
| `accept_failed` | Connections `accept` failed on, out of files. |
| `memory_used` | The node's memory, by its own count. With a cgroup, what the cgroup is charged, less file cache it would give back. |
| `memory_budget` | Its budget, `COMBD_MEMORY_BUDGET` or what it found. |
| `machine_available` | What the machine can still hand out without swapping. |
| `heap_in_use` | Bytes the allocator has handed out. glibc only, 0 elsewhere. |
| `pressure` | 0 fine, 1 reclaiming, 2 refusing. See [Overload](overload.md). |
| `disk_low` | 1 while the data disk is too full for new objects. |

**Event cells, and the rest**

| Field | What it is |
|---|---|
| `ev_instances`, `ev_requests` | Event-cell instances started, and the requests they took. |
| `procs` | Per `org/app/process`: `cpu_us`, its cells' run time with host calls, `slices`, and `engine`, `compiled` or `interpreted`. |
| `node` | This node's number in a fleet, `null` on one node. |
| `version` | The combd version, `dev` for a build with none set. |

Watch these first: `pressure`, `tcp_refusing`, `sessions_ready`,
`mailbox_refused`, `conns_refused`, and `disk_low`. Any of them above 0
means the node is turning work away.

## `/v1/nodes`

On a fleet's admin API, `GET /v1/nodes`, with a `*/*:read` token, asks
every node in `COMBD_PUBLIC_URLS` for its `/_combd/stats` at once, 3 s
each at most.

```sh
curl -s -H "authorization: Bearer $COMBD_TOKEN" https://n0.arena.combd.dev:9443/v1/nodes
```

```json
{"nodes": [
  {"node": 0, "url": "https://n0.arena.combd.dev", "stats": {"objects_live": 65, "...": "..."}},
  {"node": 1, "url": "https://n1.arena.combd.dev", "error": "..."}
]}
```

A node that doesn't answer says why, and the rest still come.

## The panel

`GET /` on the admin port is the panel. Given a token, it shows
`/v1/nodes` and `/v1/deploys` as a page: every node, where each process's
CPU goes, and deploys waiting for builds.

## Logs

combd logs to stdout and stderr, the journal under systemd.
A cell that crashes is logged with the reason and the tail of what it
printed, so a Rust panic's message shows up beside it:

```text
object "acme/shop/cart/42" trapped: unreachable (panicked at src/lib.rs:12: ...)
```

## From outside

`GET /_combd/ping` answers `{}` and is cheap. The arena's scripts wait on
it to know a node is up.
