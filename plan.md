# combd: plan

Status: thinking and research only. Nothing is built yet.
Updated: 2026-10-13

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
| **D3: fleet, peer proof** | 1 or 2 peers fsynced it | fast (~25 ms in celld's lab) | nothing acknowledged |

D1 is honest and simple, and good for many apps. D3 is where celld's hardest
code lives (node logs, sealing, takeover recovery). We earn it last.

**Capturing writes:** celld diffs SQLite's WAL file, Litestream style. A simpler
option for us: a custom SQLite VFS in the host, so we see every page write
directly and turn it into a log entry. Decide in the D1 spike.

## Fuzzing: catch the bugs normal tests miss

The TinyGo bug (an `if` right after a stack op left the stack pointer one
slot high) was found by luck: one compiler happened to emit that pattern.
Fuzzers find those on purpose. Next up, after the HTTP race:

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
4. **Kill tests on real machines:** SIGKILL mid-write and delete the local disk,
   freeze a node, cut it off from the bucket, throttle the bucket to 429s.
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

**Compiling never blocks "ready".** A cell has two states:
- **Ready:** it serves traffic on the interpreter within microseconds of
  deploy.
- **Fast:** it switches to machine code once its artifact exists.

Nodes never compile on the request path, and a node waking a cell only loads a
finished artifact. `combd deploy --wait-aot` can hold a rollout until
artifacts exist for every CPU in the fleet, then shift traffic.

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
   network yet.
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
4. **WebSockets** with hibernation.
5. **Bring your own database.** Host-owned pools, Postgres first.
6. **Fast fleet writes (D3).** Peer proofs, only after D2 is solid.
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
