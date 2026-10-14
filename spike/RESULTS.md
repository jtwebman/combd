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

What we did about it: a group commit, below.

### Group commit: one fsync for many writes

Commits no longer fsync (WAL mode, synchronous=NORMAL). A pool of 16
threads fsyncs each object's WAL file, at most one at a time per object, and
every answer is held until an fsync covers its commit. Reads wait too if
they may have seen a write that isn't on disk yet, as in Cloudflare's output
gate. Each request is still its own transaction.

One core each, same run (req/s):

| | combd before | **combd now** | workerd | celld |
|---|---|---|---|---|
| Write, one hot object | 5,462 | **45,591** | 1,285 | 1,199 |
| Read, one hot object | 116,339 | **112,344** | 14,620 | 11,212 |
| Write, 1,000 objects | 1,342 | **3,511** | 421 | 157 |

Hot writes went up 8.3x: about 12 commits shared each fsync (247,895
commits, 20,788 fsyncs). Writes over many objects went up 2.6x, from fsyncs
running at once. (workerd's hot writes vary a lot between runs on this busy
Mac: 3,019 last time, 1,285 this time.)

The test `answers_wait_for_the_fsync` slows every fsync to 300 ms and checks
that no answer leaves before it, and that a read waits behind an unsynced
write. With the hold removed, it fails (the answer left in 2.5 ms). A failed
fsync drops the object and fails its held answers, since after a failed
fsync nothing in memory can be trusted to be on disk.

### Checkpoints off the event loop

Our guess was that the hot-write p99 (96 ms) came from SQLite's automatic
checkpoint running inside commits on the event loop. Moving checkpoints took
four tries, and the guess turned out wrong. What we learned on the way:

1. **A busy timeout on the object's connection** slept on the event loop
   whenever the writer tried to restart the WAL during a checkpoint. Hot
   writes fell 5x. Removed.
2. **That wasn't all of it: SQLite still fsynced inside commits,** to sync
   the WAL header after each restart (`pagerWalFrames`, found by profiling).
   The object's connection now runs at synchronous=OFF. That's safe because
   every sync happens elsewhere, in order: our fsync threads sync the whole
   WAL before any answer leaves, checkpoints use their own connection at
   NORMAL, and closing switches back to NORMAL first.
3. **Truncating the WAL (`journal_size_limit`) made every fsync slow:** a
   file that grows must also sync its size. The WAL now keeps its size.
4. **A background checkpoint can't catch up with a busy object,** so the WAL
   never restarted and checkpoints ran nonstop. Now the object alone pauses
   while its checkpoint runs on an fsync thread (a few ms, once per about
   1,000 pages). The event loop and other objects keep going.

Old (auto-checkpoint on the loop) and new, alternating, three runs each, one
core (req/s):

| | Old | New |
|---|---|---|
| Write, one hot object | 46,467 to 50,899 | **52,481 to 55,995** |
| Write, 1,000 objects | 807 to 1,048 | **1,117 to 1,804** |

The event loop no longer calls fsync at all (checked by profiling), and the
WAL stays bounded (about 10 MB).

**The p99 tail is the disk, not checkpoints.** New stats (`fsyncs_slow`,
`fsync_max_us`) show 3 to 7 fsyncs of 10 ms or more out of about 40,000 in
10 seconds, the slowest 38 to 59 ms. Each one holds every answer waiting on
it. Hot-write p99 in those runs: 9 to 16 ms, p50 0.41 to 0.45 ms. This is
Docker's virtual disk on a busy Mac; a real Linux server will tell us what a
real disk does.

### Alarms, eviction, and more than one event loop

- **One owner per object, on any number of event loops.** An object lives on
  the loop picked by a hash of its name. A request that lands on another loop
  is forwarded there over a channel, and the answer comes back the same way.
  The 80-increment test passes on 4 loops too, with requests forwarded.
