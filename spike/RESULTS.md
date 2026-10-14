# Phase 0 results: the engine race

Date: 2026-10-13. Machine: Apple M4 Max (14 cores: 10 performance, 4
efficiency), 36 GB RAM, macOS 26.7.1. Linux runs use Docker on the same Mac:
Linux 7.0 (LinuxKit), ARM64, 4 KB pages, 8 CPUs and 16 GB given to the VM.
Rust 1.94.1. Wasmtime 47.0.4 (49 needs Rust 1.96).

Same Rust cell (`guest/`), compiled once to `wasm32-unknown-unknown`, run on:
- **interp**: our own interpreter, about 1,200 lines of safe Rust
  (`#![forbid(unsafe_code)]`). `wasmparser` validates, we do the rest.
- **wasmtime**: Wasmtime with default settings and fuel yields.
- **wt-pool**: Wasmtime with its pooling allocator, its fast path for many
  short-lived instances. This is the fair comparison.
- **native**: the same Rust code compiled normally, as a baseline.

Run it:
- macOS: `cd guest && cargo build --release --target wasm32-unknown-unknown`,
  then `cd spike && cargo run --release -- all`.
- Linux: `docker build -f spike/Dockerfile -t combd-spike .` from the repo
  root, then `docker run --rm combd-spike`. All tests run during the build.

Numbers are from the latest run. Earlier runs gave the same picture.

## Scoreboard: Linux

| Measure | interp (ours) | wasmtime | wt-pool | Winner |
|---|---|---|---|---|
| Load a module | **0.39 ms** | 26 ms | 25 ms | ours, ~65x |
| Spawn a cell | 3.9 us | 3.9 us | **1.6 us** | wt-pool |
| Spawn + first request | **8.8 us** | 44.5 us | 14.2 us | ours |
| Crash, then fresh cell serves | **6.8 us** | 25.3 us | 9.4 us | ours |
| Memory, idle cell | 8.5 KB | 6.0 KB | **5.7 KB** | wt-pool |
| Memory, cell paused mid-loop | **21.4 KB** | 88.5 KB | 89.4 KB | ours, ~4x less |
| 10,000 paused cells, whole process | **212 MB** | 873 MB | 884 MB | ours |
| Hello, quiet, p50 / p99 | **7 / 80 us** | 140 / 2,094 us | 125 / 690 us | ours |
| Hello, stuck neighbor, p50 / p99 | **24 / 183 us** | 197 / 2,295 us | 55 / 246 us | ours |
| Hello, heavy neighbor, preemption off, p50 | 62,585 us | n/a | n/a | |
| fib 30 | 26.8 ms | **4.4 ms** | (same) | wasmtime |
| sieve 5,000,000 | 167 ms | **15.9 ms** | (same) | wasmtime |
| sort 1,000,000 | 334 ms | **18.9 ms** | (same) | wasmtime |

wt-pool's "quiet" row runs first in its process, so it pays for first touch
of its pool slots. Its "stuck neighbor" row reuses warm slots, which is why
it looks better than "quiet".

## Scoreboard: macOS

| Measure | interp (ours) | wasmtime | wt-pool | Winner |
|---|---|---|---|---|
| Load a module | **0.27 ms** | 21.6 ms | 22.3 ms | ours, ~80x |
| Spawn a cell | **0.6 us** | 24.8 us | 18.8 us | ours |
| Spawn + first request | **3.4 us** | 31.1 us | 25.4 us | ours |
| Crash, then fresh cell serves | **3.4 us** | 16.3 us | 18.4 us | ours |
| Memory, idle cell | **20.5 KB** | 38.1 KB | 37.9 KB | ours |
| Memory, cell paused mid-loop | **68.5 KB** | 86.7 KB | 86.5 KB | ours |
| Hello, quiet, p50 / p99 | **11 / 57 us** | 33 / 1,752 us | 30 / 1,503 us | ours |
| Hello, stuck neighbor, p50 / p99 | **24 / 85 us** | 62 / 1,395 us | 56 / 1,141 us | ours |
| fib 30 / sieve / sort | 26 / 160 / 347 ms | **2.4 / 13.9 / 14.6 ms** | (same) | wasmtime |
| Binary, stripped (gzipped) | **1.31 MB (0.56 MB)** | 7.55 MB (3.04 MB) | (same) | ours |
| Snapshot a cell mid-call | **works** | not possible | not possible | ours |

Native Rust (macOS): fib 1.8 ms, sieve 9.6 ms, sort 12.7 ms.

On macOS the pooling allocator barely helps Wasmtime. On Linux it helps a lot:
spawn, first request, and crash restart all get 2x to 3x faster, and its
latency rows improve. That is the fair version of Wasmtime.

celld itself is not in these tables yet. Running it means installing and
running their binary, which needs a yes first.

## Fairness, the Erlang test

10,000 cells stay resident. Each thread serves one hello at a time to its
cells. In the "stuck neighbor" rows, each thread also hosts one cell in
`loop {}`, and every new hello waits behind one full neighbor slice (the worst
case). Slices: about 20 us for ours, about 45 us for Wasmtime.

