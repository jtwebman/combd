# combd: plan

Status: building. The spikes grew into a working runtime: Durable Objects on
one node (D1) and across a fleet (D2, and D3 for fast writes).
Updated: 2026-10-14

This plan was written before we built anything. Most of it held up. Where
building taught us something, the section below says what changed and why,
and the rest of the plan is updated to match.

## The one-line idea

Durable Objects, rebuilt from scratch in Rust, with an Erlang-style runtime.
Every cell is a tiny sandboxed process. The scheduler pauses any cell that runs
too long, so no one cell can take over a CPU. A cell that breaks just crashes
and restarts clean. State lives in SQLite and a bucket you own.

## Decisions so far

- **Ground up.** Inspired by celld and workerd. We use none of their code.
- **No V8. No Node.js.** Not in the first build, maybe never.
- **Rust first.** User code is Rust compiled to a Wasm component
  (`wasm32-wasip2`). Other languages can come later through the same API.
- **Erlang ideas.** Preemptive fair scheduling, let it crash, supervisors.
- **Security is not a later phase.** Built in from the first commit.
- **Correctness is proven, not hoped for.** Formal spec and deterministic
  simulation come before features.

## What building taught us

Each of these started as a line in this plan, met real code or a real test,
and changed. Numbers and details are in [spike/RESULTS.md](spike/RESULTS.md).

**Our own engine was the right bet, with one gap.** We planned our own Wasm
interpreter and kept it: against Wasmtime it won load time (about 85x),
cell start (about 25x), crash restart (about 3x), memory, and binary size.
It lost raw compute badly (15x to 39x), so we added an ahead-of-time tier
that turns Wasm into safe Rust. Over HTTP, a combd cell beat Bun by 1.3x to
1.6x per core.

**JavaScript: compile it, don't interpret it.** A QuickJS spike showed
interpreted JS can't beat Bun, not even native QuickJS. So we dropped
QuickJS and wrote jsc, which compiles JS to a normal Wasm cell. It beats
Node on every route; it's still 9% to 32% behind Bun.

**Capturing writes: SQLite's own WAL, not a custom VFS.** The plan left this
open. Reading the WAL's committed frames (the Litestream and LiteFS way)
covers everything, schema changes too, with no VFS to maintain.

**fsync was 90% of the cost of a write.** So answers now wait on a group
commit (one fsync covers many writes): hot writes went up 8.3x. The event
loop never fsyncs, and checkpoints run off the loop too.

**"S3-compatible" isn't one thing.** MinIO stopped publishing images, so we
use Silo (a MinIO fork). SeaweedFS passed one-at-a-time tests but let 2 to 6
of 16 racing writers win one conditional write, which would let two nodes
own one object. So every node runs a 16-writer race against its bucket at
startup, and refuses to serve on one that fails.

**The fleet must not hit Erlang's walls.** Asking "what about 100, 1,000,
10,000 nodes, or worldwide?" gave us the scaling rules below: no full mesh,
no chatty heartbeats, no global registry, small frames, timeouts measured
per peer.

**D3 came sooner than planned, because the scaling rules demand it.** The
plan said "fast fleet writes, only after D2 is solid." D2 was solid, and a
bucket round trip per write can't scale in cost or speed. So we built D3
right after, the same careful way: spec, then simulator, then real nodes.
- **Every peer must confirm, not "1 or 2".** One fenced peer is what stops
  an old owner, and that only works if an acknowledgement needs them all.
- **A dead peer slows writes, it doesn't stop them.** A write the bucket
  already has can be acknowledged the D2 way.
- **TLA+ found a real bug in the first design.** A new owner skipped the
  fence because the old epoch had no manifest yet; the old owner then wrote
  one late and served writes that were lost. Fix: fence unless the old
  epoch was sealed.
- **The spec asked for something a node can't do.** It read "which epoch has
  a manifest" at the instant of the fence. Now the peer hands back every
  epoch's tail, and the owner picks after listing the bucket.
- **The simulator found a bug the spec couldn't,** because the cleanup that
  caused it wasn't in the spec: peers drop old epochs' logs, which broke an
  owner that froze and woke late. Fix: stop when a newer manifest exists.
  Both are in the spec now.
- **The result:** on the cluster, about 1.6x the writes and half the p50
  (14 ms vs 28 ms), every object correct.

**A peer that loses its disk forgets two things, not one.** It forgets what
it held, and, worse, whom it fenced, which could let an old owner get writes
confirmed again. Now a wiped peer relearns its fence from the bucket before it
answers anything, and speaks only for epochs that began after it came back.
TLC found that our first rule ("it has a log of that epoch since the wipe")
was wrong, because a log being re-sent can be partial. Checked in the spec,
the simulator, and the cluster (kill a node and delete its disk).

**Network splits found three ways we held clients too long.** Safety held
through every split, in the simulator and on the cluster. But an owner cut
off from the bucket gave its objects up on the first lost upload; a node
that couldn't reach the bucket kept clients waiting forever; and one cut
off from everything but its clients held them until they timed out. Fixed
all three: D3 keeps serving on its peers, cut-off nodes say "retry" fast,
and nothing waits longer than a lease. With the owner cut off from the
bucket, D3's typical stall went from 4.7 s to 1.3 s.

**The official Wasm testsuite: 30,497 checks pass, 0 fail.** It found that we
loaded modules using instructions we don't run and only trapped when one ran,
and that a module with two memories silently got memory 0's answers. Both
are refused at load now, by name. What we don't support is counted, not
hidden (SIMD, imported memories and tables, memory64, GC, table copy).

**Differential fuzzing found real bugs in both engine tiers.** Random valid
modules run on our interpreter and Wasmtime (and on the AOT tier against the
interpreter); results, traps, and every byte of memory must match.
- **The interpreter gave a wrong answer:** `big == extend(0)` said "equal".
  A compare swapped its operands for an immediate, and a constant too big for
  one landed in the slot holding the other operand. Wrong, but it only
  touched the cell's own frame.
- **The guard-page AOT build missed traps.** LLVM deletes loads whose value
  is never used, so an out-of-bounds load that should trap never happened.
  No memory outside the cell was touched (the load was removed, not done),
  but Wasm says it must trap. Fixed the way wasm2c does: an empty asm keeps
  every load. It costs 2% to 5%.

