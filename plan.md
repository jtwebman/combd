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

**Decision:** lean toward our own interpreter. The spike races it against
Wasmtime so real numbers decide. If we ever need raw speed, add a JIT tier for
hot cells later, the way BEAM did.

## Build order (when we do build)

0. **Spike (1 to 2 weeks): the engine race.** A minimal interpreter (core Wasm,
   enough to run one Rust cell) vs the same cell on Wasmtime. Run 10,000 cells
   with a few doing `loop {}`. Measure: do the others stay fast? Memory per
   paused cell? Restart time after a crash? Compute speed gap? This proves or
   kills both the Erlang idea and our own engine with real numbers. Also run
   the same app on celld to fill in the scoreboard above.
1. **Spec + decision core + simulator** for ownership and the output gate. No
   network yet.
2. **Single node (D1).** HTTP in, route to cell, SQLite, alarms, supervisor,
   hibernation, egress broker, mTLS admin port.
3. **Fleet (D2).** Bucket leases, CAS ownership, epochs, routing between nodes
   over mTLS. Kill tests.
4. **WebSockets** with hibernation.
5. **Fast fleet writes (D3).** Peer proofs, only after D2 is solid.
6. **More languages** (Python via componentize-py, Go via TinyGo, JS via
   StarlingMonkey) using the same WIT file. Not before the core is proven.

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