A stuck cell costs its neighbors about one slice. Turn preemption off, and a
neighbor doing a long sort costs ~63 ms (Linux) to ~80 ms (macOS) per hello,
thousands of times worse. That is the case for BEAM-style scheduling,
measured. (Wasmtime's yield interval is fixed per store, so only our engine
can switch it off per call for that row.)

Max latencies were sometimes several ms on every engine. That's the OS
parking a busy thread now and then. Pinned threads on a real server should
cut that.

## Making our interpreter faster

Instruction counts showed that 22% to 39% of all ops were `local.get`, and
another 10% to 15% were `local.set` or `local.tee`. Each op was already cheap
(under 1 ns, about 4 cycles). The gap is that a stack interpreter runs many
more ops than a JIT runs machine instructions.

| Change | fib 30 | sieve | sort |
|---|---|---|---|
| Phase 0 baseline | 40 ms | 270 ms | 570 ms |
| Stack as a slice with an index, one bounds check per memory access | 35 ms | 255 ms | 480 ms |
| Fused ops for the hottest pairs (see below) | **27 ms** | **165 ms** | **335 ms** |
| **Total speedup** | **1.5x** | **1.6x** | **1.7x** |

The fused ops:
- two `local.get` at once,
- `i32.add` / `i32.shl` by a constant,
- local + constant,
- add then `local.set` / `local.tee`,
- `local.set` / `local.tee` then `local.get`,
- an `i32` compare plus `br_if` as one branch.

A fused op never spans a branch target.

Gap to Wasmtime went from 15x to 39x, down to **11x to 24x** (macOS). The next
big step is a register-based design: treat locals and stack slots as operands,
so `local.get` mostly disappears. That is phase 1 work, still aiming at
"within 5x".

## Scaling past 10,000 cells (Linux)

| Cells | Paused, per cell | Whole process |
|---|---|---|
| 10,000 | 21.4 KB | 212 MB |
| 100,000 | 21.4 KB | 2.1 GB |
| 200,000 | 21.4 KB | 4.2 GB |

It scales in a straight line. Two walls showed up on the way:
1. **glibc's mmap count.** glibc only hands out 65,536 separately mapped
   blocks by default. Past that, cell buffers came from its heap and idle cells
   jumped from 8.5 KB to 52 KB. Fix: `MALLOC_MMAP_MAX_=4000000` (in the
   Dockerfile).
2. **Linux's map limit.** `vm.max_map_count` is 262,144 here. Each cell's
   buffer is one mapping, so one process tops out near 250,000 cells. Past
   that, either raise the setting or put all cells in one big reserved region.
   The second is what Wasmtime's pool does, and it needs `unsafe`, so it
   belongs in the small audited FFI crate.

At 21.4 KB each, a million paused cells is about 21 GB. That fits on an
ordinary 32 to 64 GB server, once the map limit is handled.

## Languages (step 0a)

The same test cell in five languages, each built by `langs/Dockerfile`, run
on both engines with five checks each:
- `hello`: a host call,
- `fib 25`: correctness,
- `print`: stdout through the WASI shim,
- `spin`: must yield,
- `panic`: must crash cleanly.

| Language | Size | Load (ours / wasmtime) | fib 30 (ours / wasmtime) | Checks |
|---|---|---|---|---|
| Rust | 27.6 KB | 0.86 / 23.0 ms | 27.2 / 2.3 ms | 5 of 5 on both |
| AssemblyScript | 9.5 KB | 0.15 / 6.8 ms | 49.7 / 6.7 ms | 5 of 5 on both |
| C (wasi-libc) | 23.8 KB | 0.33 / 20.8 ms | 33.1 / 2.5 ms | 5 of 5 on both |
| Go (TinyGo) | 208.8 KB | 2.04 / 174.7 ms | 37.9 / 2.6 ms | 5 of 5 on both |
| Zig | 41.2 KB | 0.54 / 35.1 ms | 31.5 / 2.6 ms | 5 of 5 on both |

- **Our interpreter ran four new compilers' output with zero changes.** That
  includes TinyGo's runtime and Zig's standard library.
- **Loading is 20x to 85x faster than Wasmtime.** Go is the extreme: 2 ms vs
  175 ms, because Wasmtime compiles the whole module to machine code first.
- **Compute is 7x to 15x behind Wasmtime** on these builds.
- **The WASI shim is small and safe** (`spike/src/host.rs`, shared by both
  engines):
  - stdout and stderr go to a 64 KB capped buffer,
  - the clock is rounded to 1 ms (timing attacks),
  - randomness comes from the OS,
  - args and env are empty,
  - `proc_exit` crashes the cell,
  - every `path_*` call answers "not permitted",
  - anything else answers "not supported".

  Zig alone imports 27 WASI functions, mostly file-system calls its standard
  library pulls in. None of them get real access.

## JavaScript (step 0b)