- **Alarms.** A cell sets one with `alarm_set`; it's saved in the object's
  own SQLite (in a `_combd` table the cell can't touch), so it commits or
  rolls back with the request. The owner runs `ALARM` when it's due. A failed
  alarm rolls back and is retried with backoff (0.5 s, then doubling, up to 6
  tries, then dropped, as in Cloudflare). A node index lists objects with
  alarms, so a restarted node finds them. It's written before the object's
  commit and cleared after it, so it can hold an extra entry (checked and
  skipped) but never miss one. The restart test fails if the index isn't
  read at startup.
- **Eviction.** Objects idle for `COMBD_IDLE_MS` (default 60 s), or past
  `COMBD_MAX_OBJECTS` per loop (default 10,000, least recently used first),
  leave memory and come back from storage on the next request or alarm.
- **`GET /_combd/stats`:** live objects, objects opened, evictions, alarms
  fired and failed, and forwarded requests.

Speed didn't change. Alternating the old and new builds on hot writes, three
runs each: new 4,850 to 4,912 req/s (p99 11.5 to 12.5 ms), old 4,117 to
4,771 (p99 12.6 to 38 ms). Stateless HTTP was the same within noise. On 2
cores: 4,748 hot writes, 127,890 hot reads, 1,534 writes over 1,000 objects.

Still to do: the Cloudflare JS API for objects (jsc doesn't compile classes
yet).

## D2 ownership: a spec, a decision core, and a simulator

The next step is many nodes, with the bucket as the only coordinator. Before
any networking, the plan calls for three things, and all three exist now.

**1. A TLA+ spec** (`spec/Ownership.tla`, run `spec/check.sh`). A CAS
ownership record, a new epoch on every activation, a read-back after a lost
CAS reply, a manifest that records which data a new owner took over, and an
output gate that re-reads the record before any write is acknowledged. The
model has crashes, lost replies, late uploads, and two processes with the same
node name (a frozen one and its restart). No clocks: safety must hold with
any clock. TLC checks all 4,894,718 states with no error. Each known-bad
version fails on a different invariant:

| Bug | Caught by |
|---|---|
| acknowledge without re-reading the record | lost acknowledged write |
| read the old history before winning the CAS | lost acknowledged write |
| no manifest: rebuild from every segment | history rewrite |
| a same-node wake keeps its epoch | two writers in one epoch |

**2. A pure decision core** (`spike/src/own.rs`): `Core::on(now, event) ->
effects`, with no I/O, clock, or randomness. Leases are heartbeats judged by
the observer's own clock, for liveness only. Manifests and segments are
write-once, a second fence the spec doesn't need: even a process that reused
an epoch couldn't overwrite its predecessor's data.

**3. A deterministic simulator** (`spike/src/sim.rs`, `combd-spike sim N`).
The real core runs in 3 nodes against a fake bucket with real conditional
writes. Bucket calls take 1 to 50 ms (sometimes 500), 3% of requests and
replies get lost, clocks run up to 10% fast or slow, and processes crash or
freeze (the supervisor often starts a new one, and the frozen one later wakes
up stale). An oracle checks the spec's three invariants, plus that a read
sees every write acknowledged before it was sent. One seed is one exact run.

**100,000 seeds, every invariant held:** 10.4 million acknowledged writes,
7.3 million reads, 1.03 million ownership changes, 167,431 crashes, and
167,063 freezes, in 81 seconds on one core.

The same four bugs, switched on in the core, are caught within 2,000 seeds
each (most on seed 0 to 2, the slowest on seed 227). For the epoch-reuse bug,
the switch also has to turn off the write-once fence, or it can't show.

The fault rates are harsh on purpose, so liveness is low: 23% of requests
failed and clients retried. That's the next thing to measure with gentler
faults, along with real cells (Wasm is deterministic enough to run inside the
simulator) and a real executor on many nodes, with MinIO as the bucket.

## D2 on real machines: a fleet on a shared bucket

The decision core now runs in real nodes (fleet mode, when `COMBD_S3` is set),
with the bucket as the only coordinator.

**The bucket.** A small S3 client (`spike/src/s3.rs`: SigV4, get, conditional
put, list). Before serving, every node runs a storage test that races 16
writers on one `If-Match` and one `If-None-Match`; exactly one may win each.

| S3 server | One request at a time | 16-writer races |
|---|---|---|
| Silo (a MinIO fork; MinIO no longer publishes images) | pass | pass |
| RustFS | pass | pass |
| SeaweedFS 3.97 | pass | **fail: 2 to 6 of 16 won one CAS** |

SeaweedFS would let two nodes own one object, and a one-at-a-time check
wouldn't catch it. A node refuses to serve on a bucket that fails.

**The data.** A segment is the set of SQLite pages that changed, read from the
WAL's committed frames, newest copy of each (the Litestream / LiteFS way). It
covers everything, schema changes included. A new owner restores by writing
the pages of every segment its manifest lists into a fresh file. The local
file is a cache, so local commits don't fsync. The output gate is the
bucket: an answer leaves only after its segment is uploaded and a fresh read
of the ownership record shows we still own the object.

**The wire.** Nodes keep long-lived TCP connections to each other with
length-prefixed binary frames (`spike/src/fleet.rs`). A request for an object
another node owns is forwarded there, one hop at most, and the answer comes
back the same way. A broken connection fails its waiting requests with a 503
at once; a forward with no reply in 2 s does too.

**Ids.** An object id is 128 bits: the hash of its name (`/do/<name>`, like
`idFromName`), or a UUIDv7 from `/do/new` (`/do/id/<uuid>`, like
`newUniqueId`). Bucket keys always use a hash, so time-ordered UUIDv7s still
spread across the bucket.

**The cluster test** (`bench/cluster/run.sh`): Silo and three combd nodes in
Docker, one core each. `combd-spike fleet-load` sends increments to random
objects through random nodes, then checks every object: acknowledged <=
count <= acknowledged + failed. The low side means no acknowledged write was
lost; the high side means none was applied twice. (A failed answer may still
have been applied, like a timeout.) 32 clients, 50 objects, 15 s each:

| Test | Acknowledged writes/s | Failed | Longest failure stretch | Every object correct |
|---|---|---|---|---|
| Steady | 1,288 | 4 | 0 ms | yes |
| SIGKILL a node at 5 s, restart at 8 s | 1,065 | 1,525 | 3.0 s | yes |
| Freeze a node for 4 s (`docker pause`) | 705 | 19 | 41 ms | yes |

p50 is about 20 ms per write: an upload and a proof read per batch, on Silo
in Docker on a Mac. The D3 peer path (1 or 2 peers fsync and confirm) is what
brings that down.

Bugs the cluster found, fixed:
1. **The event loop died on EINTR.** A frozen node woke to SIGCONT, `poll`
   returned "interrupted", and the loop treated it as fatal. It now waits
   again. D1 had the same bug.
2. **Every node ran the storage test on the same key** (process id 1 in every
   container, same second). Their races collided and two nodes refused to
   start. The key is random now.
3. **A forward to a dead node hung until the client gave up.** A broken link
   now fails its waiting requests at once.
4. **`localhost` resolved to IPv6 first,** but the wire listened on IPv4, so
   peers couldn't connect. Every resolved address is tried now.

**mTLS on the wire.** One hub thread runs every peer connection on a `mio`
poll with rustls. Each node has a certificate for `node<N>.combd` from the
fleet's CA (`combd-spike wire-certs`). A node accepts only clients with a
certificate from that CA, and checks a server against the exact name it
dialed, so no node can pass itself off as another. Without certificates, a
node won't join a fleet unless `COMBD_WIRE_INSECURE=1` asks for plain TCP.
The test `mtls_carries_frames_and_refuses_strangers` checks that a node from
another fleet's CA and a plain-TCP node both get nothing through.

Forward timeouts are measured per peer (smoothed round trip plus 4x its
variation, 1 to 30 s), and the lease is a setting (`COMBD_LEASE_MS`,
default 10 s). The cluster, rerun over mTLS with 3 s leases: 1,192
acknowledged writes/s steady (1,288 on plain TCP, so TLS costs about 7%),
904/s through a SIGKILL (longest failure stretch 3.3 s, about one lease),
730/s through a 4 s freeze, every object correct.

**Alarms and eviction in fleet mode.**
- **Eviction:** an idle object is handed back to the core (`Release`); the
  next request claims it again, with a new epoch, and restores it from the
  bucket. The simulator now releases objects at random too; 5,000 seeds still
  hold every invariant, and the four known bugs are still caught.
- **Alarms** live in the object's SQLite, so they travel with its segments.
  An index in the bucket (`alarm/<id>/<due>`, holding the name) lets the
  fleet find due alarms of objects that aren't running anywhere. It's written
  before the segment that sets the alarm is uploaded, so it's never missing;
  a stale entry costs one wasted fire, which checks storage and cleans up.
  Loop 0 of each node scans it every second and fires due alarms as ordinary
  routed requests. A fire that finds nothing due does nothing, so duplicate
  fires are harmless.
- **Test `fleet_alarms_and_eviction`** (two nodes in one process, mTLS, a
  real bucket): an alarm fires; an alarm set on node A fires on node B after
  A stops; an evicted object comes back with its count. With index writes
  turned off, it fails ("fired": 0).

**A security bug found on the way, in D1 too:** the host runs an alarm as the
cell request `do ALARM ...`, and the HTTP front passed any method through,
so a client sending the method `ALARM` could run an object's alarm handler.
Only standard methods are accepted now.

The cluster rerun after this: 1,034 writes/s steady (1,192 last time; the
new per-request work is two small SQLite reads, so this is likely noise, but
not A/B checked), 946/s through a SIGKILL, 894/s through a freeze, every
object correct.

Not in fleet mode yet: the D3 peer path. ponytail: every node scans the whole
alarm index; at scale each node scans only the objects that place onto it.

## D3 fast writes: spec and simulator

D2 acknowledges a write only after the bucket has it, and that costs a bucket
round trip per write. D3 acknowledges once the object's peers have it on
disk, and uploads to the bucket in the background. Same order as D2: the
spec first, then the core and the simulator, then real nodes.

**1. The spec** (`spec/Replication.tla`). A write is acknowledged once every
peer has it. A new owner fences one peer (it refuses older epochs from then
on and hands back what it holds), then lists the bucket. Peers drop entries
only after the bucket has them. A clean handoff seals the epoch. If a peer is
down, a write the bucket has can be acknowledged the D2 way. TLC checks
39.9 million states with no error, and eight known-bad versions each fail.

TLC found two real bugs on the way, and the simulator found a third:
- **Fence even if the old epoch has no manifest yet** (TLC). Its owner can
  write one late and start serving, and nothing would stop it.
- **A real node can't see which epoch has a manifest at the moment it
  fences.** So the peer hands back its tail of every epoch, and the owner
  picks one after listing the bucket. Not a bug in the model; a gap between
  the model and what a node can do.
- **A stale owner must stop if a newer manifest exists** (the simulator).
  Peers clean out older epochs' logs once a newer epoch writes to them. That
  cleanup wasn't in the spec. A process that claimed an epoch, froze, and
  woke after the next epoch was serving found the old tail gone, and wrote a
  manifest missing acknowledged writes. Nobody would load from it, but it
  broke the invariant. The fix: stop when the listing shows a newer
  manifest. Both the cleanup and the stop are in the spec now, and the
  version without the stop fails there too.

**2. The core** (`own.rs`). D2 and D3 are one path now. Each seal is a log
entry. It's acknowledged when every peer confirms it (or, with peers down
or none at all, after it's uploaded and the record is re-read). Reads wait for
a confirmation sent after they ran, so a stale owner can't serve old data.
Peers are picked by rendezvous hashing and kept in the ownership record. The
set changes only across a sealed epoch, so one fence always covers every
older epoch. A sealed record also skips the lease wait on takeover. With no
peers (`replicas` 0) the core is D2, and the D2 simulator runs still hold.

**3. The simulator** (`combd-spike sim N none 2`). Each node has a peer store
that survives its crashes, like a disk. Peer calls take 1 to 5 ms (sometimes
500), get lost like bucket calls, and go unanswered while the node is down or
frozen.

**50,000 seeds, every invariant held:** 7.4 million acknowledged writes,
5.0 million reads, 734,385 ownership changes, 510,041 fences, and 832,929
entries recovered from peer tails. 78% of acknowledgements came from peers;
the rest went the bucket way while a peer was down. Each D3 bug (ack on one
peer, no fence, drop before upload, seal early) is caught on the first
2,000 seeds, most on seed 0.

**4. Real nodes.** Peer calls ride the mTLS wire as two new frame kinds.
Each node runs its peer store on one thread: take every waiting call, apply
them, write them to `peers.log`, one fsync, then answer them all (group
commit again). Startup replays the log and cuts off a torn last record; a
big log is rewritten from a snapshot. `COMBD_REPLICAS=2` turns it on. Test
`fleet_peer_writes`: three nodes, writes acknowledged by peers, then the
owner stops and another node fences, recovers, and has all 30 writes.

**The cluster race** (`REPLICAS=2 sh bench/cluster/run.sh`), D2 and D3
alternated, three runs each, on the same build:

| Test (median of 3) | D2 writes/s | D3 writes/s | D2 p50 | D3 p50 |
|---|---|---|---|---|
| Steady | 821 | 1,301 | 28 ms | 14 ms |
| SIGKILL a node | 605 | 1,166 | 25 ms | 14 ms |
| Freeze a node 4 s | 636 | 756 | 27 ms | 16 ms |

Every object was correct in all 18 runs. 98% of D3's acknowledgements came
from peers, and takeovers really recovered entries from peer tails. D3 won
8 of 9 pairs, and its p50 was about half of D2's in every run.

These numbers are rough. The MacBook was busy with other agents' work, and
runs swung 2x to 5x with nothing changed (one D2 freeze run fell to 84/s).
An A/B against the code from before the D3 core showed no D2 regression
(steady averages 828 vs 827), but only alternating runs can be trusted
here. Honest numbers need quiet Linux machines.

**A gap, closed later:** a peer that lost its disk and came back as the same
node would answer a fence knowing nothing. At first such a node refused to
start; see "A peer that loses its disk" below for the real fix. Also, a peer
no longer skips ahead past what the bucket has.

## Network splits

**In the simulator** (`combd-spike sim N none 2 splits`, or `only-splits`
for no other faults). A split lasts 0.5 to 5 s and comes about every 3 s.
It cuts off a node from the bucket (both ways, requests only, or replies
only), from the other nodes, or from everything but its clients. It can also
split the nodes into two groups, or drop one node's messages to another
one way.

**Safety: 80,000 seeds, every invariant held.** D2 and D3, with splits on
top of the other faults and with splits alone: 13.1 million acknowledged
writes through 526,000 splits. With splits as the only fault, all nine
planted bugs are still caught. The scary case, an owner cut off from the
bucket but still reaching its peers, is exactly where the no-fence bug
fails, on seed 0.

**Availability: the splits found three ways we held clients too long.**
Measured as the longest time an object went without an acknowledged write,
median over 100 seeds, one kind of split at a time:

| Split | D2 before | D2 after | D3 before | D3 after |
|---|---|---|---|---|
| node cut off from the bucket | 5.3 s | 3.2 s | 4.7 s | 1.3 s |
| node cut off from everything but clients | 5.3 s | 3.2 s | 5.6 s | 3.4 s |
| node cut off from the other nodes | 1.1 s | 1.1 s | 1.2 s | 1.2 s |
| two groups of nodes | 1.1 s | 1.1 s | 1.2 s | 1.2 s |
| one way, node to node | 1.0 s | 1.0 s | 1.1 s | 1.1 s |

The fixes:
1. **D3 keeps serving when an upload is lost.** The peers hold the entries,
   so the owner retries on the next tick instead of giving the object up.
   The retry is the exact same segment: the lost one may have landed, and
   two segments must never overlap.
2. **A node that can't reach the bucket fails its requests now** instead of
   retrying forever, so clients go to another node. All six simulated
   clients had ended up stuck on one cut-off node, stalling even objects on
   healthy nodes.
3. **A request unconfirmed for a whole lease gets "retry".** A node cut off
   from everything but its clients can't confirm anything; it shouldn't
   hold them.

What's left in the 3 s cases is the lease plus routing hints: other nodes
keep sending clients to the cut-off owner for about a lease before they
decide it's gone. That's tuning, not safety.

**On the real cluster** (`REPLICAS=2 sh bench/cluster/split.sh`). Each kind
of traffic gets its own Docker network (bucket, node to node, clients), so
one kind of link can be cut. Under load, node1 is cut off for 4 s from the
bucket, from the other nodes, or from both while its clients still reach
it. Bucket calls now time out fast (2 s to connect, 5 s to read), so a cut
shows up as a lost call instead of holding a thread.

| node1 cut off from | D2 writes/s | D3 writes/s | D2 worst gap | D3 worst gap |
|---|---|---|---|---|
| nothing (steady) | 954 | 2,160 | 0 ms | 0 ms |
| the bucket | 732 | 2,065 | 623 ms | 1,088 ms |
| the other nodes | 583 | 1,249 | 0 ms | 0 ms |
| both | 559 | 1,249 | 431 ms | 151 ms |

Every object was correct in all 8 runs. "Worst gap" is the load driver's
longest stretch of failed requests. One run each, on a busy machine, so the
rates are rough. Note D3 barely slows when the owner loses the bucket: its
peers still confirm writes. One-way splits need `iptables` in the image;
the simulator covers those.

## A peer that loses its disk

If a peer's disk is lost and the node comes back empty, it has forgotten two
things: the entries it held, and whom it fenced. The second is the worse one.
If a new owner fenced only that peer, the old owner could write to it again
and get writes confirmed.

**The fix.** A node that has run before (its lease is in the bucket) but
whose peer store is gone comes back *wiped*, with a marker file so it stays
that way. Before it answers anything about an object, it reads the object's
ownership record (epoch R). It refuses every epoch below R, which is the fence
it lost. And it speaks only for epochs after R, because those began after it
came back, so it saw all of their entries. Its fence answers carry R. A new
owner uses a peer's tail only if that peer can speak for the epoch it needs.
If none of the answers it has can, it waits for the other peers and lists the
bucket again, so the listing still comes after the fence of the peer it uses.

**Checked four ways:**
- **Spec:** a Wipe step (at most once: two lost disks at once really can lose
  writes). TLC found a hole in the first try. "The peer has a log of that
  epoch since the wipe" isn't enough, because the owner may still be
  re-sending it, so the copy can be partial. With the fix: 143.9 million
  states, no errors. The two bad versions (trust a wiped tail, forget the
  fence) both fail.
- **Simulator:** one lost disk per run, at a crash. 2,000 seeds hold every
  invariant, and both bad versions are caught.
- **Real nodes:** the test `peer_store_survives_restarts_torn_records_and_compaction`
  deletes the log, reopens, and checks that the store relearns from the bucket
  (and gives no answer when the bucket is out of reach).
- **Cluster:** SIGKILL a node and delete its disk under load. It comes back
  wiped, relearns, and every object is correct (1,425 writes/s through it).

Found on the way: a store snapshot wrote logs with dropped entries in a way
the stricter skip rule refused to replay. Compaction would have lost them. A
test covers it now. The node also writes its lease before serving, so a node
that served always has one.

## Engine safety: the spec testsuite and differential fuzzing

The fleet was checked four ways; the sandbox only had our own tests. Now it
has two outside judges.

**The official WebAssembly testsuite** (`spike/spectest.sh`, pinned commit,
every `.wast` file) against the interpreter: **30,497 checks pass, 0 fail**,
34,346 skipped. A skip is a module we refuse at load because it uses
something we don't support, counted by reason: SIMD (most of them), imported
memories, tables, or globals, memory64, GC, `table.copy` and `table.init`,
start functions. It found two problems:
1. **Lazy refusal.** A module using an instruction we don't run loaded fine
   and trapped only when it reached one. Now it's refused at load, by name.
2. **Two memories, one answer.** A module with a second memory got memory 0's
   answers for it. Wrong, though every access was still bounds-checked.
   Refused at load now.

**Differential fuzzing** (`combd-spike fuzz N`). wasm-smith makes random
valid modules that use only what we run. Every loop ends (a fuel global), and
NaN results are canonical. Every export is called with random arguments on
our interpreter and Wasmtime, each call on a fresh instance. Results, traps,
and every byte of memory must match. Running out of stack is inconclusive
(the engines have different limits). Both engines get the same memory cap.

| Run | Modules | Calls compared | Mismatches |
|---|---|---|---|
| Interpreter vs Wasmtime, first | 50,000 | 189,782 | 1 real bug (and 31 from a harness gap) |
| Interpreter vs Wasmtime, after fixes | 50,000 | 189,219 | 0 |
| AOT (guard pages) vs interpreter, first | 138 | 469 | 1 real bug |
| AOT, both builds, after the fix | 979 | 3,186 each | 0 |
| AOT, both builds, with multi-value | 1,270 | 5,949 each | 0 |

**Bug 1, the interpreter: a wrong answer.** `big == extend(0)` said "equal".
To use an immediate, a commuted compare swapped its operands. But `big`
doesn't fit an immediate, so it went to the scratch slot, and that slot held
the other operand: it compared `big` with itself. It only touched the cell's
own frame. The shrinker (wasm-shrink) and a hand-made test pinned it down;
the regression test `a_commuted_compare_keeps_both_operands` fails without
the fix.

**Bug 2, the guard-page AOT build: missing traps.** A load from a 0-page
memory returned instead of trapping. Its value was never used, and to LLVM a
plain read has no side effects, so it deleted the load, and the guard page
never saw it. No memory outside the cell was touched, since the access was
removed, not done. But Wasm says it must trap. The fix is the one wasm2c uses
(`FORCE_READ`): an empty asm takes each loaded value, so the load stays, and
every other optimization still applies. Cost, alternating builds in-process:
0.54 to 0.57 us, 1.32 to 1.38 us, and 3.53 to 3.59 us per request (2% to 5%).
The bounds-checked build never had this bug: its check has a side effect.

**The harness gap:** the first run let Wasmtime grow memory to 4 GiB while
our test engine stops at 64 MiB, so `memory.grow` disagreed. That's a limit,
not a bug; both get the same cap now.

Still open: stores. LLVM may also move a store across one that faults, so
memory at a trap could differ from Wasm's. The fuzzer compares memory after
every trap and hasn't seen it in 1,270 modules, but that's not proof. Also
not done yet: Wasmtime against the AOT tier directly. (The host API, HTTP,
and the wire are fuzzed now: see the next section.)

**Multi-value in the AOT tier.** A compiled function with two or more
results now returns `Result<[u64; N], Trap>`; one with 0 or 1 still returns
`Result<u64, Trap>`, which comes back in two registers, so the fast path
didn't change (the Rust cell's generated functions are byte for byte the
same). Calls, direct and through the table, unpack the results into stack
slots; blocks with several results already worked. Only imports with more
than one result are still refused: the host interface returns one value.
The same 1,700 fuzz seeds that had 153 modules refused now compile all
1,270, and both AOT builds agree with the interpreter on every call.