**The host API was the weak wall.** Fuzzing it found that one cell could
panic the host (a wrongly typed import), burn seconds of host CPU
(`fd_write`), or take gigabytes of host memory and disk (SQL). The fix
pattern every time: a cell's fuel only counts its own instructions, so
everything the host does for a cell needs its own cap. Values are now 2 MB
(as Cloudflare's), statements 100 ms, objects 1 GiB, and host time 1 s per
request. Fuzzing HTTP found that any client could crash the event loop with
a huge `Content-Length`.

**Benchmarks on a busy laptop lie.** Runs of the same build swung 2x to 5x
while other work ran on the machine. Only runs that alternate A and B can
be compared. Honest totals need quiet Linux machines.

## What happened (the news)

- **Oct 9, 2026:** Cloudflare bought Deno. Deno gets one more year of fixes,
  then development ends. Deno Deploy shuts down in six months.
- The prize was **celld** (Aug 2026, Apache-2.0, now v0.6.2). Cloudflare plans
  to fold its ideas into workerd. Ryan Dahl and Bert Belder lead that work.

## What we learned reading celld

The repo is in `reference/celld` (read only, never built). About 116k lines of
Rust plus 35k lines of JS.

### Where the size really is

| Part | Lines | What it means for us |
|---|---|---|
| V8 executor (`js.rs`, `js/`, runtime pool) | ~27k Rust | We skip this. Wasmtime does the hard part. |
| JS harness and Node polyfills | ~34k JS | We skip this entirely. |
| Durability core (logic, ltx, node log, replication, ownership, bucket) | ~41k | **This is the real work.** |
| Everything else (KV, D1, Queues, R2, Workflows, containers, CLI, telemetry) | ~48k | Mostly later or never. |

V8 was well contained in celld, behind one enum (`CellHost`). The hard,
valuable part is the distributed storage layer, and that is what we must
rebuild with great care.

### Ideas worth copying (as ideas, not code)

1. **A pure decision core.** All coordination logic is one function:
   `on_event(state, event) -> Vec<Effect>`. No I/O, no clock, no randomness
   inside. A real executor and a simulator both drive it. This is what lets
   you test millions of crash and race schedules.
2. **The bucket is the only coordinator.** One record per cell
   (`{node, epoch}`), claimed by conditional write (compare-and-swap). One
   record per node with a lease expiry. No consensus, no control plane.
3. **Every activation takes a new epoch.** Data goes under
   `cells/<cell>/e<epoch>/`. A stale owner can keep writing, but only into a
   dead prefix. That is the fence.
4. **The output gate.** Nothing leaves the cell (response, fetch, socket write,
   stream chunk) until the writes it could reveal are durable. A client never
   sees a value a crash could still lose.
5. **Self-fencing.** A node that can't renew its lease stops serving and exits.
   A supervisor restarts it. (Very Erlang already.)
6. **Startup storage test.** Every node proves the bucket really honors
   conditional writes before serving. Some S3 clones silently ignore them.

### Lessons from their mistakes

- **Review and tests missed a split-brain that lost writes.** TLA+ model
  checking found it, plus four other bugs. Their simulator and specs are
  **not** in the public repo, so we must build our own.
- **Bucket-only writes are slow.** Their lab measured ~600 ms to prove a write
  through the bucket, and ~25 ms when 1 to 2 peer nodes fsync a copy. The
  peer path is fast but is where most of the complex recovery code lives.
- **Subtle rules a rewrite must keep** (from their code comments and issues):
  - A same-node wake still takes a new epoch, or two histories mix.
  - A bucket proof re-reads the ownership record before saying "done".
  - A failed lease write is followed by a read-back, never a blind retry or an
    instant fence. (It might have actually succeeded.)
  - A remote lease is aged from when the read was *sent*, not when it came back.
  - A peer's HTTP error never proves its data is gone.
  - Old epoch cleanup must follow a strict order, or a fenced node can delete
    its successor's base.

### Security gaps we must not repeat

celld says plainly it is single-tenant and trusts the app code. Reading the
code showed why:

| celld gap | What we do instead |
|---|---|
| Worker `fetch` and TCP can dial anything, including `127.0.0.1` (the internal admin port) and `169.254.169.254` (cloud metadata, which can leak bucket keys) | Default-deny egress through one broker. Resolve DNS, check the IP against loopback, private, link-local, CGNAT, and metadata ranges, then connect to that exact IP. Re-check on redirects. |
| Most admin routes have no auth | Admin API on its own listener, mTLS only, scoped roles. |
| One shared HMAC key for all nodes and the CLI. Only the tunnel handshake is signed. No encryption. | mTLS between every node, a cert per node from a fleet CA. Authenticate before reading any body. |
| Up to 1 GiB buffered before auth is checked | Auth first, then small body limits that stream. |
| TCP socket ids are plain integers any isolate can guess | Unforgeable handles in a per-instance table (Wasmtime `ResourceTable`). |
| No default CPU limit | Every cell always has a CPU budget (see scheduler). |
| Secrets are plain text in the bucket | Secrets are their own type, envelope-encrypted at rest. |
| 94 `unsafe` sites, pre-1.0 C extension reachable from user SQL | `#![forbid(unsafe_code)]` everywhere except one small, audited FFI crate. No SQLite extensions. Fuzz the SQL layer. |

**Rule: a cell never sees another cell's bytes.** Every byte of a new cell's
memory starts as zero, as the Wasm spec requires. Today that comes from
`calloc` (kernel zero pages or glibc `memset`). If we ever pool and reuse
memory buffers to make spawn faster, the used part must be wiped before reuse.
The test `fresh_cells_never_see_old_memory` guards this.

## The sandbox: what Wasm gives us, and what it doesn't

**Free with Wasm:**
- No system calls. A cell can only call the imports we hand it. No import, no
  access.
- Memory is one bounds-checked block per cell. A cell can't even address host
  memory or another cell's memory.
- Growing memory asks Wasmtime, not the OS. Our `ResourceLimiter` decides.
- No jumping to random addresses, no rewriting code, no reaching the call
  stack. Overflow tricks can't escape the box.

**Not free:**
1. **Wasmtime bugs.** Its compiler (Cranelift) has had a sandbox-escape bug
   before (2023). Stay current, and don't make Wasm the only wall.
2. **Our host functions.** Every import is a door we wrote. Keep them few and
   small, no `unsafe`, fuzz them all. This is where celld's real gaps were.
3. **Timing side channels (Spectre).** Cells in one process can leak through
   timing. No high-precision timers for cells. Separate tenants by process.
4. **Hogging CPU and memory.** Covered by the scheduler limits below.

**Four walls:**
1. Wasm sandbox
2. Capability-only host API with unforgeable handles
3. One OS process per tenant, locked with seccomp and landlock, no direct
   network (only through our egress broker)
4. Later, for hostile code: a microVM (like Firecracker) per tenant group

## Stateless Workers too

combd can run plain Workers (Cloudflare-style stateless handlers) as well as
Durable Objects. A Worker is just a cell with no name and no storage:

| | Durable Object cell | Worker |
|---|---|---|
| Identity | a name, like `chat/room-42` | none |
| Where it runs | exactly one owner node | any node, many copies |
| Storage | its own SQLite | none (talks to cells, or out via fetch) |
| Needs leases, epochs, the bucket | yes | no, only to fetch the deploy |

Workers skip the hardest part of combd (ownership and durability), so they are
the easy half. A Worker reaches a Durable Object through a binding, like a DO
stub on Cloudflare.

**Warm instances are reused across requests,** like Cloudflare reuses
isolates. Globals, caches, parsed config, and connection pools survive between
requests, and init work runs once. Each node keeps a small pool of instances
per Worker and grows it with load (one instance runs one handler at a time,
unless the guest SDK interleaves requests with async).

An instance is replaced, not reused, when it:
- crashes (let it crash: the next request gets a fresh one),
- goes over its memory limit,
- belongs to an old deploy,
- sits idle long enough to be evicted.

Cheap spawning (3 to 9 us in phase 0) makes all of those fast, but it is not a
reason to throw instances away on every request.

## Levers and visibility (more than Cloudflare gives)

Cloudflare must treat every customer the same and hide its internals. combd
runs on your machines, so it can hand you the controls and show you
everything. Our own interpreter makes the data cheap: it already sees every
slice, every host call, and every byte of memory.

### Levers you can set (per Worker or cell class)

| Lever | What it does |
|---|---|
| `concurrency` | Max requests in flight per instance. 1 = strictly one at a time. Higher only helps guests that interleave async I/O. |
| `min_warm` / `max_instances` | Instances kept warm, and a ceiling per node and per fleet. |
| `recycle` | Replace an instance after N requests, an age, or a memory high-water mark (before it hits the hard limit). |
| `cpu_per_request` | Reduction budget per request. Over it = trap. |
| `slice` | Reductions per slice. Smaller = lower tail latency for neighbors, larger = more throughput. |
| `weight` | Fair share of CPU against other Workers, like BEAM process priority. |
| `memory_max` | Hard memory cap per instance. |
| `snapshot_after_init` | Run init once, snapshot the instance, and start every new instance from that snapshot. New instances come up already warm. Only our engine can do this. |
| placement | Pin to nodes or regions, or spread. |
| egress | Allowlist of hosts the Worker may call. |

### Auto mode: set a target, not a number

Most people should not hand-tune these. In auto mode you set a goal, like
"p99 under 50 ms" and maybe a cost cap ("at most 2 GB"), and a controller
adjusts concurrency and instance counts in near real time (every 100 ms to
1 s).

What it watches, per Worker: arrival rate, queue wait, CPU (reductions) per
request, time spent waiting on host calls (SQL, fetch), memory per instance,
and crash rate.

How it decides:
- **Little's law:** requests in flight = arrival rate x latency. Divide by
  concurrency to get the instances needed.
- **I/O-bound** (most time waiting on host calls): raise concurrency per
  instance, so one instance handles many waiting requests.
- **CPU-bound:** keep concurrency at 1 and add instances, up to the cores.
- Scale up when queue wait threatens the target. Scale down slowly when idle
  (hysteresis, so it doesn't flap).
- Respect `max_instances` and the cost cap, and say so when the target can't
  be met.

The controller is a pure function, `on_tick(stats) -> actions`, like the
ownership decision core. So it gets tested in the simulator against bursty,
spiky, and slow-host-call traffic before it ever runs for real.

### What you can see

- **Per request:** CPU (reductions) used, wall time, queue wait, time per host
  call type (SQL, fetch, cell RPC), how often it was preempted, memory high
  water, and which instance served it.
- **Per instance:** age, requests served, memory now and peak, crashes.
- **Live view:** `combd top`, like `htop`, or like Erlang's Observer. Every
  Worker and hot cell, with its rates, latency, CPU, memory, and what the
  auto controller is doing and why.
- **Crash snapshots you can replay.** A cell is plain data, so when one
  crashes combd can save it as it was at the crash and replay that exact run on
  your laptop. Opt-in only: snapshots hold user data, so they are encrypted
  and access-controlled.

Each Worker only sees its own numbers, never another tenant's.

## Bring your own database

Cells and Workers need to reach Postgres, MySQL, and friends, not just their
own SQLite. Cloudflare solves this with Hyperdrive. combd builds it in.

### Why the host owns the pools

There can be thousands of instances, and they come and go. If each one opened
its own connection, 10,000 cells would mean 10,000 connections and a dead
database. So the **host keeps the pools**: a few connections per node per
database, shared by every instance on that node. A cell never holds a socket.
It holds a `db` binding.

**Update: no per-node pools. One pool per database, for the whole fleet.**
1000 nodes with a pool of 4 each is 4,000 connections, and Postgres defaults
to 100 (each costs it 5 to 10 MB). So a pool is one manager per database
(`pg-main`, `max = 100`), and Postgres never sees more than `max`:
- **A member** is one connection, started on the node of the caller who
  asked, so a holder's queries stay on its node. **A lease** is a member
  checked out by one holder (a request, an event-cell request, an object).
- **The manager** keeps the books: who holds what, the line, the count.
  Asked with none free, it starts a member, up to `max`; at max, the ask
  waits in line and fails after its wait time. A member resetting (just
  released) is waited for, not doubled: a reset is ~0.2 ms, a connect 2 to
  5 ms.
- **At max, the nearest free member is reused.** The asker sends its round
  trips to other nodes, and the manager picks the closest. Members never move:
  in a busy fleet (1000 nodes, 100 members) moving would mean a reconnect on
  almost every lease.
- **Release:** the member resets (`ROLLBACK`, then `DISCARD ALL`) and tells
  the manager it's free. A lease held past its time is taken back by the
  member itself (it rolls back, and the holder's next call gets an error),
  so that works even when the manager is unreachable. Members free past the
  idle time stop.
- **A manager takeover** can briefly overshoot (the old members are still
  finishing), so "too many clients" from Postgres means "wait in line", not
  "fail". The new manager tells every node to stop members from older
  epochs; no heartbeat needed. Per pool, `on_manager_loss`: `finish` (the
  default: finish the current lease, then stop) or `stop` (die now, like a
  linked process in an Elixir supervisor tree).
- **General:** the manager is `pool.rs`, with no I/O and nothing Postgres in
  it. Postgres is the first member kind (in Rust); users' own kinds (Redis,
  a rate-limited API client) come later on the same core.

### How a query flows

1. The cell calls `db.query(sql, params)`, a host import.
2. The engine returns `HostCall`, and the cell is simply paused. A paused cell
   costs ~21 KB and no CPU, so waiting on the database is nearly free.
3. The host borrows a pooled connection, runs the query, returns the
   connection, and resumes the cell with the rows.

```wit
interface db {
  /// A pinned connection. Lives as long as the handle, across many calls and
  /// many requests. Session features all work here.
  resource session {
    query: func(sql: string, params: list<value>) -> result<rows, db-error>;
    begin: func() -> result<transaction, db-error>;
    listen: func(channel: string) -> result<_, db-error>;  // NOTIFY arrives as an event
    close: func();
  }
  resource transaction {
    query: func(sql: string, params: list<value>) -> result<rows, db-error>;
    commit: func() -> result<_, db-error>;
    rollback: func();
  }
  query: func(binding: string, sql: string, params: list<value>) -> result<rows, db-error>;
  begin: func(binding: string) -> result<transaction, db-error>;
  session: func(binding: string) -> result<session, db-error>;
}
```

### Rules

- **Two modes.**
  - **Pooled (default), like PgBouncer's transaction mode.** A connection is
    lent for one query, or held for one transaction. A `transaction` handle
    can span many calls within a request. Best for most Workers.
  - **Pinned session, which PgBouncer can't pool at all.** `db.session()` pins
    one connection to the cell for as long as the handle lives, across many
    calls and many requests. Everything session-scoped works:
    - transactions that span requests (`BEGIN` in one request, `COMMIT` in a
      later one),
    - session advisory locks (`pg_advisory_lock`),
    - `SET`, temp tables, cursors, session prepared statements,
    - `LISTEN` / `NOTIFY`, delivered to the cell as events.

    Durable Objects are the natural home for this. A DO has one name, one
    owner, and runs one thing at a time, so it can own a connection the way
    an Erlang process owns a resource. A Worker can hold a session for one
    request, and reaches longer-lived sessions through a DO.
- **Crash = rollback.** If a cell traps mid-transaction, the host rolls back
  and returns the connection. Let it crash, and the database stays clean.
- **Locks.** Row locks (`SELECT ... FOR UPDATE`) and transaction advisory
  locks (`pg_advisory_xact_lock`) work in both modes and end with the
  transaction. Session advisory locks need a pinned session. A cell waiting on
  a lock is just paused, so waiting costs no CPU. `lock_timeout` is a lever,
  and deadlocks come back as normal errors.
- **Sessions end cleanly, and that releases the locks.** A session closes
  when its handle is dropped, when the cell crashes, hibernates, or is
  evicted, and when its node fences itself or loses the cell. Postgres
  releases session locks when the connection closes, so a lock can never
  outlive its owner. When a cell moves to another node, the new owner simply
  waits for the old connection to close before it gets the lock: mutual
  exclusion holds, and the ownership epoch and the database agree.
- **Open sessions keep a cell resident.** A cell holding a session or an open
  transaction doesn't hibernate until `session_idle_timeout` passes. Then the
  host rolls back, closes the session, and lets it sleep.
- **Sessions can't starve the pool.** Pinned sessions count against the
  fleet-wide cap, and `max_sessions` per binding keeps room for pooled
  queries. When no session is free, `db.session()` waits until its deadline,
  then fails.
- **Clean before reuse.** Before any connection goes back to the pool, the
  host rolls back anything open and runs `DISCARD ALL`. That resets `SET`,
  drops temp tables, unlistens, deallocates statements, and releases every
  advisory lock. If anything about the connection looks off, it is closed
  instead of reused. Nothing leaks between Workers that share a binding.
- **Prepared statements are cached by the host** per connection, so cells
  get the speed without managing names.
- **Credentials never reach the cell.** They are an encrypted secret that only
  the host decrypts. The database host must be in the binding config; the
  egress broker blocks everything else.
- **Fleet-wide connection cap.** You set "at most 200 connections" for the
  whole fleet. Each node takes its share, using the live node count from the
  bucket leases it already reads. No new coordination service.
- **Backpressure, not pileups.** When the pool is busy, requests wait in a
  queue with a deadline. If the database is down, a circuit breaker fails fast
  instead of piling up.
- **Timeouts:** per query, and a max transaction time, so a stuck cell can't
  hold a connection forever.

### Levers and visibility

- Levers per binding: `max_connections` (fleet-wide), `max_sessions`,
  `min_idle`, query timeout, `lock_timeout`, max transaction time,
  `session_idle_timeout`, queue deadline, and read-replica routing.
- Lever per cell class: a scheduling boost for cells holding a database lock,
  so a lock holder is never starved behind busy neighbors.
- Query caching for reads, opt-in per binding with a TTL. Off by default,
  because stale data is a correctness risk.
- You can see: open, idle, and busy connections, pinned sessions and which
  cell holds each, locks held and lock wait time, pool wait time, query
  latency p50 and p99, slow queries, and errors, per binding and per Worker. Auto mode
  uses "time waiting on the database" to raise concurrency per instance.

### Also: raw TCP

For drivers or protocols we don't pool, a cell can open a TCP socket through
the egress broker (like Cloudflare's `connect()`). No pooling help there, and
it still obeys the allowlist.

### Order

Postgres first (most common), then MySQL. The host uses a well-tested Rust
driver for the wire protocol. Each cell's own SQLite stays the default for
per-cell state. A shared database is for data many cells need.

## JavaScript

Most Workers code is JS and TypeScript, so combd needs it to grow past Rust
developers. No V8, no JS engine in the host, and (decided 2026-10-13) **no
QuickJS either**: the JS spike showed an interpreted JS can't beat Bun, so
combd compiles JS instead.

**How: our own JS-to-Rust compiler (`jsc`).** JS → Rust → Wasm → our AOT
tier. The compiled app is an ordinary Rust cell, so it gets the sandbox,
yielding, let it crash, guard pages, and snapshots for free.
- Parse with `oxc`, an existing Rust JS parser.
- A small runtime in Rust: a dynamic `Value`, insertion-ordered objects (so
  `JSON.stringify` key order matches), and a JS-compatible regex engine
  (`regress`).
- Built by need: start with the subset `app.js` uses, and grow with real apps.
  Node is the byte-for-byte oracle (`langs/js/expected.txt`). Bun and Node are
  the speed bar.

**Scope: the JS language plus a web platform layer, not Node.js.** Workers
apps use web-standard APIs (`fetch`, `Request`, `Response`, `URL`, `Headers`,
`crypto.subtle`, `TextEncoder`), a much smaller surface than Node's. Node
compatibility, if ever, is a library of shims on top, as Cloudflare and Deno
did it.

**Status (2026-10-13): step 1 works.** jsc compiles `app.js`, matches Node
byte for byte, beats Node over HTTP on every route, and trails Bun by 9% to
32%. Under a heavy neighbor it keeps 35x to 56x Bun's throughput. Numbers in
`spike/RESULTS.md`. Next for speed: per-request memory regions instead of
refcounts, shapes, and cheaper AOT checks.

**What we support, in steps:**
1. The `app.js` subset, behind our cell interface (`handle(request)`).
2. Workers APIs: `fetch` handlers, `Request` / `Response` / `Headers` / `URL`,
   streams, WebCrypto, and the Durable Object class with `storage.sql`,
   alarms, and WebSockets. Hono, itty-router, and web-standard npm packages
   should work then.
3. Type-directed speed: TypeScript types and inferred shapes compile to plain
   Rust structs where possible.

## Our own JS engine

Bun did not write a JS engine (it runs on JavaScriptCore). But the old rule
that a fast engine "takes big teams a decade" assumed humans write the code.
With AI writing it, the bottleneck moves to checking it, and JS has excellent
checkers. So we treat this as an experiment with a scoreboard, like phase 0.

**Why ours could win where it counts:**
- **Compile JS straight to our engine's own bytecode,** not to Wasm. One
  interpreter, not two stacked.
- **Yielding, let it crash, `snapshot_after_init`, mid-call snapshots, and
  replayable crashes built in from day one.** V8 can't retrofit these.
- **A narrower target than V8:** no browser, no DOM, fewer legacy web quirks,
  and no `eval` / `new Function` (Cloudflare Workers ban them too).
- **Workers traffic is mostly short requests.** V8's peak speed comes from its
  optimizing JIT, which only kicks in once code runs hot. A strong interpreter
  (shapes, inline caches) plus warm snapshots can beat V8 on startup, memory
  per instance, tail latency, and short requests, even where V8 still wins
  long-running compute.

**Measured (2026-10-13): QuickJS can't get us there.** Native QuickJS is 3x to
9x behind Node per request, and 15x on compute. So our engine must be a
compiler, not an interpreter. combd already compiles at deploy, so the best
shot is **compiling the app's own JS/TypeScript ahead of time** to the same
safe-Rust target the AOT tier uses. It would use TypeScript types where they
exist (like Meta's Static Hermes), with shapes and inline caches for dynamic
code. The bar: the Rust version of the same routes already runs at 0.46 to
4.7 us on our AOT tier.

**The harness comes first, before the engine:**
- test262 runner (50,000+ conformance tests), tracked as a pass rate.
- Differential runner: every program on our engine, Node (V8), and Bun (JSC).
  Outputs must match.
- Performance suite: startup, short Hono-style requests, JSON, templating,
  memory per instance, p99 under load, plus long compute benchmarks.
- Fuzzer: random JS programs, run everywhere, outputs compared. Runs nonstop.
- Nightly scoreboard against Node (V8) and Bun (JSC).

**Then build, and let the AI loop grind against the harness:**
1. First: `jsc` compiles the `app.js` subset to Rust, matching Node byte for
   byte on every route.
2. Then grow by real apps: Hono, itty-router, real Workers code. Track the
   test262 pass rate as we go.
3. Type-directed speed: TypeScript types and inferred shapes become plain
   Rust structs where possible.
4. The bar at every step: the same routes on Node and Bun, in-process, over
   HTTP, and in the fairness race. Measured, never assumed.

**Security rules for any JIT:** tests prove an engine gives right answers.
They can't prove a speculative optimizing JIT is safe against a hostile
program, and that is where V8's worst security bugs come from. So:
- the safe-Rust interpreter is always the default,
- any JIT tier is opt-in, for trusted code, in its own locked-down process,
- the fuzzer runs against the JIT before and after every change.

## The runtime: Erlang ideas on Wasmtime

### A cell is a process

| Erlang | combd |
|---|---|
| process | cell (a Wasm component instance) |
| pid / registered name | cell name, like `chat/room-42` |
| mailbox | per-cell request queue (bounded) |
| one message at a time | one handler at a time per cell |
| no shared memory | Wasm gives every instance its own memory |
| reductions | Wasmtime fuel or epoch ticks |
| crash | Wasm trap |
| supervisor | the cell host |
| hibernate | drop the instance, keep SQLite |

### Event cells: many requests per instance (opt-in)

Most Worker requests spend their time waiting on I/O, and one instance per
request wastes memory. A Worker that exports `on_request(id, ptr, len)` and
`on_event(ticket, len)` is an **event cell**, like a Node process: one
instance holds up to `COMBD_EV_MAX` (32) requests at once. It still runs one
callback at a time; requests interleave only where the cell waits.
- Anything that waits returns a ticket at once (`timer_async`,
  `pg_open_async`, `pg_query_async`); `on_event` brings the data back.
- `respond(id, answer)` answers a request, now or later. Work may go on after
  it: that's waitUntil. A request is done when it has answered and has
  nothing open; then its database handles close.
- **The price of sharing:** a crash fails every unanswered request on that
  instance, not just one. Work after `respond` is best effort: if the
  instance crashes, it's lost and nothing retries it. Anything that must
  happen goes before the answer, or into a Durable Object.
  `COMBD_EV_MAX=1` brings back strict one-request let-it-crash.
- Limits: 30 s to answer (then 504), 60 s in all.
- Durable Objects stay one at a time, so a failure never leaves ordering in
  doubt. Batching comes next: an object that exports `on_batch` gets up to N
  queued requests at once (N is its choice: ask for 10 with 2 waiting, get
  2), for one transaction and one fsync, with an answer for each.

### Socket processes: one per WebSocket (2026-10-19)

A WebSocket is long-lived and stateful, so it gets a process of its own,
the way Erlang gives every connection one. A `[ws.*]` process with a
route (`/ws`) starts a new instance, a **session**, for each socket
that connects there. The client never names it, and it ends when the socket
closes. It gets the same events an object does (`WSOPEN`, `WSMSG`, `WSBIN`,
`WSCLOSE`) and runs the app's code for that one user: what they watch,
their rate limit, later their auth.

**Why it's cheap here.** An object claims ownership in the bucket because
two nodes could both try to run it. A session can't have that problem: its
id is the socket's (`<org>/<app>/<process>/~<socket>`), made fresh, and it
lives only on the loop that holds the connection. So it needs no bucket, no
ownership, and no forwarding of its own messages, and it starts at once. It
has no SQLite: what must last goes to an object.

**Reaching one.** Other processes `call` or `send` it by id, where its
`callers` allow. The socket id says which node and loop hold it (the bits
are already there), so the host sends straight there, with no lookup. A
message to a session that's gone is dropped (a call gets 410); it never
starts one. Only a socket can.

**Hibernation: memory follows what it keeps.** Most sockets sit idle, and a
live cell costs tens of KB. A session idle for `hibernate_ms` is asked for
its state (`HIBERNATE`, answered `200 <state>`), and its cell goes; the
host keeps only those bytes. The next event starts a fresh cell and hands
them back first (`WAKE`, read with `ws_msg`). A viewer that keeps a socket
id and two ints costs about 24 bytes plus the socket; one that answers
anything but 200 stays warm. That's the goal of a BEAM process (about
2.7 KB). Each process sets how it works:

| Setting | What it does | Default |
|---|---|---|
| `hibernate_ms` | idle time before the cell goes; `0`: after every event (a state machine: state in, event, state out) | 5000 |
| `hibernate` | `auto`: the host freezes the cell (what it changed against the image), no app code; `app`: `HIBERNATE`/`WAKE`; `off` | `auto` |
| `state_kb` | the most a hibernating session keeps | 4 |
| `memory_kb` | the ceiling its memory may grow to (any process; past it, the cell crashes) | the engine's |

**Push straight to the socket.** Waking a session just to pass bytes along
wastes a cell start per message. So `send` with `"ws": text` (no path)
writes to a session's socket without running it: the session does the
control (what to watch, the client's own messages), its sources do the data.
A watching viewer never runs. That's Phoenix's fastlane, in our shape. The
same `callers` rule decides who may.

**Subscriptions are the app's.** An object that pushes to sessions keeps
their ids, and sessions renew what they watch on a beat; the object forgets
ids that stop renewing, so a node that dies without closing its sessions
leaves nothing behind. combd adds no topics.

**What Cloudflare and celld do instead.** Both send an upgrade to a
stateless Worker, which picks an object (`stub.fetch`) that takes the
socket; a socket belongs to one object. Per-user logic means an object per
user (a full one, with its cost) or a Worker pinned for the socket's life
(no hibernation). celld pipes the bytes to the owner node; neither has
pub/sub. Phoenix does what we do (a process per socket and per channel),
and learned that sending every broadcast through every subscriber's
process is slow at large fan-out ("fastlane": encode once, write to the
sockets).

**Next, once it works:** one message per node, not per subscriber (an
object sending one change to 500 sessions on 3 nodes sends 3 wire frames;
each node fans out to its own), and a load test that holds N idle
connections and measures memory per connection.

### Lighter cells: toward a BEAM process (2026-10-19)

A real app's cell is 74 to 81 KB on Linux x86 (the arena's). Most of it is
copied, not used: each cell starts by copying the module's static data
(35 to 60 KB in our examples) into its own memory, and Wasm memory never
shrinks, so a cell keeps whatever its worst request needed. An Erlang
process starts at 2.7 KB because code and constants are shared and its
heap grows and shrinks. The plan:
1. **Pools shrink.** Warm worker cells idle past `COMBD_IDLE_MS` go (the
   newest stays). Built 2026-10-19.
2. **Caps.** `memory_kb` on a process: past it, a grow fails; one under
   the module's start is refused at deploy. Built 2026-10-19 (interpreter).
3. **A shared starting image.** Each cell maps the module's starting
   memory copy-on-write, so static data costs once per module, not once
   per cell, and a cell starts by mapping, not copying. Built 2026-10-19
   (`combd-mem`): a new cell is about 1 KB, down from 21 to 65.
4. **A fresh cell per request, opt-in** (`fresh = true` on http): nothing
   one request left can reach the next. Built 2026-10-19; measured at 4x
   to 25x the time of a warm cell (page faults, and caches rebuilt), so
   warm stays the default and idle trimming bounds the creep.
5. **One reserved region** for every cell's memory, past Linux's map limit.

3 to 5 need `mmap`, so they go in one small audited FFI crate. A live
cell's floor is a page or two of stack and heap plus the interpreter's
state, so about 8 to 12 KB; a hibernated session is under 1 KB of host
state. Measured before and after, every step.

### Settings: the manifest, environments, vars, and secrets

An app's settings live in one manifest shipped with its code: `combd.toml`,
`combd.yaml`, or `combd.json` (all three read the same; humans like TOML
or YAML, AI writes JSON fast). It describes the app's shape, the same in
every environment:
- `[vars]` and `[secrets]` declare every env var, typed. No default means
  required; `?` means optional. Secrets map to a secret's name and never
  have defaults. Processes read both as normal env vars.
- A process's type is its table (`[http.*]`, `[ws.*]`, `[durable.*]`,
  `[pool.*]`), and each type declares and types its settings; `module`
  names its code (or a built-in, `combd:postgres`). `"$NAME"`
  references must match types exactly. Any mismatch and nothing starts.

**Environments live in the control plane, not the file.** Teams make one
per pull request: `combd env create pr-123 --from staging --ttl 7d`. An env
inherits values from the one it came from and overrides what differs.
`combd var set` and `combd secret put` work only on declared names, and
check the type when the value is set. A deploy fails early if an env is
missing a required name. Each env has its own Durable Object namespace,
storage, and pools, so a preview can never touch production's objects;
deleting an env deletes its data. Secrets are encrypted at rest (a data key
per app, sealed by a KMS master key; AES-256-GCM with the app, env, and
name bound in), write-only in the API, and scrubbed from logs. Local dev is
an env named `dev`, backed by a gitignored file.

### Everything is a process

Workers, event cells, Durable Objects, pool managers, and pool members are
all processes, told apart by placement (anywhere, or one owner), state
(none, or its own SQLite), and concurrency (one event at a time, or many).
What they hold (a pool lease, a WebSocket, a database session, a timer) is
a **handle**: an owner, a release, a timeout, and cleanup when the owner
crashes. A per-node ingress pool owns the TCP sockets, parses HTTP, and
routes by domain and path to a process as an event. A WebSocket is a handle
the node keeps: the process gets events and can hibernate or crash while
the socket lives on. Users can write their own member kinds (a MySQL
connection, a rate-limited API client); a member implements `on_start`,
`on_call`, `on_release`, and `on_stop`, and the pool does the rest.

### Supervisors: let it crash, then tell someone

The control plane supervises everything (it already restarts what crashes)
and can kill anything at any time: an object, an instance, a pool member, by
id. Users may add their own supervisor: a Durable Object that gets a crash
event (what crashed, why, how many requests failed) for everything it
watches, saved and in order. It can post to Slack, count crashes, or ask the
control plane to stop or restart a group (one_for_all, with a cap on
restarts per minute so a crash loop doesn't flap). Needs outbound `fetch`
from cells first.

### Fair scheduling (no cell hogs a core)

- One worker thread per core (Tokio multi-thread runtime).
- Each cell runs on its own small stack (Wasmtime async fibers), so the host
  can **pause it mid-loop** and resume it later. Rust `async` alone can't do
  that. A Rust loop with no `.await` would block the thread forever.
- **Soft limit (the reduction count):** every ~1 ms of guest time, an epoch
  tick makes the guest yield back to the scheduler. Other cells get a turn.
  (`epoch_deadline_async_yield_and_update` in Wasmtime.)
- **Hard limit:** a per-request CPU budget (say 50 ms default, set per cell).
  Over budget = trap = crash. The request fails, the cell restarts.
- **Memory limit:** `ResourceLimiter` caps each instance's memory, tables, and
  instance count. Over the cap = trap.
- **SQL can hog the CPU too.** A host-side SQLite call can't be paused by Wasm
  fuel. Use SQLite's progress handler to count steps and abort long queries,
  plus `sqlite3_limit` and an authorizer (no ATTACH, no raw pragmas).

### Let it crash

- A trap kills only that instance. Host memory is never touched.
- An open SQLite transaction rolls back. The last committed state is safe.
- The supervisor makes a fresh instance on the next message. Wasmtime's pooling
  allocator makes that take microseconds.
- **Restart intensity, like Erlang:** if a cell crashes more than N times in T
  seconds, stop restarting it, back off, and report it. One bad cell can't burn
  the node.
- **Monitors (later):** a cell can ask to be told when another cell crashes.

### The host API (one WIT file)

```wit
package combd:cell@0.1.0;

interface storage {
  // sync, because a cell runs one handler at a time
  exec: func(sql: string, params: list<value>) -> result<rows, sql-error>;
  transaction: func(body: ...) -> result<_, sql-error>;
}
interface alarm   { set: func(at-ms: u64); clear: func(); }
interface cells   { call: func(name: string, msg: list<u8>) -> result<list<u8>, call-error>; }

world cell {
  import storage;
  import alarm;
  import cells;
  import wasi:http/outgoing-handler@0.3.0;   // goes through the egress broker
  export wasi:http/handler@0.3.0;
  export on-alarm: func();
}
```

A cell only gets the imports it was granted. No ambient access to anything.
WASI 0.3 (June 2026) added native async streams and futures, so HTTP
handlers and streaming work cleanly.

What a user writes, roughly:

```rust
#[cell]
struct ChatRoom;

impl Cell for ChatRoom {
    fn fetch(&mut self, req: Request, ctx: &Ctx) -> Result<Response> {
        ctx.sql("INSERT INTO msgs(body) VALUES (?)", [req.text()?])?;
        Ok(Response::ok("saved"))
    }
}
```

## Durability, built in steps

Each step is its own safe system. We only move up when the step below has
passed its spec and simulator.

| Step | Ack a write when... | Speed | What can be lost |
|---|---|---|---|
| **D1: single node** | local fsync done. Bucket backup runs async. | very fast (~1 ms) | last few seconds, only if the disk dies |
| **D2: fleet, bucket proof** | the bucket has it and ownership is re-checked | slow (~hundreds of ms, many writes share one upload) | nothing acknowledged |
| **D3: fleet, peer proof** | every peer of the object (2 by default) has it on disk; if one is down, the D2 proof instead | fast (~14 ms p50 on our Docker cluster, vs ~28 ms for D2) | nothing acknowledged, as long as a peer's disk survives |

All three are built. D1 is honest and simple, and good for many apps. D3 is
where celld's hardest code lives (node logs, sealing, takeover recovery). We
planned to earn it last; we built it right after D2, because the scaling
rules need it, and gave it the same spec, simulator, and kill tests first.

**Capturing writes (decided):** read the WAL's committed frames, Litestream
style. A custom VFS turned out not to be needed.

## Fuzzing: catch the bugs normal tests miss

The TinyGo bug (an `if` right after a stack op left the stack pointer one
slot high) was found by luck: one compiler happened to emit that pattern.
Fuzzers find those on purpose.

**Status, 2026-10-14:** item 1 runs (`combd-spike fuzz`, `aot-fuzz.sh`), along
with the official spec testsuite (`spectest.sh`), and both found real bugs
(see "What building taught us"). The AOT tier handles multi-value now too.
Item 4 (the host API) and the HTTP parser and wire are fuzzed too
(`fuzz-host`, and tests). Still to do: Wasmtime against the AOT tier
directly, and items 2, 3, and 5.

The list:

1. **Differential fuzzing of every engine.** `wasm-smith` makes random valid
   Wasm modules. Each one runs on our interpreter, our AOT tier, and
   Wasmtime. Results, traps, and final memory must match exactly. Any
   difference is a bug in one of them. Shrink the module to the smallest one
   that still disagrees, and keep it as a regression test.
2. **Translator fuzzing.** Mix register ops and stack ops in random orders,
   with random block shapes, branch depths, and `local.tee` chains, to stress
   the virtual stack, lazy `sp`, and fused branches.
3. **Pause and resume fuzzing.** Run the same module with random slice sizes
   (1, 7, 100, ...) and random snapshots mid-call. The answer must not change.
4. **Host API fuzzing.** Random arguments to every WASI and combd import:
   bad pointers, huge lengths, overlapping buffers. The host must never panic
   or read outside the cell.
5. **Security properties as fuzz targets:**
   - a new cell never sees an old cell's bytes,
   - memory limits always hold,
   - a trap never leaks into a neighbor.
6. **Races and crashes:** the deterministic simulator below, for the
   distributed parts.

Fuzzers run nonstop in CI on a spare machine. Every failure they find becomes
a permanent regression test.

## How we prove it's correct

1. **TLA+ (or Quint) spec first** for ownership, epochs, leases, and the output
   gate. Two invariants: one writer per epoch, no lost acknowledged write.
   Keep known-bad versions of the spec that must fail, to prove the checker
   works.
2. **Pure decision core** like celld's: events in, effects out.
3. **Deterministic simulator** in the repo from day one. Fake bucket that adds
   latency, CAS races, and lost replies. Clocks that drift. A crash at every
   await. Seeded, so any failure replays exactly. Wasm is deterministic enough
   to include real cells in the sim, which celld couldn't do with V8.
4. **Kill tests on real machines:** SIGKILL mid-write, freeze a node, cut it
   off from the bucket, from the other nodes, or both (all done, every
   object correct). Still to do: delete a node's disk (today it must come
   back as a new node), one-way splits on real machines, and a bucket that
   throttles with 429s.
5. **Fuzzing** for the HTTP parser edge, the SQL layer, and the peer protocol.

## The engine: our own Wasm interpreter (bold option)

Instead of Wasmtime, write our own interpreter in safe Rust. This fits the
Erlang idea better than a JIT does. BEAM itself started as an interpreter and
only added a JIT (BeamAsm) in 2021.

**Why it fits:**
- **True reductions.** Count instructions and pause a cell at any instruction.
  No epoch ticks, no fibers.
- **A paused cell is plain data:** value stack, call frames, program counter,
  and linear memory. No 2 MB fiber stack per cell, so many more paused cells
  fit in RAM.
- **Powers Wasmtime can't give:** snapshot a cell mid-request, move a running
  cell to another node, and replay it exactly in the simulator. Native machine
  code state can't be saved like that.
- **Smaller attack surface.** No JIT means no codegen bugs (the 2023 Cranelift
  sandbox escape is a whole class that goes away) and no writable+executable
  memory. `#![forbid(unsafe_code)]` in the whole engine.
- **Light and instant.** Tiny binary, no compile step on deploy.
- **Strong test oracles.** The official Wasm spec test suite, plus
  differential fuzzing: random modules from `wasm-smith`, run on ours and on
  Wasmtime, results must match.

**The costs:**
- Pure compute runs roughly 3 to 10x slower than Wasmtime. Most cell time is in
  SQLite and the network, which run native in the host. Heavy work (crypto,
  compression) becomes host functions.
- The core interpreter is the small part. The component model, WIT, and WASI
  0.3 async are the big part. For Rust first, skip them: plain core Wasm
  modules plus our own small import API. Add components later.
- Decoding and validating Wasm is the front door, and a bug there is a
  security bug. Use the Bytecode Alliance `wasmparser` crate for that (a
  heavily fuzzed library, not a runtime). Everything after it is ours.

**Design sketch:**
- Translate each validated function once into our own compact internal
  bytecode (register-based, with fused common ops). Don't interpret raw Wasm.
- Every loop back-edge and call costs reductions. At zero, save state and
  return `Yield` to the scheduler.
- Explicit bounds checks on every memory access. Memory grows only through
  our limiter.
- Trap = return an error value. The host never panics because of guest code.

**Prior art:** wasmi (Rust, fuel and resumable calls), wasm3 (fastest C
interpreter, mostly in maintenance), Stitch (fast Rust interpreter), Pulley
(Wasmtime's own interpreter mode), Wizard (research engine, in-place
interpreter).

**Keeping the JIT door open.** Interpreter first, JIT later is the BEAM, V8,
JVM, and CPython path. Rules from day one so a JIT can slot in:
1. One internal bytecode. The interpreter runs it, a future JIT compiles it.
2. Pauses only at fixed safe points (loop back-edges, calls). Any tier must
   write its state out in interpreter format at those points. This keeps
   snapshot, migration, and exact replay working with a JIT.
3. One reduction counter, same place, same meaning in every tier.
4. One memory layout and host-call ABI, so interpreted and compiled functions
   call each other freely.
5. Per-function hot counters from the start, to pick what to compile.

The interpreter stays the JIT's test oracle forever (differential fuzzing).

JIT costs: it brings back `unsafe`, writable+executable memory, and the codegen
bug class. So it is opt-in per deployment, off for untrusted tenants, and the
compiler runs in its own locked process. Use Cranelift as a codegen library
rather than writing our own backends.

Speed ladder, stop when good enough: (1) faster interpreter (register
bytecode, fused ops), (2) copy-and-patch baseline JIT like CPython's,
(3) Cranelift optimizing tier for hot cells only.

**Engine interface.** Worth having from day one, because the spike already
has two engines (ours and Wasmtime), and tiers come later.

```rust
trait Engine {
    fn load(&self, wasm: &[u8]) -> Result<Program>;   // validate + translate, once per deploy
    fn spawn(&self, p: &Program, limits: Limits) -> Result<CellVm>;
}

trait CellVm {
    fn run(&mut self, reductions: u32) -> Outcome;   // run until something happens
    fn resume(&mut self, host_result: HostValue);
    fn snapshot(&self) -> Option<Snapshot>;          // None if the engine can't
}

enum Outcome { Done(Value), Yield, HostCall(HostCall), Trap(Trap) }
```

- The engine never does I/O. It returns `HostCall` and waits for `resume`.
  Same events-in, effects-out idea as the decision core, so engines stay
  deterministic and simulator-friendly.
- `snapshot` is optional so Wasmtime can still plug in.
- Rule: nothing engine-specific ever reaches disk or the wire. (celld stores
  KV values in V8's serialization format, which ties its data to V8 forever.)

**Expected scoreboard vs celld (guesses until measured):**

| Measure | Guess | Why |
|---|---|---|
| Memory per cell | big win | V8 isolates cost MBs; ours is a struct + linear memory |
| Cells per GB | big win | follows from memory |
| Cold start | win | no isolate setup, no compile |
| Fairness, p99 under load | win | celld cells share isolate turn lanes, no default CPU limit |
| Binary size | win | celld v0.6.2 ships ~22 MB gzipped, mostly V8 |
| Raw compute | loss | V8 JIT vs our interpreter; Rust's leanness closes some gap |
| Durable writes | tie | disk and network bound (~25 ms fleet, ~600 ms bucket in celld's lab) |
| Warm request latency | tie or small win | celld p50 ~1.1 ms, mostly HTTP + SQLite |

The spike measures every row against a real celld node on the same machine.

**Decision (after the phase 0 race):** our own interpreter is the default
engine. Wasmtime stays behind the engine interface as a fast-compute option.
See [spike/RESULTS.md](spike/RESULTS.md). If we ever need raw speed, add a JIT tier for
hot cells later, the way BEAM did.

## AOT tier: compile once per CPU, cache it, run it everywhere

Our interpreter is 6x to 15x behind Wasmtime on compute, and about 20x behind
on JS. The interpreter alone won't close that, so cells that need speed get a
machine-code tier, compiled ahead of time instead of while requests run.

**How it works:**
1. At `combd deploy`, the tool (or a build node) compiles each module to
   machine code for each CPU target the fleet uses: x86-64 at a couple of
   feature levels, and ARM64.
2. The artifacts go into the bucket next to the deploy, keyed by
   `hash(wasm) + target + CPU features + compiler version`.
3. A node picks the artifact that matches its CPU. If there isn't one, the cell
   starts on the interpreter at once (no compile needed), and the node compiles
   in the background, then switches. That's V8's tier-up idea, but done at
   deploy time, so there's no warm-up stutter and no compiler in the serving
   process.
4. The auto controller decides per cell: CPU-heavy cells and JS cells run the
   AOT tier; idle-heavy cells stay on the interpreter (no compile, smallest
   memory).

**Security rules (it's executable code):**
- Compile in a separate locked-down process: no network, seccomp.
- Sign every artifact. A node verifies the signature before mapping it as
  executable, and memory is never writable and executable at once.
- Only artifacts from our own compiler are ever loaded, never from tenants.
  (Wasmtime warns that loading precompiled code is unsafe for untrusted
  input.)

**Keeping our superpowers:**
- Preemption: generated code counts reductions at loop heads and calls, the
  same safe points as the interpreter.
- Pausing mid-function needs one of two things:
  - (a) a native stack per running cell, like Wasmtime's async fibers. Simple
    and proven, but each paused cell costs a stack.
  - (b) at a safe point, compiled code writes its live values into the
    interpreter's frame format, and the cell continues as plain data. This
    keeps mid-call snapshots and live migration, but needs stack maps.

  Start with (a), add (b) later.
- `snapshot_after_init` keeps working either way. It's taken when no frames are
  on the stack.

**No tier-up (decided 2026-10-20).** A module runs compiled or
interpreted, chosen in its manifest, never both and never switching while
it runs: a cell that ran compiled under load and fell back to the
interpreter (or the other way) would be a different program to the
operator. A compiled module's deploy waits for its builds; see "Compile on
deploy" below.

**Compile runtimes once, apps at deploy.** The `jsc` runtime (values, JSON,
regex, Workers APIs) is the same for every JS app, so it is compiled once per
version per CPU and shared. Only the app's own code compiles at deploy,
whatever language it was written in.

**Built (2026-10-13): Wasm to safe Rust.** Our own compiler (`aot-gen`)
turns a Wasm module into plain safe Rust, and rustc + LLVM produce the machine
code for any CPU. No Wasmtime. rustc guarantees the generated code can't
escape its sandbox, so a generator bug can't become a security bug. It beat
Wasmtime on fib, sieve, and every JS API route, and cold starts a JS cell from
a snapshot in 53 us. See [spike/RESULTS.md](spike/RESULTS.md).

**Racing Bun (2026-10-13):** a sandboxed, preemptible Rust cell on this
tier ties or beats Bun on two of three API routes and beats Node on all three.
Native QuickJS loses to Node and Bun, so JS throughput needs a JS compiler,
not a faster QuickJS (see "Our own JS engine"). Details in
[spike/RESULTS.md](spike/RESULTS.md).

Done: 1-byte traps (results in registers), memory as its own argument, a stack
budget in a register, fuel in a register, one bounds check per access, lazy
zero-skipping snapshot copies.

Done (2026-10-13): **guard pages** behind `aot-guard` (`guard.rs` +
`trap.c`, the only `unsafe` code). JSON route 3.54 us in-process, and 1.32x to
1.38x Bun's throughput over HTTP.

Next for this tier, biggest first:
1. **A JS-to-Rust pipeline** for the simple routes (JS → Rust → Wasm → this
   tier). See "Our own JS engine".
2. **Pausing mid-call:** a stack per cell first, deopt to the interpreter
   later.
3. **SDK: an opt-in per-request arena allocator** for Rust cells (bump
   allocation, reset per request). It could beat native code that uses
   malloc.
4. **Wasm SIMD** in the generator.
5. Maybe a Zig or C backend if rustc compile time hurts. The safe Rust
   backend stays the reference.

## Compile on deploy: builders, the admin panel, and CPU targets (2026-10-20)

The AOT tier is 15 to 39x the interpreter, but today only modules built
into the combd binary run compiled, so every deployed app (million, the
arena) runs interpreted. The platform should work like Cloudflare's:
upload the manifest and the Wasm, and it comes back running fast.

**Each module picks its engine** (`[modules.<name>] engine = "compiled"`,
the default, or `"interpreted"`), and runs that way only.

**A deploy goes live when it's built.** Uploading still goes through the
admin API, and the bucket holds it, but nodes don't take it yet:
1. The cluster lists the builds it needs: each compiled module, for each
   CPU target its nodes have.
2. Builders take those jobs, compile, sign, and upload the artifacts.
3. When every build is there, the deploy is ready; nodes fetch the
   artifacts for their own target and move to it as deploys move today
   (an object at its next idle moment, so nothing in play is cut off).
4. A build that fails fails the deploy: the version that's running keeps
   running, and the panel says why. There is no interpreted fallback.

**Builders are their own machines.** Compiling Rust takes cores and
minutes; a 1 vCPU node can't spare either. A builder is `combd builder
<cluster admin URL>` with a build token: it asks for jobs, runs `aot-gen`
(Wasm to safe Rust) and rustc, and uploads. The first one is a machine we
own (the Windows box, under WSL); later the platform can start them on
demand. Any number can run; a job is leased to one at a time.

**Targets come from the cluster.** Each node reports what it is: x86-64 at
a feature level (v2, v3, v4: AVX2 and so on) or ARM64, and builds are per
target, so a mixed cluster (x64 and ARM, older and newer x64) gets a build
for each kind it has. Tuning for one CPU model (`znver4`, `icelake`) can be
an option per cluster later; for this code it's usually a few percent.

**Loading one: the one new `unsafe`.** An artifact is the generated Rust
built as a library, loaded into the running node (`dlopen`). Rust has no
stable ABI, so it must be built by the same rustc and the same combd-aot
sources as the node's own binary: the node publishes that pin (rustc
version, combd-aot hash, target), the builder uses that toolchain, the
library carries the pin, and the node refuses one that doesn't match.
The code inside is safe Rust (`forbid(unsafe_code)`), so the sandbox comes
from rustc on the builder, which is why nodes trust only signed artifacts:
each builder has a key the cluster lists, and a node checks the signature
before loading. The loader is a small crate of its own, like combd-mem.

**The admin API and panel.** On the admin port, for operators, not apps:
- `/v1/nodes`: each node, its version, target, region, and stats;
- `/v1/procs`: per process, CPU time (counted by the loop around every
  slice), cells, sessions awake and asleep, mailbox refusals, batches;
- `/v1/deploys`: each deploy, its builds per target (wanted, building,
  done, failed) and which nodes have moved to it;
- `/v1/builders`: who's building what.
And a page at `/` that shows them: what I've been doing by hand with SSH
and perf, without either.

**Later: GPUs.** Wasm can't touch a GPU itself, so it would be host calls
(as wasi-nn does), and placement: a node says what it has, a process says
what it needs, and it runs only where that is.

**Build order:**
1. Per-process CPU accounting, and `/v1/nodes` and `/v1/procs`, and the
   panel. (Needed to see what the rest does.)
2. `[modules]` and `engine`, with compiled modules that are already in the
   binary (the tests' guest): the plumbing, and deploys that wait.
3. The artifact: generated Rust as a library, the pin, the loader.
4. `combd builder`, build jobs, signing, artifacts in the bucket, and
   `/v1/deploys` and `/v1/builders`.
5. Node targets (x86-64 levels, ARM64), and a build per target.
6. Million and the arena compiled, and the click test again.

**Queued after it:**
- **The click test against celld and workerd** (and Cloudflare, for cost
  and latency): a Workers version of million, `ws-click` against each on
  the same droplet size, sessions per GB and changes per second side by
  side. After AOT, so combd isn't measured on its interpreter against
  their JITs.
- **Million: hot chunks split.** On the real site most people land at box
  1, so chunk 0 is hot while the rest idle. A supervisor holds the chunk
  map; a chunk that runs big batches or refuses work asks to split, and
  its halves (new ids, spread in the region) take its bits; viewers get
  the map with their next renewal. Pays only where the halves can run on
  different cores or nodes, and it doesn't cut pushes (a viewer of both
  halves gets every change); it spreads them. Test it with most viewers
  at the top, not spread evenly.
- **A billion checkboxes** (2026-10-22): million times a thousand, on the
  same three $6 nodes. The bits are only 125 MB (64,000 chunks at million's
  size), so it works only if almost every chunk sleeps, and it has to hold
  against someone who writes to every chunk as fast as we allow. That
  needs four things, each good for every app; measure first (waking cost
  when every write lands on a cold chunk, bucket writes per click), then
  build them, then the billion:
  1. **Memory.** Every write wakes its chunk: 64,000 awake at ~50 KB is
     ~3 GB, more than the fleet has. The whole-machine memory check helps
     (under pressure, idle chunks sleep again within a second), but how
     fast can a node wake chunks when every write hits a sleeping one?
     Not measured yet.
  2. **Writes per client.** Million caps a socket at 20 changes a second,
     but an attacker opens thousands of sockets. combd needs per-IP limits
     at the edge, since it does what Cloudflare would do in front.
  3. **The bucket bill.** Each awake chunk uploads its changes to R2 in
     batches, and R2 charges per write: a script touching every chunk is
     millions of bucket writes an hour. combd needs a floor on how often
     one object uploads, and a fleet-wide budget.
  4. **Hot chunks.** Everyone piles onto the first screen: the splitting
     above.

## Cheaper sends: kernel TLS, then io_uring (2026-10-21)

The one-node ramp (million, every page clicking) put the ceiling at
~10,000 active users on a 1 vCPU node: past it the CPU is gone, and most
of it isn't ours. At the peak: kernel 43%, combd's host code 24%, libc
24% (allocating, copying), the compiled apps 8%. The kernel's share is a
small TLS record and a write per socket per frame: TCP's send path, the
virtio NIC kicked per packet (`iowrite16`, ~6%), and each system call's
entry and exit with AMD's SRSO mitigation (`srso_safe_ret`). Encryption
itself was ~0.5% (rustls on AES-NI). So it's the count of sends and the
work around each, more than the crypto.

**First, measure where the kernel's 43% goes** (a load droplet set, perf
with call graphs on a node at 10,000 and 20,000 active users): system
call entry/exit, `tcp_sendmsg` and transmit, the NIC kicks, receiving
the ACKs (softirq), epoll. That decides the order below; it's a day, not
a guess.

**1. Kernel TLS (kTLS).** After the handshake, rustls hands over the
session keys (`UnbufferedServerConnection::dangerous_into_kernel_connection`,
in the version we use) and the kernel encrypts and decrypts on the
socket (`TCP_ULP` "tls", `SOL_TLS`). What it buys:
- **Memory:** a connection drops its rustls state once the keys are in
  the kernel: most of the ~9 KB an idle TLS connection costs today, for
  every app (105,000 idle users on a node was mostly that).
- **CPU:** a frame is written once, plain, to the socket: no copy into
  an outgoing buffer, no record framing in combd. The kernel's crypto
  is the same AES-GCM.
- What it doesn't: the system call per socket per frame stays.
- The work: a small crate for the socket options (`unsafe`, like
  combd-mem and combd-load); the kernel connection rustls returns for
  what comes after the handshake (key updates; session tickets are off);
  TLS 1.3 and 1.2 with AES-GCM (ChaCha20 where the kernel has it); a
  node without the `tls` module, or a cipher it can't do, keeps
  rustls as now (a node, not an app, so this isn't a tier-up).
- Risk: an alert or record the kernel hands back as a control message
  must be read right, or a connection hangs; the Autobahn and testssl
  runs cover it.

**2. io_uring for the sends.** The loop keeps mio for readiness, and
only the flush (every socket with frames waiting, `flush_unsent`) goes
through one io_uring submission: one system call for all the sockets a
turn writes, not one each, and the NIC driver sees them together (one
kick for many packets). With kTLS the frames are plain bytes, so a send
is a single submission entry. Linux 5.6+; the ring and its buffers in
the same kind of small `unsafe` crate. Without io_uring (another OS, or
it's off), the flush writes as now.

**Measure after each:** the same ramp (10,000 to 40,000 active users on
one node), and the idle hold (105,000 before: how many now).

**Measured (2026-10-21), and the order changed.** The whole system on one
1 vCPU node, 10,000 active users (20,000 alike): kernel 75% of all CPU;
`sendto` 45% (TCP transmit 27%, the device queue 13%), receiving 24%
(network interrupts and softirq, mostly the ACKs for those packets), the
NIC kicks 4%, system call entry with SRSO 3%, copying data in 0.3%. So
the ceiling is packets a second on a small VM, not the system calls and
not the crypto. kTLS saves almost no CPU, but it's still the memory win
(TLS is ~8 KB of an idle connection's ~10), so it goes first, measured
by the idle hold. io_uring would save maybe 5 to 10%: later, if at all.
More active users per node means fewer packets per user (an app's
choice, as million's beat) or bigger machines.

**io_uring, built and measured (2026-10-21): no gain.** `COMBD_URING=1`
(Docker needs `--security-opt seccomp=unconfined`). At 10,000 active
users, kTLS on, two rounds each, the ring carried 98% of the frames and
the node's CPU didn't move: 66% and 65% without, 67% and 63% with, the
same latency. The cost is TCP itself, per packet, not the system calls.
It stays in, off by default; the arena's nodes don't run it (not worth
dropping Docker's seccomp for).

**Both removed (2026-10-22).** kTLS turned out to move memory into the
kernel rather than save it (`spike/RESULTS.md`), and io_uring saved no CPU:
Linux-only unsafe code for nothing, so out. Git has both (`combd-ktls`,
`combd-ring`) if NIC offload or bigger sends make them worth measuring again.

## The edge: TLS in combd, or edge nodes in front (2026-10-20)

combd gets an idle connection down to 2.24 KB, but in front of it on each
node sits Caddy: it ends TLS and proxies every socket to combd on
localhost. So a client is three sockets on the node (client to Caddy,
Caddy to combd, and combd's end), plus Caddy's own state for it (TLS,
buffers, two goroutines), by estimate tens of KB. On a 1 GB node that's
the limit, not combd. Two modes, one code path:

**Single (small fleets, like the arena's three droplets).** combd ends TLS
itself (rustls, already used on the wire), on 443, and connections go
straight into its loops: one socket per client, no hop. Certificates come
from files (what `tls.sh` already ships) and reload when they change; port
80 redirects. A restart hands the port over (`SO_REUSEPORT`: the new
process listens before the old one stops), so new connections never fail;
open sockets reconnect, as they do now. Idle connections give back their
TLS buffers, as they do their WebSocket buffers.

**Edge (big fleets).** Machines of their own run `combd edge`: the same
binary, holding only client sockets and TLS, no cells, near users. They
carry every client over a few multiplexed connections on the wire we
already have, to compute nodes that run combd as usual. A client is one
socket at the edge; compute restarts without one client's TCP connection
dropping; and later the edge can hold hibernated sessions' state, so even
those survive a compute restart. This is fleet mode's rule made general:
a socket stays on the node that took it, and its events go to the owner.

**Order:** measure first (Caddy, HAProxy, and combd with no proxy on a
1 GB box: "Results", "What a node holds"), then single mode, so the arena
drops Caddy (built 2026-10-20: ~14 KB a connection over TLS, 7.5x less than
behind Caddy; limits and timeouts at the edge; Autobahn and testssl.sh
clean; then rustls's unbuffered API, ~10 KB a connection; next, the socket
kept open across a restart, and HTTP/2 and HTTP/3 behind settings); edge mode after v0.1, when a fleet needs it. HAProxy (C,
buffers only while there's data, TLS buffers freed when idle) is the
stopgap if it gets close to the floor.

## Scaling the fleet: rules that keep it quiet

The fleet must not hit the walls Erlang distribution hits:

| Erlang's wall | Why | combd's rule |
|---|---|---|
| The full mesh forms by itself | connecting to one node connects to all, transitively | Connect only to the node you forward to. Close idle links. |
| Heartbeats grow as N² | every link ticks | Nothing chats on the wire. Liveness is a bucket lease now, SWIM gossip at scale (constant work per node). |
| `global` names need cluster-wide locks | one registry for everything | No registry. Each object's owner is one record, claimed by CAS on that key alone. |
| Head-of-line blocking | one TCP link carries everything | Small frames. Bulk data (D3 replication) is chunked or gets its own stream, or QUIC. |
| All-or-nothing trust | any node can run code anywhere | mTLS with node identities, and a fixed set of message types. |

What changes as the fleet grows:
- **About 100 nodes:** lease timing becomes a cost. A node writes its lease
  every third of `COMBD_LEASE_MS`; 1 s leases at 100 nodes would be 500 S3
  PUTs a second.
- **1,000 to 10,000 nodes:**
  - **Liveness moves to SWIM gossip.** The bucket is touched only when
    ownership changes.
  - **Placement uses rendezvous hashing** over live members, and the edge
    routes straight to an object's home, so forwarding is rare.
  - **Membership is dynamic**, by gossip, bootstrapped from a bucket record.
  - **Writes take the D3 path:** peers confirm, the bucket gets batches.
    A bucket proof per write doesn't scale in cost or in S3's per-prefix
    limits.
  - **The fleet splits into cells** of a few hundred nodes, each with its own
    bucket prefix, to bound blast radius.
- **Worldwide:**
  - **Regional cells with regional buckets.** A node in Sydney can't pay
    a round trip to a bucket in Virginia on every write.
  - **Objects live near their traffic,** like Cloudflare's location hints,
    and can move.
  - **One cross-region hop at most.**
  - **Timeouts are measured per peer**, in TCP's way (smoothed round trip
    plus 4x its variation), never fixed. Safety never depends on a timeout;
    only availability does.

## The finish lines (set 2026-10-15)

The plan grew fast: a process model, supervisors, user pools, environments
per pull request, KMS secrets, an ingress router. Each is a good idea, and
together they're a platform nobody can use yet. So: **no new concepts until
v0.1 runs.** Two finish lines.

**v0.1: one real app, deployed to a fleet.**
1. One app and its manifest: routes by domain and path, a Worker process,
   a Durable Object, and a Postgres pool.
2. `combd deploy` to a 3-node fleet on one bucket.
3. Durable Objects with their own storage, WebSockets, and the pool capped
   across the whole fleet (one manager, members near their callers). (Done
   2026-10-15.)
4. `callers` and `owners` enforced; `call` and `send` between processes.
5. Secrets set with the CLI, encrypted in the bucket. (Done 2026-10-15.)

Waits until after v0.1: supervisors, user-written pool kinds, `enqueue`,
per-PR environments, `fetch` beyond what the JS line needs.

**Languages: any that compiles to Wasm. JS is paused.** (Decided
2026-10-15.) With agents writing most code, the language matters less than
strict config, clear errors, and a small API, and backend JS may have seen
its best days. So apps are Wasm, run by our interpreter or compiled to Rust
by the AOT tier, and the docs cover the popular languages. `jsc/` stays in
the repo, unmaintained; the docs say "JS support coming soon". If JS comes
back, the target is Workers at launch (2017-2018): the `fetch` handler,
`Request`/`Response`/`Headers`, `fetch()`, `URL`, text encoding, base64,
`crypto.subtle` basics, timers, `waitUntil`; not Node.

Where each language stands (what its compiler needs from us):
- **Today:** Rust, C, Go, Zig, AssemblyScript (`langs/`, all three engines).
- **Next, plain linear-memory Wasm:** Swift (official WASI SDK) and C#
  (.NET's WASI build runs Mono inside Wasm: likely 10+ MB and slow cold
  starts; test before promising).
- **With work:** Python. CPython builds for WASI but reads its stdlib from
  disk, and cells have no filesystem, so bundle it or give apps a
  read-only one. An interpreter inside our engine: AOT tier only, tens of
  MB per instance.
- **Blocked on Wasm GC (and exception handling):** Kotlin and Java (Kotlin/
  Wasm, GraalVM, newer TeaVM). That's a managed heap, struct and array
  types, and subtyping in both engines: its own track, after v0.1.

## The catalog: app state in the bucket, a copy on every node (2026-10-15)

Per-request bucket fees were the real cost of running a fleet: every node
renewed its lease in the bucket every 3.3 s (a read and a write), about
$0.14 a node a day on S3 ($14 a day at 100 nodes), and checking deploys by
polling every app's pointer would have been far worse. And the first deploy
design had a real bug (two deploys on two nodes could claim one domain).

So, designed with the user:
- **The bucket is the source of truth, written before anything is
  acknowledged.** The catalog (apps, versions, vars, secrets, encrypted) is
  an append-only log: slot n+1 is written only if empty, so every change
  gets one place in one order with no leader needed for safety. Checks run
  against the log's head and are retried if the slot was taken.
- **Every node keeps a copy in SQLite** (in memory, or on disk for a fast
  restart on the same machine), applies pushes in order, and fetches gaps.
- **Heartbeats go over the wire, not the bucket.** Leases were never part
  of the safety proofs (the ownership spec lets any process claim at any
  time), so this changes no proof: a node wrongly judged dead just loses
  its objects at the next check. A leader (one conditional lease in the
  bucket for the whole fleet) takes the heartbeats and fans out pushes.
- **Object ownership stays per object in the bucket**, touched only on a
  claim or takeover. No node keeps every app or object: nodes load apps on
  demand, and a very large fleet splits into cells (each its own catalog
  and leader) with a small map from tenant to cell.
- **No user data in it.** It's one ordered log: one writer's worth of
  throughput, and every node holds it. User data lives in Durable Objects
  (sharded, with SQLite each) or Postgres. A small capped app KV (flags,
  config) may come later.

Spec: `spec/Catalog.tla` (14.6 million states with snapshots and trimming;
six known-bad versions caught, one of them the first deploy design, and
one the trap that trimming sets: a deleted slot is empty again). The
simulator found a seventh (a reader meeting a trimmed slot); the spec has
it now (58.2 million states). **In the nodes (2026-10-15):** deploys and
vars are on it, a deploy reaches every node in about 25 ms, and two
racing deploys can't share a domain. Heartbeats are over the wire now (no
per-node bucket writes), with a liveness check and a lost node in the
simulator. Secrets are on it too, sealed (a data key per app under a
master key file; write-only API; opened only in a node's memory). Uploads
are batched (10 s or 1 MB while every peer is up: 12 to 56x fewer bucket
writes on the Docker cluster). Postgres pools are fleet-wide (one manager,
named by the catalog; members near their callers; at max the nearest free
one over the wire; tested on three nodes with a real database). The
multi-region demo runs on DigitalOcean (Terraform, continuous deploy).
Reads take a lease from their peers (spec, simulator, and fleet), so a read
no longer waits a round trip to the farthest peer. Next: deploys that never
end a running game (old objects finish on the old version), then
snapshot-and-restore for hot upgrades and moving objects.
ponytail: heartbeats go all to all; a leader or gossip
if fleets get very large.

**The data path's cost, found while pricing this:** D3 uploads a busy
object's writes as fast as the bucket answers (one upload in flight at a
time, 20 to 30 a second), about $10 to $13 a day per object busy all day on
S3. The peers already hold every write before it's acknowledged, so
uploads can wait: per object every 10 s or 1 MB (about $0.04 a day per busy
object), or one combined upload per node every 5 to 30 s (cost by node, not
object). **Done 2026-10-15** (per object; see RESULTS.md, "Upload
batching"). The longer the wait, the longer the newest writes live only on
the two peers. Comes right after the catalog, with the replication spec's
timing checked again. **The node's operator decides:** `COMBD_UPLOAD_MS`
(default 10 s; no cap, but a warning in the logs above 5 minutes; 0 is
today's as-fast-as-it-answers) and `COMBD_UPLOAD_BYTES` (upload early at this much, default 1 MB).
Maybe later a per-app setting in the manifest, shorter than the node's,
never longer. "Never upload" (memory only, durable in the peers alone,
across data centers) is a different mode, not a big number: peers drop
entries only once the bucket has them, so with no uploads they'd keep
every write forever, and a takeover would replay all of it. It needs
snapshots and compaction among the peers, more peers (3 to 5 across
zones), its own spec check, and a loud warning. Later, if wanted.

**Storage behind a small interface, two roles.** The cores only ask for
get, put-if-unchanged, put-if-absent, delete, and list; only the S3 client
under them is hard-wired. Split it: a coordination store (the catalog log,
ownership records, the leader lease: tiny, needs compare-and-swap) and a
data store (object segments and snapshots: big, append-only, cheap bulk
writes). Each gets a backend: S3 or R2 for both by default, Postgres for
coordination, and at very large scale whatever the operator runs. The
startup storage test runs against any backend, so a store that can't
really do compare-and-swap is never used. The catalog code is built on
this from the start; the bucket code moves behind it then.

## The contract: a spec anyone can build, combd one way to run it (2026-10-19)

The patterns here aren't combd's; they're what most backends already are.
A durable object is a service with one owner and its own state, reached by
HTTP, gRPC, or a WebSocket. A ws process is a connection with its own
logic. A pool is shared access to a database. Events can ride SQS,
Postgres, Redis, or anything with at-least-once delivery. So the contract
gets written down on its own, and combd is the reference runtime, not the
only one. An app written to it runs on any runtime that passes the suite.

**What the contract is:**
- **The manifest:** the types (`http`, `ws`, `durable`, `pool`), their
  settings, `callers`, routes, domains, vars, and secrets. Already strict
  and typed: it becomes a published schema.
- **What each type promises:** `durable` has one owner at a time, its
  writes are on disk before anything it said leaves (the output gate), and
  an acknowledged write is never lost; `ws` is one instance per socket,
  events in order, gone when the socket is; `http` keeps nothing between
  requests; `call` answers once, `send` goes only if the sender succeeds.
- **The host API:** `sql`, `call`, `send`, `ws_send`, alarms, ticks,
  `pg_query`, `now_ms`, hibernation. Today it's raw imports with JSON;
  for the contract it becomes one WIT file, so any language's bindings
  come from it.
- **The conformance suite:** black-box tests over HTTP and WebSockets,
  plus the crash and split tests (kill the owner, cut the network), that
  any runtime can run. Most of our integration tests already only talk to
  a server; they're the start of it. The TLA+ specs state the promises
  formally.

**Other ways to run it**, for whoever wants to build them: on AWS, http as
Lambda, a durable object as an ECS task that holds a DynamoDB lease (the
owner), events on SQS; on plain VMs, Docker containers, Postgres advisory
locks for ownership, and Redis streams for events. Each trades something
(a Lambda can't hold a socket, so ws needs a connection service like API
Gateway's), and the suite says what still holds.

**When:** after v0.1. The WIT file and the suite come out of what we
already have; the schema is the manifest code written down.

## Maybe later: combd's own OS, for a hosted combd (an idea, 2026-10-21)

Not for adoption: the open-source combd stays a binary and a container on
any Linux (and a Mac or WSL for development), using what each kernel has
(kTLS, io_uring) where it has it. But a hosted combd, sold as a service,
could boot nodes from an image of our own: a stripped Linux kernel with
combd as its only process, no shell, no Docker, no package manager, read-
only and replaced whole to update (as Talos or Bottlerocket do).
- **Why:** on a 1 GB node, Ubuntu and Docker take ~390 MB before combd
  starts; an image like this needs maybe 30 to 60 MB: ~350 MB more for
  users (tens of thousands more idle ones a node). Boots in a second, has
  little to attack, and nothing to patch but the image.
- **No local disk:** writes are on peers before they're acknowledged and
  in the bucket for good, so an object's SQLite can live in memory and a
  restarted node recovers from its peers and the bucket (the takeover
  path). Fits small-state apps.
- **Admin over SSH,** as `erl -remsh`: the admin API on localhost (or
  mTLS), reached through an SSH tunnel, maybe an SSH server inside combd;
  a terminal view (`combd top`) in place of the web panel.
- **Not a unikernel:** the CPU goes to TCP per packet, not to system
  calls (3%), so leaving Linux would cost its network stack, kTLS, and
  drivers for little.
- **The moat:** if a big cloud runs the open-source combd, we'd still run
  it cheaper per user. (Whether the license should also stop that, as
  Elastic's and Mongo's did, is its own question.)

## Maybe later: realtime objects (guaranteed ticks)

combd is fair, not realtime: every request gets a slice (about 50 us) in
turn, so a due tick waits behind whatever is runnable on its loop (100
ahead is about 5 ms). The arena's run on a shared laptop: 1.4% of ticks
more than 75 ms apart, worst gap 271 ms (most likely the OS pausing us
under a load average near 10, not our scheduler, which would need about
5,000 runnable requests ahead; not yet proven: measure on a quiet Linux
box).

Compiling without yields wouldn't fix it: it only shortens the tick's own
run (already microseconds), and a neighbor that never yields could hold
the loop, and the tick, without limit. Preemption is what caps a
neighbor's delay. If guaranteed ticks are ever wanted, a realtime class:
- `realtime = true` and `tick_budget_ms` on a process in the manifest;
- its objects on their own event loop, pinned to a reserved core (Linux:
  an isolated CPU, maybe `SCHED_FIFO`), never sharing a run queue with
  ordinary requests or other tenants;
- due ticks checked between every slice, not once a round, and first;
- a budget instead of "no yields": a tick past its budget is cut off and
  its object demoted, so a buggy game can't take the core;
- AOT for speed (15 to 39x the interpreter), which needs compile-on-deploy
  first (today only modules built into the binary run compiled).

On a quiet Linux box that typically holds jitter well under 1 ms. Not
needed for the arena, which says plainly it isn't a game engine.

**Stretch: microseconds.** Doable on Linux with the whole stack tuned: an
isolated core (`isolcpus`, `nohz_full`, `rcu_nocbs`); busy-waiting on the
cycle counter instead of sleeping (waking alone can cost 50+ us); no page
faults or allocation in the tick (`mlockall`, cells touched before the
match, per-tick arenas); no system calls in the tick (sends batched
after it); compiled code; `PREEMPT_RT`, and no CPU frequency scaling or
deep sleep states. Tuned machines show worst cases in the low tens of us
(`cyclictest`), single digits with busy polling on isolated cores. Not on
macOS. It doesn't help players over the internet (their round trips wobble
by milliseconds); it pays off for deterministic simulation, fairness
between many matches on one core, and latency-critical work. Proof: a
jitter histogram of tick start times (p50, p99, p99.99, worst) on a quiet
Linux VM, with and without load on the other cores; aim for p99.99 under
10 us.

## Build order (when we do build)

0. **DONE 2026-10-13. Spike: the engine race.** Results in
   [spike/RESULTS.md](spike/RESULTS.md). Our interpreter beat Wasmtime on load
   (~85x), spawn (~25x), crash restart (~3x), memory per cell, latency, binary
   size (~5.5x smaller), and can snapshot mid-call. It lost on raw compute
   (15x to 39x slower). Preemption works: a stuck neighbor costs ~40 us, versus
   ~146 ms with preemption off. Verdict: keep our interpreter, and close the
   compute gap to within 5x of Wasmtime in phase 1. Still to do: add celld
   itself to the scoreboard.
0a. **DONE 2026-10-13. Languages right away.** A small, safe WASI preview 1
   shim, plus the test cell in AssemblyScript, TinyGo, C, and Zig
   (`langs/`). All five languages, including Rust, pass all checks on both
   engines. See [spike/RESULTS.md](spike/RESULTS.md).
0b. **DONE 2026-10-13. JS spike.** QuickJS in Wasm, a Hono-style app, on our
   engine, Wasmtime, Node, and Bun. Interpreted JS can't beat Bun, even native
   QuickJS. **Decision: compile JS instead, and drop QuickJS.** See
   [spike/RESULTS.md](spike/RESULTS.md).
0c. **DONE 2026-10-13. HTTP race against workerd, celld, Bun, and Node.** Per
   core, a Rust cell on combd beat Bun on every route (1.3x to 1.6x), celld by
   2.7x to 5.3x, and workerd by 3.9x to 6.8x, with sub-millisecond p99. See
   [spike/RESULTS.md](spike/RESULTS.md). Still to race: Durable Objects with
   state, cold start over HTTP, and memory per object at scale.
1. **Spec + decision core + simulator** for ownership and the output gate. No
   network yet. **Done 2026-10-14:** TLC checks 4.9 million states, and four
   known-bad versions fail; the core runs 100,000 simulator seeds (10.4M
   acknowledged writes, 167k crashes, 167k freezes) with every invariant
   holding. See [spike/RESULTS.md](spike/RESULTS.md) and [spec/](spec/).
2. **Single node (D1).** HTTP in, route to cell, SQLite, alarms, supervisor,
   hibernation, egress broker, mTLS admin port.
   **Started 2026-10-14:** objects, host SQLite with an authorizer, the output
   gate, crash rollback, and the race against workerd and celld are done (see
   [spike/RESULTS.md](spike/RESULTS.md)). Then alarms (with retries, and
   found again after a restart), eviction of idle objects, one owner per
   object across event loops, and `/_combd/stats`. Then a group commit (one
   fsync for many writes, answers held until covered): hot writes 8.3x
   faster. Then checkpoints off the event loop (the loop never fsyncs now;
   the remaining p99 tail is slow fsyncs on Docker's disk). Next: the spec
   and simulator for D2. Real Linux VMs for honest disk numbers soon; a worldwide set for
   soak tests once D3 works in the simulator.
3. **Fleet (D2).** Bucket leases, CAS ownership, epochs, routing between nodes
   over mTLS. Kill tests.
   **Running 2026-10-14:** three nodes on Silo in Docker; SIGKILL and freeze
   tests keep every object correct, over mTLS, with alarms and eviction.
   **D3, 2026-10-14:** spec (TLC, 39.9M states), core and simulator
   (50,000 seeds), and real nodes (peer wire, on-disk peer store). On the
   cluster D3 roughly doubles writes through a node kill and halves p50.
   **Network splits, 2026-10-14:** 80,000 simulator seeds with splits, and
   cluster cuts (bucket, nodes, both) keep every object correct; three
   availability fixes came out of it.
   Next: handle a peer that lost its disk (spec first), then quiet Linux
   machines for honest numbers. See
   [spike/RESULTS.md](spike/RESULTS.md).
4. **WebSockets** with hibernation. **Done 2026-10-14** on one node: the
   host holds the sockets, idle objects leave memory, a message wakes them.
   Then binary messages and fleet mode: connections stay on the node they
   reached, events go to the owner, and the socket list lives in the
   object's storage, so it survives a takeover. Next: numbers. See
   [spike/RESULTS.md](spike/RESULTS.md).
5. **Bring your own database.** Host-owned pools, Postgres first.
   **Started 2026-10-15:** pooled statements, and pinned handles: a Worker's
   lasts its request, a Durable Object's lasts across messages, with
   transactions, row locks, and advisory locks. Cells wait paused (both
   engines). Then the per-node pools were scrapped for one pool per
   database (above): single node done (growth to max, a line with a wait
   time, lease timeouts, idle stop). Then the manifest replaced env vars
   (typed vars and secrets, process and pool settings, strict checks).
   Next: the process model, then the fleet pool (nearest member,
   cross-node calls, takeover), then TLS, LISTEN/NOTIFY. See
   [spike/RESULTS.md](spike/RESULTS.md).
6. **Fast fleet writes (D3).** Done early, as part of step 3 (see "What
   building taught us").
7. **JavaScript** (our JS-to-Rust compiler, Workers APIs next), then **more
   languages** (Python, Go) using the same cell interface. Not before the core
   is proven.

Not planned: Node.js, V8, KV, R2, Queues, D1 (the Cloudflare product), Workflows,
containers. Most can be built later as cells on top of the core.

## Open questions

- **Stack memory per paused cell.** Each async fiber needs its own stack.
  Reserved virtual memory is cheap but real. How many resident cells per GB?
  The spike answers this.
- **Host SQLite or SQLite inside the guest?** Host is faster and makes write
  capture easy. Guest puts SQLite's C code inside the sandbox too, at some
  speed and memory cost. Start with host, keep guest as a hardening option.
- **Multi-tenant from day one, or single-tenant first?** Recommend: design
  every boundary for hostile tenants, but ship single-tenant first. Later, run
  a separate process per tenant with seccomp/landlock as a second wall.
- **A peer that loses its disk.** Today it must rejoin as a new node. Better:
  a store that knows it's new, so the next owner uses another peer's tail.
  Spec first.
- **Lease and hint tuning under splits.** A node cut off from everything but
  its clients still costs about 3 s, mostly one lease plus routing hints.
- **Message format between cells.** Plain bytes (simple) or WIT types (safer,
  typed). Lean toward WIT types.
## Name and shipping

**Name: combd** (say "comb-dee"). Like a honeycomb: tiny cells packed tight,
light but very strong. The `d` means daemon, like `celld` and `workerd`.
Picked 2026-10-13, replacing the working name "openworkerd" (too close to
Cloudflare's workerd).

Checked free on 2026-10-13. Grab these before anything goes public:

| Where | Name | Status |
|---|---|---|
| Domain | combd.dev | unregistered (.dev registry RDAP: 404) |
| npm | `combd` | free |
| npm org | `@combd` | could not check from a script, try when creating |
| crates.io | `combd` | free |
| GitHub | `combd` org | free |
| PyPI | `combd` | free (for a future Python SDK) |

Still to do: a USPTO trademark search.

**License: Apache-2.0** (decided 2026-10-13). Same as Wasmtime, celld, and
workerd. Includes a patent grant.

### Shipping through npm

npm can ship a Rust binary. esbuild, Biome, and Turborepo all do it this way:

- `combd`: a tiny package with a short JS launcher. It lists the platform
  packages as `optionalDependencies`.
- `@combd/darwin-arm64`, `@combd/linux-x64`, `@combd/linux-arm64`: each holds
  one prebuilt binary and sets `os` and `cpu`, so npm only downloads the one
  that matches.
- `npx combd dev` just works. No Rust toolchain needed.

npm is a supply-chain target, so publish with care:
- Only from CI, using npm trusted publishing (OIDC) with `--provenance`. No
  long-lived npm tokens.
- 2FA on every maintainer account.
- No `postinstall` scripts. The launcher only picks the right binary.
- Sign the same binaries for GitHub Releases (with attestations), so
  `curl combd.dev/install.sh | sh` and a container image get the exact same
  bytes.

The Rust SDK for writing cells (the `#[cell]` macro and WIT bindings) goes on
crates.io as `combd`.

## Prior art

- **celld** (Deno, now Cloudflare): the storage and coordination design.
- **workerd** (Cloudflare): input and output gates, the Durable Objects model.
- **Erlang/OTP and the BEAM:** reductions, supervisors, let it crash.
- **Lunatic:** an Erlang-style runtime on Wasm. Activity slowed in recent years. Worth reading for
  what worked and what didn't.
- **Golem (Golem Cloud):** durable Wasm workers on a Wasmtime fork, Rust,
  open source. Closest thing to this idea. It makes whole executions durable
  by logging every host call and replaying it. We differ: SQLite state per
  cell, a bucket as the only coordinator, and BEAM-style scheduling. Study it
  closely before building.
- **Spin (Akamai), wasmCloud:** Wasm serving, no Durable Objects.
- **Litestream / LTX:** SQLite replication to a bucket.

## Sources

- Cloudflare blog: https://blog.cloudflare.com/deno-joins-cloudflare/
- Deno blog: https://deno.com/blog/cloudflare
- Simon Willison: https://simonwillison.net/2026/Oct/9/deno-is-joining-cloudflare/
- celld repo: https://github.com/denoland/celld (read at commit 90b4301, 2026-10-07)
- celld guarantees, testing, security docs: `reference/celld/docs/`
- WASI 0.3: https://wasi.dev/releases/wasi-p3
- Fermyon joins Akamai: https://www.fermyon.com/blog/fermyon-joins-akamai