QuickJS (quickjs-ng 0.17.0) compiled to Wasm with a small C wrapper
(`langs/js/`), running a Hono-style app in plain JS: a router with path
params, `JSON.parse` and `JSON.stringify`, three API routes. Same requests
(`langs/js/requests.txt`) on four runtimes, all in Linux Docker on the same
Mac. Every runtime returned byte-identical answers. Node 24.21 (V8) and Bun
1.4.2 (JavaScriptCore) run the same `app.js` (`langs/js/bench.mjs`).

Warm, per request (lower is better):

| Request | ours (QuickJS on our interpreter) | QuickJS on Wasmtime | Node (V8) | Bun (JSC) |
|---|---|---|---|---|
| GET /api/users/42 | 253 us | 8.4 us | 1.1 us | **0.5 us** |
| GET /api/users (20 users) | 997 us | 45.5 us | 1.9 us | **1.7 us** |
| POST /api/orders (20 items) | 1,453 us | 46.0 us | 4.8 us | **3.4 us** |
| fib 25 | 190,029 us | 7,830 us | 457 us | **300 us** |

Cold start and memory:

| | ours | QuickJS on Wasmtime | Node (V8) | Bun (JSC) |
|---|---|---|---|---|
| Cold, no snapshot | 9,666 us | 477 us | 302 us (new vm context) | **174 us** (new vm context) |
| Cold, from snapshot after init | 772 us | n/a | n/a | n/a |
| Memory per instance | 410 KB | 410 KB | 179 KB per context | 73 KB per context |

The Node and Bun memory numbers are for a `vm` context, not a full isolate,
so they are a lower bound. Real tenant isolation in V8 means an isolate per
tenant (that is what workerd does). Testing workerd and celld directly is the
next step.

**Verdict: JS on our interpreter is not viable.** It is 230x to 525x slower
than V8 per request, because QuickJS is an interpreter running on our
interpreter. A simple GET at 253 us is about 4,000 requests per second per
core.

What does work:
- **QuickJS on Wasmtime is viable for typical Workers:** 8 to 46 us per API
  request. That's 8x to 24x behind V8, but still microseconds. It also cold
  starts in 0.48 ms with no snapshot at all.
- **Snapshot after init helps a lot on our engine** (12x faster cold start),
  but not enough to catch Wasmtime.
- **Snapshots don't save memory yet.** A copy costs the same ~410 KB as a
  fresh cell, because we copy every page. Sharing pages copy-on-write needs
  the audited memory crate.

What this means for the plan:
1. JS cells run on the Wasmtime tier by default. Our interpreter stays the
   default for Rust, C, Zig, Go, and AssemblyScript cells.
2. The gate for our own JS engine changes. It must beat QuickJS on Wasmtime
   (8 to 46 us), and it should be measured against V8 and Bun (0.5 to 5 us),
   not against QuickJS on our interpreter.
3. The faster interpreter (register-based) matters even more now. Every
   language, and any future JS engine on our VM, rides on it.

## Interpreter round 2: a register design

The JS spike showed every language rides on interpreter speed, so we spent a
few hours on it. Op counts first: about a third of all ops only moved values
between locals and the stack. So the translator now works like V8's Ignition
and wasmi:
- `local.get` and constants become virtual stack entries and emit nothing.
- Math, compares, loads, stores, globals, and `select` read their operands
  straight from frame slots.
- `local.set` usually redirects where the previous op writes, instead of
  copying.
- A compare and the `br_if` or `if` that uses it become one op.
- The runtime stack pointer is only updated when something actually reads it.

Anything not converted yet still runs as a stack op. The translator writes the
virtual values out first, so the two paths mix safely.

| | Phase 0 | Round 1 (fusion) | Round 2 (register) | Wasmtime |
|---|---|---|---|---|
| fib 30 (macOS) | 40 ms | 27 ms | **22 ms** | 2.5 ms |
| sieve (macOS) | 270 ms | 165 ms | **106 ms** | 14 ms |
| sort (macOS) | 570 ms | 335 ms | **212 ms** | 15 ms |
| JS GET /api/users/42 (Linux) | n/a | 253 us | **201 us** | 8.4 us |
| JS POST /api/orders (Linux) | n/a | 1,453 us | **969 us** | 46 us |
| JS fib 25 (Linux) | n/a | 190 ms | **148 ms** | 7.8 ms |

Since phase 0: **1.8x to 2.7x faster** on Rust compute, 1.3x to 1.5x on JS.
Wasmtime is still 6x to 15x ahead on compute (Linux) and about 20x ahead on
JS.

**One bug, found and fixed.** TinyGo's `hello` crashed. Two tools found it:
- a trace of every write to locals, globals, and memory in the old stack mode
  and the new register mode, diffed to the first difference;
- fixed randomness, so the two runs could be compared at all.

The cause: an `if` or `br_if` right after a stack op left the runtime stack
pointer one slot high. Two regression tests in Wasm text format now guard it.
We confirmed they fail without the fix.