## Fuzzing the boundaries: the host API, HTTP, and the wire

The sandbox wall is the host API: a cell calls host functions with pointers,
lengths, and SQL it picked, and it even picks their types. `combd-spike
fuzz-host N` makes random calls against a real object database: right and
wrong arities, edge-value pointers and lengths, real iov arrays, hostile SQL,
memories from 0 bytes to 64 MiB. After each call: the host never panics, no
call takes over 250 ms, the alarm changes only through `alarm_set`, the
host's transaction is still open, and no stray file appears. To make sure
the fuzzer can see them, each bug below was put back and caught.

| Bug | What one cell could do | Fix |
|---|---|---|
| Wrongly typed import | import `sql` with no parameters: the host indexed past its arguments and **panicked** | host calls never index arguments blindly |
| `fd_write` | 20,000 iovs over 64 MiB: **seconds** of host time, at almost no fuel | copy only what fits under the 64 KB cap; at most 1,024 iovs |
| Big answers | 200 rows of 15 MB: **3.8 GB** of host memory before the size check | count the answer as it grows |
| Big values, slow steps | one statement wrote **2.9 GB** to disk; one step can build a 15 MB string, so the step limit allowed seconds | values up to 2 MB (as Cloudflare's), 100 ms per statement, 1 GiB per object |
| Lost transaction | hitting those limits makes SQLite roll back the whole transaction: the request lost its earlier writes while the host thought it was open | that crashes the cell, so the request fails as a whole |
| Host time per request | a cell looping over slow SQL paid almost nothing (fuel counts its own instructions) | 1 s of host time per request |

After the fixes: 200,000 hostile calls, no problems; the slowest was a SQL
statement at the 100 ms limit. One surprise on the way: the broken
`fd_write` copied about 70 GB/s from a 1 MB memory (it stays in cache), so it
only showed as slow with big memories. The fuzzer now uses those too.

**HTTP.** Any client could crash the event loop: `Content-Length:
18446744073709551615` wrapped `hlen + clen` past `usize::MAX`, and slicing
the body panicked. Now a bad length, or two that disagree, is refused (RFC
9112), and so is a body over the request cap. A test damages valid requests
200,000 ways (flipped bytes, garbage, cut-offs, lying lengths) and the
parser never panics.

**The wire.** 100,000 damaged frames (flipped bytes, lying lengths,
cut-offs) and 200 damaged peer logs: no panics, and whatever decodes, the
peer store takes. The decoder was bounds-checked from the start, and it held.

**Stores at a trap, in the guard-page AOT tier.** Hand-written cases
(`spike/aot-cases/precise_traps.wat`) store, trap, and store again. One
fails on the guard-page build: LLVM drops a store that a later one
overwrites, not knowing the load between them can fault, so memory at the
trap isn't what Wasm says. That's invisible in combd: a trapped cell is
never used again (stateless cells are dropped; an object's cell goes back
only after a request succeeds). So it's documented as an invariant rather
than paid for on every store. The bounds-checked build is exact, and both
builds still must match the interpreter on results, traps, and memory
after every call that succeeds.

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
