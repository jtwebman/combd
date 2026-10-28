# Storage and durability

Each durable object has its own SQLite database. A request is one
transaction, and the object answers only once its writes are safe. What
"safe" means depends on whether combd runs on one node or a fleet.

## SQL from a cell

A cell sends one statement at a time, as JSON, and gets JSON back:

```json
{"q": "INSERT INTO posts (title) VALUES (?)", "p": ["hello"]}
```

```json
{"rows": [[1, "hello"]], "changes": 1}
```

An error comes back as `{"error": "..."}`. In Rust,
[`sql::run`](rust-sdk.md#sql) does this.

SQLite runs in the host, so a cell's SQL is fenced in:

- No `ATTACH`, no `PRAGMA`, and no `BEGIN`, `COMMIT`, or `SAVEPOINT` of
  its own. The host owns the transaction.
- No tables named `_combd...`. Those are the host's own, like the alarm.
- A string or blob value is at most 2 MB. An object's database is at most
  1 GB, and a statement or its answer at most 16 MB.
- One statement runs at most 100 ms, or about 10 million SQLite steps.
  The cell's own budget doesn't count SQLite's CPU.

Only durable objects have storage. An `http` process or a WebSocket
session that calls `sql` gets `{"error":"this cell has no storage"}`.

## One request, one transaction

The host opens a transaction before each request and commits it after.

- If the cell answers, with any status, its writes commit.
- If the cell crashes, its writes roll back and the cell is dropped. The
  next request starts a fresh one from storage. A crash never leaves half
  a request behind.
- If SQLite itself rolls the transaction back, say when out of space,
  combd crashes the request, and it fails as a whole.

An alarm set in a request commits with it. WebSocket messages and `send`s
from a request go out only after it commits and its writes are safe.

## Durability, in steps

combd builds durability in steps, each its own safe system with its own
spec and simulator.

| Step | When | An answer leaves once... |
|---|---|---|
| **D1** | one node (`combd dev`, `serve-apps`) | an fsync has put its writes on the node's disk |
| **D2** | a fleet with `COMBD_REPLICAS=0`, the default | the bucket has its writes, and a fresh read of the object's ownership record shows this node still owns it |
| **D3** | a fleet with `COMBD_REPLICAS=2`, say | every one of the object's peers has its writes on disk. With a peer down, the D2 way instead. |

### D1: one node

SQLite runs in WAL mode, and commits don't fsync. A pool of threads
fsyncs each object's WAL off the event loop. Every answer is held until
an fsync has covered its commit. One fsync covers every commit made
before it started. So a busy object pays one fsync for many requests, and
many objects sync at once. A read waits too, if it may have seen a write
that isn't on disk yet. A new object's folders are fsynced, so a power
cut can't lose its file.

On one node, the data lives only on that node's disk.

### D2: a fleet, proven by the bucket

Several nodes share one bucket. The bucket decides who owns each object,
by conditional writes. A request for an object another node owns is
forwarded there.

An object's changed SQLite pages, read from the WAL, go to the bucket as
a segment. An answer leaves only once its segment is uploaded and a fresh
read of the ownership record shows this node still owns the object.
Nothing acknowledged is lost if a node dies. The next owner restores from
the bucket. Many writes share one upload, but each answer waits on the
bucket.

### D3: a fleet, proven by peers

With `COMBD_REPLICAS` set, each object has that many peer nodes, picked by
hash. A write is acknowledged once every peer has it on disk, and the
bucket gets it in batches, in the background. If a peer is down, a write
is acknowledged the D2 way.

While every peer is up, uploads wait up to `COMBD_UPLOAD_MS` (10 s), or
until `COMBD_UPLOAD_BYTES` (1 MB, at most 16 MB) build up. So an object's
newest writes live only on its peers for up to that long. Losing every
peer's disk in that window loses them.

The arena's fleet runs D3 with `COMBD_REPLICAS=2`.

### Reads

In a fleet, a read waits for a round to the object's peers, as proof that
no newer owner has taken over. With `COMBD_READ_LEASE=1`,
owners answer reads with no round while their peers' promises hold. Every
node promises. Turn the lease on only once every node runs a version that
does.

### Taking over

An owner that goes silent keeps its objects for `COMBD_LEASE_MS` (10 s by
default). Then another node can claim them and restore them from the
bucket, and from the peers with D3, with every acknowledged write.

Before it serves, every node tests its bucket. 16 writers race one
conditional write, and exactly one may win. A node refuses to run on a
bucket that fails. Not every "S3-compatible" server passes. One popular
server let several writers win. `combd bucket-check` runs the same test
by itself.

### How it's checked

Each part is checked four ways:

- a TLA+ spec, in `spec/`
- a deterministic simulator, `combd sim`, with crashes, lost messages,
  network splits, and lost disks
- a real cluster with kill tests
- known-bad versions that must fail every one of them

The details are in `plan.md` and `combd/RESULTS.md`.

## What isn't durable

Only what an object writes to its SQLite is kept. These are not:

- **WebSocket sessions** in `ws` processes live in memory only. If
  their node dies, they're gone with their sockets.
- **Ticks** from `tick_every`, and anything an object keeps only in
  memory, stop with a crash or a move.
- **An event cell's work after its answer** is best effort.

The arena keeps a match's game state in memory on purpose. If its node
dies mid-match, that match is lost. The players' records are in SQLite.

## Disk and memory

- An idle object leaves memory after `COMBD_IDLE_MS` (60 s), or when a
  loop holds more than `COMBD_MAX_OBJECTS` (10,000). Its data stays.
- In a fleet, an object's local file goes with the object, since the bucket
  has it.
- With less free disk than a tenth of the data disk or 2 GB, whichever is
  less, a node takes no new objects. A request that would make one gets
  `507 {"error":"no room for new objects"}`.
- Data lives under `COMBD_DATA` (default `data`), one file per object,
  named by a hash of its name, in a folder per app, `<org>/<app>/`.