**What it means:** each op is already cheap, and what's left is the cost of a
safe-Rust `match` loop. More fusion gives a few percent at a time. The next big
jump needs machine code: an AOT tier (see the plan).

## Our own AOT compiler: Wasm to safe Rust (macOS)

`aot-gen` (`spike/src/aot_gen.rs`, about 600 lines) compiles a Wasm module into
plain safe Rust, and rustc + LLVM turn it into machine code. No Wasmtime, and
no compiler of our own for each CPU.
- Every Wasm local and stack slot becomes a `let mut` variable.
- Wasm blocks and loops become Rust labeled blocks and loops.
- Traps are `Err` values passed up with `?`.
- `call_indirect` goes through a generated `match` per signature.
- Call depth is counted, so deep recursion traps cleanly instead of
  overflowing the host's stack.

The output has no `unsafe`, so rustc, not our generator, guarantees a cell
can't touch memory outside its own. Build it with `spike/aot.sh`.

The Rust cell became 13,500 lines of Rust. QuickJS became 432,000 lines. rustc
took 3 min 40 s and 3 GB for both: fine at deploy time, never at request time.
Both compiled cells passed every check on the first run, and the
`aot_matches_interp` test keeps it that way.

| | Our interpreter | **Our AOT** | Wasmtime | Native Rust |
|---|---|---|---|---|
| fib 30 | 22 ms | **2.2 ms** | 2.6 ms | 1.5 ms |
| sieve 5,000,000 | 106 ms | **12.4 ms** | 14.1 ms | 9.6 ms |
| sort 1,000,000 | 212 ms | **24.5 ms** | 14.4 ms | 12.3 ms |

JS (QuickJS), per request:

| | Our interpreter | **Our AOT** | Wasmtime | Node | Bun |
|---|---|---|---|---|---|
| GET /api/users/42 | 182 us | **7.7 us** | 8.1 us | 1.1 us | 0.5 us |
| GET /api/users | 644 us | **35 us** | 42 us | 1.9 us | 1.7 us |
| POST /api/orders | 879 us | **41 us** | 43 us | 4.8 us | 3.4 us |
| fib 25 | 137 ms | 31 ms | **7.2 ms** | 0.46 ms | 0.30 ms |
| Cold start | 586 us (snapshot) | **53 us (snapshot)** | 477 us | 302 us | 174 us |

(Node and Bun numbers are from the Linux runs above; the rest are macOS.)

What it shows:
- **8x to 10x faster than our interpreter,** and faster than Wasmtime on fib,
  sieve, and every JS API route.
- **Cold start from a snapshot is 53 us,** 3x faster than Bun and 6x faster
  than Node.
- **Memory-heavy code is behind Wasmtime** (sort 1.7x). Every memory access
  checks bounds and reloads the memory pointer, because LLVM can't prove the
  buffer and the cell state don't overlap. Wasmtime keeps the base in a
  register and uses guard pages. Fixes: pass memory as its own argument, then
  guard pages through the audited `unsafe` crate.
- **Recursive JS is 4x behind Wasmtime** (fib 25). Each call zeroes every local
  in QuickJS's giant interpreter function. Fix: only zero what's read before
  it's written.
- **A fresh JS cell is 1.6 MB, a snapshot copy 413 KB.** The runtime grows
  memory with `resize`, which touches every new page. The interpreter's lazy
  growth fixes that.
- **Still 7x to 18x behind V8 per JS request.** That's QuickJS being an
  interpreter, now running at machine-code speed. A JS engine of our own is the
  only way past it.
- **A compiled cell can't pause mid-call yet.** A stuck loop is killed cleanly
  at a CPU limit instead of being paused. Pausing needs a stack per cell or
  deopt to the interpreter (see the plan).

## Racing Bun: compiled handlers (Linux)

QuickJS can't win on JS throughput. Native QuickJS (gcc `-O2`, computed
gotos, `langs/js/Dockerfile.native`) is the ceiling, and it loses to Node and
Bun:

| | Native QuickJS | Our AOT QuickJS | Node | Bun |
|---|---|---|---|---|
| GET /api/users/42 | 2.9 us | 6.3 us | 1.1 us | 0.5 to 0.9 us |
| GET /api/users | 15.6 us | 29.6 us | 1.9 to 2.0 us | 1.7 to 2.4 us |
| POST /api/orders | 22.5 us | 36.5 us | 4.8 to 4.9 us | 3.4 to 3.9 us |
| fib 25 | 4.8 ms | 11.9 ms | 0.43 ms | 0.32 ms |

So we wrote the same three routes in Rust (`guest/src/lib.rs`, `api` module:
path routing and serde), with answers byte-for-byte equal to the JS app. The
test `rust_api_matches_js` checks that. All runs are in the same Docker VM;
Bun and Node ranges are from two separate runs.

| | Native Rust | **Rust on combd AOT** | Rust on Wasmtime | Bun (JS) | Node (JS) |
|---|---|---|---|---|---|
| GET /api/users/42 | 0.17 us | **0.46 us** | 1.42 us | 0.5 to 0.9 us | 1.1 us |
| GET /api/users | 0.79 us | **1.89 us** | 3.14 us | 1.7 to 2.4 us | 1.9 to 2.0 us |
| POST /api/orders | 2.40 us | **4.71 us** | 6.92 us | 3.4 to 3.9 us | 4.8 to 4.9 us |

**A sandboxed, preemptible, crash-safe Rust cell on combd ties or beats Bun on
two of the three routes, and beats Node on all three.** It's 3x faster than the
same cell on Wasmtime. Bun still wins the JSON-parsing route by about 1.2x to
1.4x.

### Where our overhead goes (macOS)

We turned each safety feature off in turn (`COMBD_AOT_EXPERIMENT`, an
experiment-only switch that never ships):

| Variant | GET user | GET users | fib 30 | sieve | sort |
|---|---|---|---|---|---|
| Full safety, before tuning | 0.64 us | 2.21 us | 2.2 ms | 11.7 ms | 24.8 ms |
| No CPU ticks | 0.57 us | 2.20 us | 2.2 ms | 10.6 ms | 22.1 ms |
| + no stack checks | 0.60 us | 2.19 us | 1.4 ms | 10.3 ms | 23.8 ms |
| + no bounds checks (unsafe) | 0.41 us | 1.51 us | 1.4 ms | 8.7 ms | 15.2 ms |
| Native Rust | 0.24 us | 0.91 us | 1.8 ms | 10.6 ms | 13.5 ms |
| **Full safety, after tuning** | **0.53 us** | **1.81 us** | **1.9 ms** | **10.5 ms** | **20.0 ms** |

Bounds checks cost the most (about 30%). The stack check was 36% on
call-heavy code. Ticks were about 10%. The tuning, all still safe:
- **Results come back in registers.** `Trap` is one byte now, so
  `Result<u64, Trap>` fits in two registers.
- **Memory is its own argument.** LLVM keeps its pointer and length in
  registers instead of reloading them from the cell state.
- **The stack budget rides in a register.** Each function subtracts a
  conservative frame estimate. Big frames also check the real stack.
- **Fuel stays in a local** and is saved only around calls, and only in
  functions that have loops.
- **One bounds check per access, not two.** Snapshot copies skip zero chunks
  with a vectorized OR-fold.

What's left between us and native, biggest first:
1. **Bounds checks (about 30%).** The fix is guard pages: reserve address
   space, map only the live memory, and turn a fault into a trap. This is what
   Wasmtime and V8 do. It needs `unsafe` and a signal handler, so it belongs
   in the small audited crate.
2. **The guest's own code.** Rust compiled to Wasm uses dlmalloc and no SIMD.
   Ideas:
   - an opt-in per-request arena allocator in the combd Rust SDK (a bump
     pointer, reset after each request),
   - Wasm SIMD support in the generator.
3. Small fixed costs per call and per loop, already mostly squeezed.

## The HTTP race: combd vs workerd, celld, Bun, and Node (Linux)

The real competitors are servers, so this race is HTTP end to end
(`bench/http/run.sh`).
- **Same app everywhere:** `langs/js/app.js` plus a standard Worker `fetch`
  handler, on workerd 1.20260929.1, celld v0.6.2, Bun 1.4.2, and Node 24.21
  (its own `http` server, its best case).
- **combd** runs the Rust cell (same routes, byte-identical answers) and the
  JS cell (QuickJS), both on our AOT tier.
- **Fairness:** every server gets one CPU core in Docker. `wrk` runs from its
  own container: 32 connections, 3 s warmup, 10 s per route.

Requests per second per core (higher is better), and p99 latency:

| Server | GET /api/users/42 | GET /api/users | POST /api/orders | p99 |
|---|---|---|---|---|
| **combd, Rust cell** | **225,350** | **169,508** | **104,413** | **0.23 to 0.40 ms** |
| Bun | 138,062 | 128,785 | 79,085 | 1.8 to 4.3 ms |
| Node | 77,685 | 67,703 | 48,097 | 1.1 to 1.5 ms |
| combd, JS cell (QuickJS) | 72,494 | 26,130 | 22,635 | 1.5 to 3.8 ms |
| celld | 42,619 | 37,803 | 38,324 | 3.1 to 4.9 ms |
| workerd | 33,207 | 29,496 | 26,580 | 600 to 990 ms |

- **A sandboxed, preemptible Rust cell on combd beats Bun on every route**
  (1.3x to 1.6x), with 5x to 10x better tail latency.
- **It beats celld by 2.7x to 5.3x and workerd by 3.9x to 6.8x.**
- **The QuickJS cell beats celld and workerd on the simple route** (about 2x)
  but loses on the JSON-heavy ones. That's the QuickJS ceiling again: JS needs
  its own compiler.
- workerd's p99 is very high under this load on one core. We used it as
  shipped and did not tune it.
- `wrk` reported a few socket timeouts on several servers (combd 31, Bun 9,
  workerd 1, out of millions). Not investigated yet.

**The front door was most of the battle.** The first run used one thread per
connection: 46k req/s and a 75 ms p99, because 32 threads were fighting over
one core. A `mio` event loop with the cell on the same thread (no context
switch per request) took it to 212k req/s and a 0.25 ms p99.

**The arena allocator** (opt-in, per request, in the Rust cell) cut the
JSON-parsing route from 4.5 to about 4.0 us on its own (12%).

**A leak, found by let-it-crash.** In the first event-loop run, wrk saw exactly
one failed response on each of two routes. The trap log showed why: the cell
hit its 1 GiB memory limit and trapped. The test calling convention never
freed answers, so after about a million list requests the memory was full. One
request failed and a fresh cell took over, which is let-it-crash working as
designed, but it was still a bug.

The fix: cells can now export `release(ptr, len)`, which the host calls after
reading each answer. All six language cells have it. The test
`answers_are_released` fails without the fix. The rerun: **zero traps and
zero failed responses across 4.9 million requests.**

## Guard pages instead of bounds checks

Behind the `aot-guard` feature, each cell reserves 4 GiB + 64 KiB of address
space (no RAM) and maps only its live memory read-write. Loads and stores in
generated code have no bounds checks. A Wasm address is a u32 plus a static
offset under 64 KiB, so it always lands inside the reservation; past live
memory it hits a guard page.

A small C shim (`spike/aot/src/trap.c`, sigsetjmp/siglongjmp) turns that fault
into a trap, but only when the faulting address is inside the reservation of
the cell running on that thread. Any other fault goes to the previous handler,
so real host crashes still crash. `guard.rs` and `trap.c` hold the only
`unsafe` code; generated code stays `forbid(unsafe_code)`. Offsets of 64 KiB
or more keep an explicit check.

Two bugs found on the way, both by tests:
1. **An `unsafe` bug.** `Mem::new` used struct-update syntax, so a temporary
   `Mem` was dropped and unmapped the reservation it had just made. Every
   cell crashed at once. Fixed by building it in place.
2. **A slow trap setup.** `sigsetjmp(buf, 1)` saved the signal mask with a
   system call on every call, so small requests got 2x slower. Installing the
   handler with `SA_NODEFER` means the mask never needs restoring, so
   `sigsetjmp(buf, 0)` is enough.

The test `out_of_bounds_traps_on_every_engine` reads far past a cell's memory
on the interpreter, Wasmtime, and AOT. Under AOT it goes through the fault
handler 100 times in a row, and the host carries on each time.

In-process, Linux (us per request):

| | Checked bounds | **Guard pages** | Native Rust |
|---|---|---|---|
| GET /api/users/42 | 0.46 | **0.46** | 0.17 |
| GET /api/users | 1.89 | **1.54** | 0.79 |
| POST /api/orders | 4.71 | **3.54** | 2.40 |

On macOS, sort went from 20.5 to 15.4 ms (native: 12.0). The QuickJS cell got
21% to 25% faster per request, and 39% faster on fib 25. One regression: sieve
is slightly slower with guards (12.9 to 14.1 ms vs 11.9 ms checked). Our
guess, not yet verified, is that raw-pointer access stops LLVM from optimizing
that byte loop as well as slices allow.

HTTP, one core per server, same session (req/s):

| | combd Rust cell | Bun | combd JS cell |
|---|---|---|---|
| GET /api/users/42 | **202,593** | 153,240 | 91,103 |
| GET /api/users | **189,972** | 141,287 | 30,490 |
| POST /api/orders | **116,740** | 84,757 | 26,913 |

The Rust cell beats Bun by 1.32x to 1.38x on every route, and the POST route
(JSON parsing) now leads by the widest margin. One outlier to recheck: the
first route showed a 27 ms p99 in this run, against 0.23 ms in the last. The
other routes stayed under 0.4 ms.

## JavaScript through jsc, our JS-to-Rust compiler

QuickJS is gone. `jsc` (`jsc/`) parses `langs/js/app.js` with oxc and writes
a Rust crate that builds to an ordinary Wasm cell, so the AOT tier gives it
yields, guard pages, and let-it-crash. Its runtime (`jsc/rt`) has a dynamic
`Value`, insertion-ordered objects, JS number printing, JSON, and regex
(regress). The compiled app answers every request byte for byte like Node
(`js_matches_node`) and passes all five language checks on the interpreter
and the AOT tier. Anything outside the subset stops the build with a line
number.

What made it fast, in order of effect:
- **A size-class allocator** in the cell (free lists, no system malloc on the
  hot path).
- **Typed twins for number code.** A function that only does number math also
  gets an `f64` version. fib(25), native: 1,288 us to 253 us (Node: 466 us).
- **Faster JSON:** escape checks 8 bytes at a time, no copy for plain strings,
  shared keys, an exact fast path for short decimals. Orders: 10.2 to 7.2 us.
- **Less copying:** operators and parameters borrow instead of copy, small
  memory copies skip memmove, string index math scans only what it needs.
- **Inline caches** for `o.k`, and direct calls for built-in methods.

In-process, macOS, AOT with guard pages (us per request, best of 5):

| | combd JS cell | combd Rust cell | Node 24 (V8) |
|---|---|---|---|
| GET /api/users/42 | 1.6 | 0.55 | 0.7 |
| GET /api/users | 3.8 | 1.34 | 1.6 |
| POST /api/orders | 7.1 | 3.5 | 4.4 |

Natively, the same compiled JS runs users/42 in 0.71 us, level with Node.
The AOT layer costs about 2.3x on this code. Most of that is the wasm32
calling rules (a 16-byte `Value` goes through memory, not registers), plus
fuel ticks and stack checks (about 20% together, measured by turning them
off).

HTTP, Linux (Docker on the same Mac), one core per server, 32 connections
(req/s):

| | combd Rust | **combd JS** | Bun 1.4 | Node 24 | celld | workerd |
|---|---|---|---|---|---|---|
| GET /api/users/42 | 199,801 | **153,429** | 168,622 | 76,300 | 44,808 | 35,778 |
| GET /api/users | 182,970 | **103,279** | 151,073 | 72,819 | 40,054 | 32,092 |
| POST /api/orders | 114,357 | **66,907** | 87,949 | 50,717 | 38,914 | 28,280 |
| p99, worst route | 0.51 ms | **1.61 ms** | 2.37 ms | 15.2 ms | 6.3 ms | 1.16 s |

**The JS cell beats Node on every route (1.3x to 2x), and celld and workerd
by 2x to 4x.** It is 1.7x to 3.4x faster than the old QuickJS cell. It still
loses to Bun: by 9% on users/42, 32% on users, and 24% on orders. The Rust
cell beats Bun on every route.

Fairness, one core, a neighbor hammering `/api/fib/32` (req/s on the fast
route, and p99):

| | Alone | With heavy neighbor |
|---|---|---|
| combd Rust | 211,828 to 214,615, 0.13 to 0.15 ms | **40,274 to 46,506, 0.54 to 0.57 ms** |
| combd Rust, yielding off | 219,713, 0.12 ms | 706, 24.7 ms |
| **combd JS** | 153,605 to 162,147, 0.16 ms | **15,875 to 25,652, 1.23 to 1.60 ms** |
| Bun | 145,122, 4.45 ms | 459, 65 ms |
| Node | 77,966, 0.52 ms | 291, 519 ms |
| celld | 43,432, 2.24 ms | 329, 54 ms |
| workerd | 36,918, 235 ms | 407, 599 ms |

Under a bad neighbor, the combd JS cell keeps 35x to 56x Bun's throughput and
55x to 88x Node's, with a p99 40x to 50x lower than Bun's.

Ranges are from three runs. This laptop was shared and busy (load average
about 7), so runs vary. One run gave the Rust cell a 52.7 ms p99 with the
neighbor; the reruns gave 0.57 ms, so we count it as noise but keep watching
it (the same kind of outlier showed up once before).

**A security bug found on the way.** In the AOT tier, a function with no loop
never spent fuel, so plain recursion could run without yielding and without
hitting the CPU limit. Rust's fib hid it, because LLVM turns it into a loop.
The JS fib twin is plain recursion, and the yield test caught it (0 yields).
Now aot-gen reads the whole call graph and spends fuel at every call that can
recurse, at every indirect call, and on entry to any function whose work
without a tick could be large. Small helpers stay tick-free. The fairness runs
above show it holds under load. A check: the old rule (tick on every entry)
and the new one yield exactly as often on fib 27 (317 times), and fib 30
takes the same time under both (`COMBD_AOT_EXPERIMENT=entrytick`).

What's next for JS speed, biggest first:
1. **Memory without refcounts.** Dropping values is about 20% of the JS cell's
   time. A per-request region that is reset after the request (with values
   that escape copied out) would remove most of it.
2. **Shapes:** objects from the same literal share a key layout, so they
   store only values.
3. **Cheaper AOT checks:** fuel and stack checks are about 20%. The call-graph
   analysis above can also prove where stack checks aren't needed.

## Durable Objects on one node (D1)

`/do/<name>/<path>` goes to one warm cell per object, with its own SQLite file
in the host. Requests to an object run one at a time. Each request is one
transaction, committed with an fsync before its answer is written (the output
gate). A crash rolls the request back and drops the cell; the next request
gets a fresh cell, which loads its state from storage. The cell's SQL is
fenced in by a SQLite authorizer (no ATTACH, PRAGMA, or transactions of its
own), size limits, and a step limit for runaway queries.

Tests run real HTTP against the interpreter and AOT: state across requests,
every answered write already on disk, crash rollback, reload after a crash,
and 80 concurrent increments on one object with none lost or doubled.

The race: the same counter object on combd (Rust cell), workerd, and celld
(Cloudflare's JS API, `bench/do/`), one core each, 32 connections, Linux in
Docker on a Mac (req/s):

| | combd | workerd | celld |
|---|---|---|---|
| Write, one hot object | **5,462** (p99 15 ms) | 3,019 (p99 15 ms) | 821 (p99 1.5 s) |
| Read, one hot object | **116,339** (p99 1.2 ms) | 13,977 (p99 1.6 s) | 10,936 (p99 40 ms) |
| Write, 1,000 objects | **1,342** (p99 43 ms) | 234 (p99 245 ms) | 182 (p99 1.6 s) |

combd is 1.8x workerd on hot writes, 8.3x on reads, and 5.7x on writes
spread over many objects. workerd and celld also had socket timeouts.

**fsync is about 90% of combd's write cost.** With the fsync turned off (an
experiment switch, never for real use), writes go to 51,922 req/s on one
object and 42,939 across 1,000. We didn't check how often workerd and celld
fsync, so their write numbers may not be the same promise.

What's next for writes:
1. **Write coalescing on a busy object:** keep one transaction open across
   the requests waiting in line, each in its own savepoint, then one fsync
   releases all of their answers. A crash rolls back only its own savepoint.
2. **One fsync for many objects:** a node log of each request's changes
   (SQLite's session changesets), fsynced once per round, with the object
   files written without fsync. The same changesets are what D2 and D3 ship
   to the bucket and to peers.

Not done yet: alarms, hibernation (evicting idle objects), objects on more
than one event loop (for now, `/do/` needs `COMBD_THREADS=1`), and the
Cloudflare JS API for objects (jsc doesn't compile classes yet).

## Security: memory is always zero

Every byte a new cell can see starts as zero. Its buffer comes from `calloc`,
whether the kernel zeroes pages lazily or glibc zeroes them with `memset`.
Safe Rust can't expose leftover memory at all (that takes `unsafe`, which is
banned). The test `fresh_cells_never_see_old_memory` fills dead cells with a
pattern and checks that new cells never see it.

## What these numbers do not cover

- **Busy cells use more memory.** 21 KB is a small cell paused mid-loop. A
  cell holding 5 MB of state uses 5 MB. Memory per cell is the cell's own
  working set. The engine adds very little on top. The way to run millions is
  that most cells are idle at any moment, and idle cells hibernate (drop the
  instance, keep SQLite on disk).
- **CPU-heavy cells hit cores, not memory.** Preemption keeps things fair, but
  it doesn't create CPU. And heavy compute is exactly where our engine is
  weakest today (11x to 24x behind Wasmtime). For those cells, the fast-
  interpreter work matters most, and Wasmtime behind the engine interface is
  the escape hatch.
- **No real storage yet.** These cells don't touch SQLite or the network. Real
  requests will spend most of their time there, and that part is the same for
  any engine.

## What we learned

1. **The Erlang shape works.** Preemption holds, crashes stay contained, a
   fresh cell serves again in 3 to 7 us, and mid-call snapshots work.
2. **Our engine wins almost everything except raw compute,** even against
   pooled Wasmtime. Pooled Wasmtime spawns faster on Linux and has smaller
   idle cells. We win load time, first request, crash restart, paused-cell
   memory (~4x), latency, and binary size.
3. **Raw compute is the real gap: 11x to 24x behind Wasmtime** after fusion.
   Each op is already cheap, so the fix is running fewer ops (a register-based
   design).
4. **Per-cell memory is mostly the guest's own touched pages.** Linux 4 KB
   pages: ~21 KB per paused cell. Apple Silicon 16 KB pages: ~69 KB.
5. **The allocator matters a lot at scale.** macOS charges ~9 ms per GB to
   free and reuse big zeroed blocks, which rules out reserving 1 GiB per cell.
   On glibc, two settings (`MALLOC_MMAP_THRESHOLD_`, `MALLOC_MMAP_MAX_`) were
   the difference between 9 us and 33 us crash restarts, and 8.5 KB and 52 KB
   idle cells. Long term: one reserved region for all cells, in the audited
   FFI crate.
6. **A bug in our test cell, caught by the native baseline.** `i * i` on a
   32-bit `usize` overflowed. Both Wasm engines agreed with each other, and
   correctly. Keep the native baseline in every test.

## Verdict

**Keep our own interpreter as the default engine.** The things combd cares
about most (many small cells, fast start, cheap crashes, fairness, small
binary, snapshots) came out in its favor, even against pooled Wasmtime. Close
the compute gap in phase 1 with a register-based design, aiming for within 5x
of Wasmtime. Keep Wasmtime behind the engine interface for compute-heavy
cells.

## Next

1. Run the official Wasm spec test suite against the interpreter. The fused
   ops make this more important, not less.
2. Register-based interpreter, toward the 5x target.
3. One reserved memory region for all cells (audited FFI crate), to get past
   the per-process map limit.
4. A real Linux x86 server with pinned threads.
5. Add celld to the scoreboard (needs a yes to install and run it).
