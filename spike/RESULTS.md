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

### Round 3: a hot loop that stays in registers (2026-10-16)

A sampling profile, down to single instructions, showed the real cost. The
one big `exec` loop spilled its key state (the frame pointer, the op
pointer, fuel) to memory, so every op reloaded it from the stack. Cutting
ops didn't help at first: the sieve ran a third fewer ops in the same time.
What changed:
- **A small hot loop.** Register ops, branches, calls, and returns run in a
  separate function small enough that its state stays in registers. Anything
  else falls back to the old loop. No library calls inside it (a `memmove`
  for branch values forced spills too).
- **A fixed window per frame.** Each frame sees a 64K-slot window of the value
  stack, so a slot index (a `u16`) needs no bounds check. A function whose
  frame doesn't fit is refused at load.
- **Fewer ops.** A reduction is spent on a loop's back-edge, not on a `Tick` at
  its head; `br_if out; br loop` is one branch; `br_table` reads its index
  from a slot. Fused, from the profile: `a + (b << k)`, a load from `a + imm`,
  a load straight into a store, `imm - x`, and the shadow stack pointer moves
  every compiled Rust function does on entry (five ops become two).

Later the same night: the stack-form ops (all float math) moved into the hot
loop too, then f64 add, sub, mul, div, and compares got register forms (JS
numbers are f64; each f64 op had cost four ops).

macOS, this morning's binary against tonight's, interleaved, best of 5:

| | Before | After | Wasmtime |
|---|---|---|---|
| fib 30 | 19.2 ms | **16.0 ms** | 2.2 ms |
| sieve 5,000,000 | 105 ms | **95 ms** | 12.0 ms |
| sort 1,000,000 | 208 ms | **177 ms** | 13.1 ms |
| JS fib 30 | 47.1 ms | **24.5 ms** | 6.5 ms |
| Go fib 30 | 22.6 ms | **17.7 ms** | 3.5 ms |
| JS (jsc) GET /api/users/42 | 33.3 us | **28.6 us** | 3.2 us |
| JS (jsc) GET /api/users | 60.7 us | **55.6 us** | 6.0 us |
| JS (jsc) POST /api/orders | 134 us | **122 us** | 12.3 us |
| Rust GET /api/users/42 | 4.84 us | 4.87 us | 1.4 us |
| Rust POST /api/orders | 53.0 us | **46.7 us** | 5.7 us |

On the arena's own nodes (DigitalOcean, one shared x86 vCPU), this morning's
image against tonight's, best of 3: fib 30 160 to **104 ms**, sieve 925 to
**742 ms**, sort 1,721 to **1,080 ms**. The gain is bigger on x86 (20% to 37%),
which has half the registers to keep the loop's state in.

JS compute is **1.9x faster**; Rust compute and the JS API 8% to 17%; Rust
API requests are about the same (except POST, 12%). Every time the hot loop
grew, the Rust compute rows paid a few percent back in register allocation
and layout, so it holds only what real workloads run. One cost: a cell paused
mid-call is bigger, since its stack no longer shares pages with other small
allocations: about 7 KB on Linux x86 with 4 KB pages (the arena's images,
3,000 cells: 74.0 to 81.1 KB per cell), 23 KB on macOS with 16 KB pages
(10,000 cells: 118 to 141 KB). An idle cell is the same.
The spec suite (30,497 checks), the differential fuzzer, and a new test of
each fused op at its edges (`fused_ops_match_wasmtime`) all agree with
Wasmtime. The gap is now 4x to 10x. The rest is dispatch: each op is a
table jump, about 1 ns. Fewer, bigger ops or the AOT tier are the ways past it.

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

## WebSockets with hibernation

Cloudflare's hibernatable WebSocket model, on combd's own pieces. The host
holds the sockets, not the cell: an idle object leaves memory as usual, its
sockets stay open, and the next message brings it back with a fresh cell
that loads its state from SQLite. Pings are answered by the host and wake
nothing.

**How a cell sees it.** Three host-made events, like `ALARM` (clients can't
send these methods): `do WSOPEN <path> <id>` (answer `101` to accept, or any
other status to refuse it as plain HTTP), `do WSMSG /<id> <text>`, and
`do WSCLOSE /<id> <code>`. Three imports: `ws_send(id, ptr, len)`,
`ws_close(id, code)`, and `ws_list(ptr, cap)` for the object's socket ids.

**Same rules as everything else:**
- Sends wait for the output gate, with the answer: they go out only once the
  request's writes are on disk. A request that traps sends nothing.
- Every socket of an object lives on the object's event loop. A handshake
  another loop accepts is handed over, TCP stream and all.
- Caps, from the fuzzing lesson: 1 MiB per message (as Cloudflare's), 16 MB
  queued per request, 64 messages waiting per socket before we stop
  reading, and a client that reads too slowly (8 MB unsent) or sends too
  fast (4 MiB unread) is closed.
- The protocol code (`spike/src/ws.rs`) checks every RFC 6455 rule a server
  must: masking, reserved bits, opcodes, control frame sizes, fragments,
  UTF-8. Its own SHA-1 and base64 (a few dozen lines each) for the handshake.

**Tests.** `websockets_on_the_interpreter` and `websockets_on_aot` run a
chat room over real TCP on two event loops: broadcast, a ping, hibernation
(the object leaves memory, both sockets stay, a message wakes it with its
messages intact), sends from an HTTP request, a crash that sends nothing, a
close from the object (4000) and from the client (echoed, and the object
hears it), a refused handshake (403), a frame that breaks the rules (1002),
and handshakes handed between loops. The frame decoder takes 20,000 damaged
inputs without panicking, and the host fuzzer covers the three new imports.

**Binary messages.** A cell's events are text, so a binary message rides in
its event as base64 (`do WSBIN /<id> <len>` by the time the cell sees it)
and the host turns it back into bytes just before the cell runs; the cell
copies them out with `ws_msg(ptr)`, and sends binary with
`ws_send_binary`. Riding in the event means it crosses nodes as is.

**Fleet mode.** A WebSocket's TCP connection stays on the node the client
reached. Its events become ordinary requests to the object, which the fleet
already routes to the owner (forwarding, takeover, D3). The object's socket
list moved into its storage (`_combd_sockets`, a host table the cell can't
touch), written in the request's transaction, so it replicates with the
object and comes back after a takeover. Socket ids carry their node, a
random boot number, and their loop, so they're unique across the fleet and
across restarts. The owner sends to a socket on another node with a one-way
wire frame (`WsPush`). A socket that turns out to be gone (the node
restarted, or can't be reached) gets its object a close event (1006), and
leaves the list with the next committed request. Sends still wait for the
output gate; in fleet mode, that's the core saying the write is proven.

Test `fleet_websockets_cross_nodes_and_survive_a_takeover`: two nodes, a
client on each in one room. Node 0 owns the room, so node 1's handshake goes
there. Text and binary cross both ways. Then node 0 stops; node 1 takes the
room over after the lease, with its socket list, and its client hears its
own message again. The single-node test sends all 256 byte values both ways.

Not yet: per-socket attachments (cells can keep that in SQLite), and
numbers (idle sockets per GB, wake-up time, cross-node message time).

## Bring your own database: Postgres

The host runs one pool per `[pools.<name>]` in the app's manifest:
a manager (`spike/src/pool.rs`, pure decisions, tested on its own) and
members, each one connection on its own thread (`spike/src/pg.rs`). The
first ask starts a member; an ask while all are busy starts another, up to
`max`; then asks wait in line and fail after `wait_ms`.
A lease is checked on every call, so a holder whose lease ran out can't
reach the next holder's. A member takes back a lease held past
`lease_ms` on its own (it rolls back; the holder's next call gets an
error), and stops after `idle_ms` free. A failed connect is tried again
`connection.retries` times, with backoff. A cell names a pool;
it never sees a socket or a password. Its imports: `pg_open(req)` leases a
member and returns a handle, `pg_query(handle, req)` runs one statement
(handle 0: a lease for one statement), `pg_read(ptr, cap)` copies the answer
(the same JSON as the object's own SQLite), and `pg_close(handle)`.

**Two bugs the rewrite found.** The old reset sent `ROLLBACK; DISCARD ALL`
as one string, which runs as one implicit transaction, and `DISCARD ALL`
refuses to run in one. Every reset failed, and the old pool hid it by
quietly reconnecting each time: a full connect per statement. Now they're
two statements, and a test checks that two pooled statements in a row get
the same backend. That test then caught a race: a pooled statement answers
before its member resets, so the next ask came in before "free" and started
a new connection, growing the pool to max under one-at-a-time traffic. Now
the member says "resetting" before it answers, and an ask waits for it.

**A cell waiting on the database is paused, and costs no CPU.** On the
interpreter the host call just doesn't resume yet; on the guard-page AOT tier
the call pauses the cell's fiber, the same way a fuel slice does, and its
request and answer pass through a small mailbox while the cell's state is on
the fiber. The event loop parks the cell (out of the run queue, so it isn't
woken for nothing) and runs everything else. Wasmtime can't pause there, so
there `pg_*` crashes the cell with a clear message.

**Where a handle lives decides what it can hold:**
- A Worker's handle belongs to its request and closes when the request
  ends: a transaction within one request.
- A Durable Object's handle belongs to the object, across requests and
  WebSocket messages: `BEGIN` in one message, `COMMIT` in a later one,
  with row locks (`FOR UPDATE`) and session advisory locks
  (`pg_advisory_lock`) held in between.
- A handle closes on `pg_close`, when the cell crashes, when its object
  leaves memory or this node loses it, and when it's been idle for
  the pool's `session_idle_ms` (60 s). An object with an open handle stays in
  memory until then. Closing resets the connection (`ROLLBACK; DISCARD
  ALL`), so a transaction rolls back and every lock is released, and the
  connection goes back to the pool.

**Rules from the plan:** connections are reset after every lease (a `SET`,
temp table, or lock never reaches the next user); every connection has a
statement timeout (5 s; a lock wait counts), and an
`idle_in_transaction_session_timeout` and TCP keepalives, so a session from
a node that died frees its slot on the server; "too many clients" from the
server makes an ask wait, not fail; answers are capped at 16 MB,
counted as they grow.

**Test `postgres_on_the_interpreter` / `postgres_on_aot`**, against a real
Postgres 18: every mapped type both ways (int, bigint, float, bool, text,
jsonb, null) and SQLSTATE codes on errors; a pooled `SET` that never leaks; a
Worker's transaction in one request, and crashes mid-transaction that roll
back without leaking their connections; a Durable Object's transaction over
three messages with a row lock, while another object's update waits for it
and other requests keep getting answered; an advisory lock held across
messages (`pg_try_advisory_lock` elsewhere says no, then yes); a crash that
releases its object's locks; a blocking `pg_advisory_lock` that parks the
cell without stalling the loop; and an idle session that closes, releases its
lock, and lets its object leave memory.

**Test `postgres_pool_limits`** (max 2): two held sessions fill the pool; a
third ask fails after its wait; a release serves one that's waiting; a
session held past its lease is taken back and its holder told; then two
more fit.

Not yet: the pool across a fleet (one manager, nearest member, cross-node
calls, takeover), LISTEN/NOTIFY as events, query caching, read replicas, MySQL, and
numbers.

## The manifest: typed settings, vars, and secrets

An app ships one of `combd.toml`, `combd.yaml`, or `combd.json`
(`spike/src/manifest.rs`). All three read into the same tree and get the
same checks, and every mistake is reported at once. Any mistake and nothing
starts. No more env vars for app settings: node settings (threads, data
dir, fleet) stay with whoever runs the machine.

- `[vars]` and `[secrets]` declare every env var the app gets. A default
  gives a var its type (`PG_MAX = 8` is an int), or it says one; no default
  means every environment must set it, and `?` on the name makes it
  optional. A secret maps an env var to a secret's name and never has a
  default: a default would be in git, and in every environment. A name
  can't be both.
- Types: `string` (`pattern`, and `min`/`max` length in characters),
  `int` (i64), `float` (f64), `bool`, `datetime` (RFC 3339 with an offset,
  `min`/`max` compared as instants), `url`, and `enum`. Hard caps: 16 KiB a
  value, 256 KiB of env vars in all, 512 names, a 1 MiB file, 1 KiB
  patterns. Patterns use Rust's `regex`: no look-around or backreferences,
  so matching is linear (a test runs `(a+)+$` on 16 KiB in no time).
- `[processes.<name>]` sets a process kind's settings, and each kind lists
  what it takes, typed the same way (`combd:postgres`: `url`,
  `connection.timeout_ms`, `connection.retries`, `statement_timeout_ms`).
  `[pools.<name>]` is generic: a `member` process plus `max`, `wait_ms`,
  `lease_ms`, `idle_ms`, `session_idle_ms`, `on_manager_loss`.
- `"$NAME"` uses a var or secret, and must be exactly the type the setting
  wants: no conversions. An optional name can't fill a required setting.
  Values are checked again per environment against the setting's own
  limits, since a var's may be wider. `$$` is a literal `$`.
- Who may reach whom is config, default deny: `routes` (the internet;
  `host/path` or `/path`, `:name` captures, a final `*`), `callers` (which
  processes may `call` or `send` to it), and a pool's `owners` (which may
  hold a lease). Only the app's own processes can call or own; built-in
  members are reached only through a pool. A durable process's routes name
  it with `:id`, and it takes one event at a time. A process nothing can
  reach is an error. `kind = "api"` is the app's module `modules/api.wasm`.
- Every var and secret reaches cells as an env var (WASI `environ`).
- The local `dev` environment is `combd.dev.{toml,yaml,json}` beside the
  manifest, refused if git tracks it. An error about a secret never shows
  its value, and a pool's config has no `Debug`, so it can't be printed.

**Tests:** `manifest::tests` (all three formats resolve the same; 17 kinds
of mistake in one file, all reported; references and their types;
datetimes and their limits; caps; patterns), and
`the_manifest_sets_up_vars_secrets_and_pools` (vars and secrets reach a
cell's `environ`, the pool runs a query, a bad environment doesn't start).

Next: environments in the control plane (per pull request, `--from`, a
TTL, their own storage), and secrets encrypted there.

## An app of processes

`combd-spike serve-app <engine> <dir> <port>` serves an app from its
manifest (`spike/src/app.rs`): one program per kind (`modules/<kind>.wasm`,
loaded once), and each process with its own warm cells, event-cell
instances, and settings (`max_concurrent`). Requests are routed by host
(case and port ignored) and path (query string ignored), most specific
first: a host beats any host, then a literal beats a capture beats `*`. A
durable process's object is `<process>/<id>`, so two processes never share
an object, and its request sees the path after the route's `*`. Anything
no route matches is a 404, the old fixed `/do/` routes included. With no
manifest, `serve` works as before.

**Processes call each other, only where the manifest says.** `call` waits
for an answer: a classic cell parks (no CPU) and gets it from `pg_read`; an
event cell uses `call_async` and gets it in `on_event`. `send` (and
`send_async`) is fire and forget. Both take `{"to", "id"?, "method"?,
"path"?, "body"?, "timeout_ms"?}`; `id` names the object when the target is
durable. The target sees an ordinary request. The host checks `callers`
(it knows which process is calling, so a cell can't pretend to be another)
and answers a call exactly once: `{"status", "body"}` (a crashed target is
a 500), or `{"error"}` for a denial or a timeout (the caller's loop keeps a
deadline per call, at most the target's `timeout_ms`; a late answer is
dropped). A request's `send`s go out only if it succeeds, and an object's
only once its writes are on disk: the same output gate as its WebSocket
sends. Pools check `owners` the same way, on every lease and pooled query.

**Test `processes_call_and_send_*`:** calls to a worker and to an object;
a denial for a process not in `callers` (and an unknown one); a timeout
(200 ms against a 3 s sleep) and a crash's 500; `send` from an event cell;
a parked `call` from an object; `send` after an object's commit; and a
request that sends, then crashes: its message never arrives. The manifest
test checks that a process not in a pool's `owners` is denied. The
host-API fuzzer covers `call`, `send`, `call_async`, `send_async`, and
`environ_get` (20,000 seeds, clean; two `sql` calls ran slow under a load
average near 10, and were fine when run again alone).

Object names were allowed up to 256 bytes, but they're hex-encoded into
file names, which most file systems cap at 255 bytes: names over 124 bytes
could never open. The cap is 120 now.

**Tests:** `an_app_of_processes_*` (two processes on one module keep their
own instances and state, host routes, a classic worker, objects scoped to
their process and on disk as `room/r1`, and 404s), `routes_pick_the_most_
specific`, and the parser fuzzer now runs every mangled request through a
manifest's routes too.

## Heartbeats over the wire, not the bucket

A node used to prove it was alive by renewing `lease/<node>` in the bucket
every third of a lease: a read and a write each time, about $0.14 a node a
day on S3 ($140 a day at 1,000 nodes), the biggest steady bucket cost. Now
loop 0 sends every peer a tiny `Heartbeat` frame each tick (node and boot
number), and a node's cores record when they last heard each other node.
An object whose owner hasn't been heard from for a lease, by the
observer's own clock, can be taken over; a node never heard from is timed
from the core's own start, so a node that just booted doesn't take
everything. Liveness only: the ownership spec already lets any process
claim at any time, so no proof changed.

The heartbeats come from the event loop itself, not a thread of their own:
the first version had a thread, and the real-node test caught it: a node
whose loops had stopped kept "beating", so nobody ever took over its
objects. The simulator couldn't have seen that (its processes have no
threads).

The duplicate-id check moved too (it read heartbeats in the bucket): a
starting node asks every peer, over the wire, when it last heard its id and
from which boot. Heard lately from another boot: it waits for that one to
go quiet (a restart takes up to a lease longer), and after three leases of
it, refuses: it's a live duplicate.

**The simulator** now sends heartbeats through its network (lost, late, or
cut by a split like anything else), can lose a node for good (`loss`), and
checks liveness: once the faults stop, a probe per object must get a write
acknowledged. A new known-bad version, `assume_alive` (everyone counts as
heard from, always), is caught at once when a node is lost, and passes
every safety check when none is, as it must (safety never rested on
liveness). D2 and D3, with splits, lost disks, and a lost node: 8,000 seeds,
every invariant held. One thing changed: false deaths (and so takeovers)
are rarer now that liveness doesn't ride a lossy, slow bucket (0.68
takeovers a run, from over 1), so the bug that needs a takeover while a
peer's disk is gone (`wipe_forgets_fence`) takes about 7,000 seeds to show
instead of under 5,000. Its test searches up to 20,000.

**Real nodes** (`bench/cluster`, three Docker nodes and a bucket, 32
clients on 50 objects, 15 s each; `ENGINE` now picks the engine, `interp`
here): every run kept `acknowledged <= count <= acknowledged + failed` for
every object.

| run | writes/s | failed | longest failure stretch |
|---|---|---|---|
| steady | 2,950 | 0 | 0 ms |
| SIGKILL a node, back at 8 s | 1,962 | 1,655 | 2,743 ms |
| freeze a node 4 s | 1,893 | 34 | 0 ms |
| SIGKILL and lose its disk | 1,509 | 1,907 | 2,351 ms |
| cut from the bucket 4 s | 2,194 | 747 | 373 ms |
| cut from the wire 4 s | 1,197 | 327 | 237 ms |
| cut from both 4 s | 1,393 | 233 | 346 ms |

A killed node's objects move in under three seconds, a lease and a bit,
with nothing in the bucket telling anyone it died.

## The catalog in the nodes: deploys and vars

Every fleet node runs the catalog (`spike/src/catnode.rs`): a thread around
the tested core that does its bucket calls through a small storage trait
(`spike/src/kv.rs`: get, put always / if absent / if the etag matches,
delete, list; the bucket is the first backend), pushes each new entry to
the other nodes over the wire (a new one-way frame, `Catalog`), keeps its
copy on local disk for a fast restart, and snapshots and trims the log
every 100 entries. It looks at the bucket every 30 s in case a push was
missed.

Deploys and vars live in it now, not in per-app pointers: `<org>/<app>` is
`{"version", "domains"}` (empty when taken down) and `<org>/<app>#vars` the
vars. The domain check runs in the catalog's write, against the log's
head, so the bug TLC found in the first design (two deploys on two nodes
both claiming one domain) is gone. A node swaps in what changed the moment
its copy moves: no polling. Bundles stay in the bucket, by hash.

**Tests:** `deploys_reach_every_node` (all of it through the admin API, now
on two nodes): a deploy reaches all three nodes about 25 ms after it's
acknowledged (it was up to 2 s, polling); two deploys at once, through two
nodes, both claiming `race.test`: exactly one wins, the other hears the
domain is taken, and every node serves the winner (3 runs out of 3).
`the_catalog_snapshots_trims_and_catches_up` (a real bucket): 250 changes
snapshot and trim the log, a duplicate domain is refused, and a new node
with no copy at all catches up to the identical state. The whole test run
got faster too (no 2 s polls): about 9 s.

Not yet: node heartbeats over the wire (they still renew in the bucket;
that's the ownership core, next), secrets, and two vars changes at once on
two nodes (the later one wins whole).

## One Postgres pool for the whole fleet

A pool in the manifest (`max = 100`) is now one for the fleet, not one per
node: 1,000 nodes never open more than `max` connections. Every node runs
an agent per pool (`spike/src/pg.rs`); one of them is also the manager,
running the tested pool core (`pool.rs`) as before.
- **Members start on the asker's node.** At max, the nearest free member
  is leased instead, and that holder's queries go over the wire (a new
  one-way frame, `Pool`). Members never move.
- **The catalog names the manager** (`<org>/<app>#pool/<pool>`: node and
  epoch), so every node agrees on it about 25 ms after a change. A claim is
  "epoch e + 1, if it's still e" (the catalog's check refuses anything
  else), so of two at once, one wins. A node claims when there's no manager
  yet, when its own process restarted, or when the manager's node has been
  quiet for a lease, and only when it has asks or members.
- **A new manager starts with empty books.** Members of the older epoch
  finish their current lease and stop (`on_manager_loss = "finish"`, the
  default) or stop now (`"stop"`). Until they're gone the database can see
  more than `max` for a moment; "too many clients" makes an ask wait, not
  fail.
- **Once a second each node tells the manager what its members are
  doing,** so a lost message can't leak a member: one it lists that the
  manager doesn't know stops, one it no longer lists is gone, and one busy
  in the books but free on its node is free. Changes younger than 2 s are
  left alone (their news may be on its way).

**Test** (`a_pool_is_one_for_the_whole_fleet`, three nodes and a real
Postgres): `max = 2`, four `pg_sleep(0.3)` queries on every node at once.
They took 1.82 s (12 x 0.3 s on two connections is 1.8 s: both kept busy,
so the node with no member of its own used one on another node), and a
sampler on `pg_stat_activity` never saw more than two connections. Then
the manager's node stops during a 2 s query on another node: the query
finishes, another node takes over, every node still gets answers, and the
connections fall back to at most two. Five runs in a row: 1.820 to 1.821 s.

ponytail: no simulator for the pool's messages yet (the core has unit
tests, and this runs on real nodes); a manager cut off from the fleet keeps
granting to its own node until it hears the new epoch (more than `max`
until then); the manager is wherever the first ask came from, with no
placement preference; `on_manager_loss = "stop"` has no test of its own.

## Read leases: reads with no round trip

Every read used to wait for a round to its object's peers, the proof that
no newer owner had taken over. On one LAN that's under a millisecond; on
the arena's fleet (New York, Frankfurt, Singapore) it was up to 230 ms per
read, and every match tick and every input over a socket is a read. A
matchmaker call took 0.75 to 0.9 s, and 74 to 82% of a bot's game ticks
had no fresh input from it.

The obvious lease (an owner trusts its last round for a while) isn't safe:
an owner cut off from the node that takes over, but not from its peers,
would keep answering from stale memory after a newer owner's writes. So
**the peers grant it**. A peer that acknowledges an owner's append promises
that epoch half a lease, by its own clock: a fence for a newer epoch gets
"not yet" (`PeerRes::Wait`) until then, and a store holds off every fence
that long after it starts (it can't remember what it promised). The owner
answers reads alone until the oldest of its peers' acknowledged sends plus
three quarters of the promise (room for clocks a fifth apart), renewing
past half. A takeover can take up to half a lease longer.

**Checked three ways.**
- The simulator (each node's clock up to 10% fast or slow) runs with leases
  on: 48,000 seeds across splits, lost disks, and a lost node, every
  invariant held, and over half of all reads used the lease. Its read check
  ("a read sees every write acknowledged before it was sent") catches both
  known-bad versions: peers that break their promise, and an owner that
  trusts 150% of it.
- TLA+: promises, leases, and the one clock assumption (a promise lapses only
  after the owner's lease has ended) are in `Replication.tla`, with a new
  invariant, `NoStaleRead`. 95.9 million states, no errors; both known-bad
  versions break `NoStaleRead`.
- The Docker cluster's kill, freeze, lost-disk, and split runs kept every
  object right.

**Rolled out in two steps,** because an older peer promises nothing: peers
always promise; owners trust leases only with `COMBD_READ_LEASE=1`, set on
the arena once every node ran the new version.

**On the arena** (a bot on this laptop against the match's own bot):

| | before | with leases |
|---|---|---|
| a player's ticks with no fresh input | 74 to 82% | 12.7% |
| ping (WebSocket, through the match; the network's is ~75 ms) | 435 to 529 ms | 149 ms |
| late ticks | 1.4 to 2.1% | 0.4% |
| a matchmaker call, server time | 750 to 900 ms | 160 ms |

Found on the way: forwarding gave up after a 1 s floor, too short for a
fresh object's start (a few bucket calls, about 250 ms each to R2 in
Europe): new matches failed with "owner didn't answer". The floor is 5 s
now (a dead owner is found by its silence). The bucket client keeps
connections now (40 ms a call to R2 from Oregon: its edge is near, most of
the 250 ms is R2 itself).

**Later the same night, the lease stuck.** A clean ping (a spectator's,
nothing else on the socket) on a live match took 416 to 1,050 ms, against
73 ms on a waiting one, and node 0 counted about one peer round per
WebSocket message. A read could use the lease only with nothing waiting
for a round. Once one read had to wait (say the lease lapsed between
messages), each of the match's 20 beats a second queued behind it, and a
round to Singapore (230 ms) is longer than the gap between beats: the queue
never emptied, so every read waited from then on. Locally a round takes no
time, so it never showed. The condition that matters is that every entry
made so far is acknowledged, which waiting reads don't change; under a valid
lease, what waits is proven too and goes first, keeping answers in order.
The simulator holds every invariant over 260,000 seeds, and breaks within 17
without the acknowledged-entries check.

| live match, same laptop bot | stuck lease | fixed |
|---|---|---|
| ping (clean, median) | 220 to 613 ms | **73 ms** (the network's: 75) |
| ping (the bot's own) | 453 ms | **84 ms** |
| a player's ticks with no fresh input | 79.5% | **1.2%** |
| late ticks | 1.0% | **0.0%** |
| reads under the lease, node 0 | 4 of 132 | **558 of 532** (beats too), 14 rounds |

**And a slow restore.** After the Singapore node restarted, the arena's
matchmaker (its home is there) didn't answer for about a minute: forwards
to it timed out ("owner didn't answer") and `/api/stats` failed through
every node. Its restore read 201 segments (one per upload, over 99 epochs)
one GET at a time, about 300 ms each from Singapore to R2 in Europe. They
go 16 at a time now. Across the next rollout, 437 of 438 calls to
`/api/stats` answered (the one that didn't was the moment the node went
down), and no forward timed out. A forward that times out now logs the
object and the node, and `/_combd/stats` counts `fence_waits`.

## Postgres through combd: how fast

`pg_bench` (not run by default: `cargo test --release -- --ignored pg_bench
--nocapture`; `PG_BENCH_CLIENTS` sets the clients, 64 by default): three
nodes on one laptop with nothing else running, a real Postgres (in Docker
Desktop), a pool of 8, 4 s each, 10,000 rows, by primary key.

| | ops/s | p50 |
|---|---|---|
| HTTP only, no database (`GET /hello`) | 7,432 | 8.5 ms (64 clients) |
| Postgres round trip (`SELECT 1`), 1 / 8 connections | 3,962 / 18,551 | 0.24 / 0.42 ms |
| Postgres direct, 8 connections: get / insert / update | 8,428 / 6,851 / 6,752 | 0.9 to 1.1 ms |
| combd, manager's node: get / insert / update | 4,543 / 4,580 / 4,762 | |
| combd, another node | 3,886 / 3,948 / 3,934 | |
| combd, all three nodes | 4,035 / 4,124 / 4,118 | |

It started at about 3,000 a second. The limit was never the event loops (six
a node instead of two changed nothing) or the clients (64 instead of 16 only
made the wait longer): it's how long each of the 8 connections is busy per
statement, and that's round trips to Postgres (0.42 ms each here, under
load). A pooled statement took three: prepare, execute, and the reset after
it. Direct takes two. In order:
- **The reset was two round trips** (`ROLLBACK`, then `DISCARD ALL`). It's
  one string now: `ROLLBACK` and the pieces of `DISCARD ALL` (which itself
  won't run in the implicit transaction a multi-statement string gets),
  checked clean in an open, an aborted, and no transaction. About +12%.
- **A statement cache per connection:** a statement it has seen skips the
  prepare. The reset now deallocates only the holder's own `PREPARE`s, so
  the cache survives it. A cached statement gone stale (its table altered,
  or the user deallocated everything) is prepared again, once; the test
  that checks this found the first version never recognized the error.
  About +15%.
- **The reset waits for the next statement** and goes out with it, back to
  back, one round trip (the connection's thread drives the async client
  for that). If no statement comes within 2 ms, it goes alone, so nothing
  the last holder left (an open transaction, an advisory lock) is held much
  past its lease: the guarantee is now "released within a moment", not "at
  release". About +20%.
- **A node keeps the members it gets** and lends them to its own next asks
  with no word to the manager; it hands one back after 5 s unused, or when
  the manager recalls it because another node's asks wait at max. On one
  laptop the manager is microseconds away, so this changed little here;
  across regions it takes a round trip to the manager (230 ms New York to
  Singapore) off every statement.

What's left: a connection is busy about 1.3 ms per statement through
combd against 0.9 ms direct, most of it Postgres doing the reset's ten
statements along with the real one. A faster client can't help much: the
same client does two round trips in 0.94 ms direct, against a 0.84 ms
floor. Not done: the reset could skip what can't have changed, if the
server ever says so cheaply.

ponytail: when a node's own members are gone at max, it borrows another
node's free one over the wire for every statement (members never move);
fine on one laptop, slow across regions. Starting one locally, in place of
a far free one, is the fix when that shows up.

## Upload batching

An object's writes are acknowledged once its peers have them; the bucket
upload was only ever background work, but it went as fast as the bucket
answered (one in flight per object, 20 to 30 a second, about $11 a day for
an object busy all day on S3). Now, while every peer is up, uploads wait:
up to `COMBD_UPLOAD_MS` (default 10 s; a warning in the log above 5
minutes) or until `COMBD_UPLOAD_BYTES` build up (default 1 MB, at most
16 MB, under the wire's frame limit so a takeover's tails fit one reply).
They still go at once when the bucket is how writes get acknowledged (no
peers, or a peer down), on a retry (it must be the same upload), and when
an idle object is being let go. The cost: an object's newest writes live
only on its two peers for up to that long. Losing both peers' disks in
that window loses them, as before, just over a longer window.

The spec (`spec/Replication.tla`) already lets an upload happen at any
time, so this changes no proof: waiting is one of the orders it checked.

**Simulator:** each seed picks a wait (none, 300 ms, 2 s, or 5 s with a
40-byte cap), so every test runs them all. A new known-bad version,
`wait_when_down` (uploads wait even with a peer down), loses no safety but
makes writes crawl; the liveness probe now has a clock (a write must get in
within ten leases after the faults stop; the worst seen is about 4 s, the
bad version takes 32 s) and catches it. 56,000 seeds across D2 and D3
with splits, lost disks, and a lost node: every invariant held. The
simulator also had a hole: it allowed one wiped disk per run *and* a node
lost for good, which is two lost disks. An object whose two peers were
exactly those rightly stopped (its acknowledged writes were gone from
everywhere), and the probe called it a bug. Now a run with a lost node
wipes no other disk.

**Two real-node bugs it found,** both older than batching and both
invisible while tails were tiny:
- **The wire dropped any frame over 64 KB.** rustls caps what waits to be
  sent at 64 KB by default, so a bigger frame failed the write and dropped
  the connection. A takeover's fence reply, now carrying seconds of
  writes, never arrived, and the takeover retried forever. Now uncapped,
  as plain TCP mode already was (frames are capped instead), and the mTLS
  test sends 1 MB each way.
- **The peer store compacted on every sync** once what it holds passed
  64 MB (with uploads waiting, 50 to 80 MB here). Each group commit then
  rewrote and fsynced the whole store: a run with a lost disk fell to 157
  writes/s with a p99 of 1 s. Now it compacts past twice what the last
  compaction left.

**Real nodes** (`bench/cluster`, three Docker nodes, 32 clients, 50
objects, 15 s each; `UPLOAD_MS` sets the wait). Every run kept
`acknowledged <= count <= acknowledged + failed` for every object, both
ways:

| run | no wait: writes/s, p99 | 10 s: writes/s, p99 |
|---|---|---|
| steady | 2,932, 33 ms | 2,190, 36 ms |
| SIGKILL a node | 1,690, 68 ms | 1,699, 96 ms |
| freeze a node 4 s | 1,423, 53 ms | 1,870, 55 ms |
| SIGKILL, lose its disk | 1,411, 77 ms | 1,660, 87 ms |
| steady (split run) | 2,224, 45 ms | 3,063, 24 ms |
| cut from the bucket | 1,683, 54 ms | 1,367, 133 ms |
| cut from the wire | 1,195, 72 ms | 1,444, 115 ms |
| cut from both | 689, 160 ms | 538, 213 ms |
| **bucket uploads, kill runs** | **45,935** | **3,766** (12x fewer) |
| **bucket uploads, split runs** | **44,957** | **796** (56x fewer) |

Throughput moves within the noise of a shared laptop (it swings 30% run to
run); p99 is higher during a fault (a takeover now has seconds of writes to
fetch from a peer, not a few). A 1 s wait measured as well as no wait in
every case, with 9x fewer uploads.

**What it costs now:** entries are whole 4 KB pages, so a write-heavy
object fills 1 MB in a few seconds: one upload every ~2.5 s at 50 writes/s,
about $0.17 a day on S3 or R2 (was about $11). A light object uploads every
10 s: about $0.04 a day. Raising `COMBD_UPLOAD_BYTES` cuts the heavy case
further, at the price of more memory: an owner holds up to that much per
busy object, and each peer the same.

## Secrets, sealed in the catalog

Secrets live in the catalog, sealed (`spike/src/secrets.rs`). Each app
gets its own data key with its first secret, sealed by the fleet's master
key (a file of 64 hex characters, `COMBD_MASTER_KEY_FILE`, refused if
anyone but its owner can read it; `combd-spike master-key <file>` makes
one). Each secret is sealed by the app's data key with AES-256-GCM, and
the app and the secret's name are bound in, so a sealed value copied to
another name or app won't open. A node opens them only in memory, when it
loads the app, and hands them to its instances as env vars. The bucket and
every node's copy on disk only ever hold sealed bytes.

- **Write-only:** `PUT` and `DELETE /v1/apps/<org>/<app>/secrets/<name>`,
  under their own scope (`secrets`). Nothing reads one back. The CLI reads
  the value from stdin, never the command line.
- **Checked like vars:** with a version live, a secret must be declared and
  valid for its type, and one a process requires can't be deleted. Errors
  name the secret, never its value.
- **The data key is written once:** the catalog's check refuses a second
  one (two nodes making one at once can't both win) and refuses deleting
  it. The loser uses the winner's key.
- `submit` now answers only after the node's copy has the write, so a
  caller reading right after sees its own change.

**Tests:** `deploys_reach_every_node` sets a secret before the first deploy,
deploys an app that needs it, and every node serves it; a change through
another node reaches all three; a bad URL, an undeclared name, and deleting
a required secret are refused (the error never shows the value); then it
reads every object in the bucket and every node's catalog copy and finds
no secret in the clear. The Postgres app's URL is a secret now.
`sealed_secrets_open_only_where_they_belong`: another master key, no
master key, a value moved to another name or app, and one flipped bit are
all refused.

ponytail: no key rotation yet (new master key: open each data key with the
old, seal with the new), and no KMS (the master key would live there, and
nodes would ask it to open data keys). No env per secret yet: the plan's
environments bind it in when they land.

## The catalog: core and simulator

`spike/src/catalog.rs` is the catalog's decision core (no I/O), and
`spike/src/catsim.rs` runs it on three nodes against a fake bucket where a
deleted key is simply gone: calls of 1 to 50 ms (sometimes 500), lost
requests and replies, lost, repeated, and reordered pushes, crashes that
keep or lose a node's copy, and random snapshots that trim the log. An
oracle checks the spec's invariants after every step, and, once the faults
stop, that every node has caught up to the whole log (which the spec
doesn't check). A lost reply to a write is followed by reading the slot
back (entries carry an id), never a blind retry.

**It found a hole in the protocol and the spec.** A node catching up read a
trimmed slot just as a slow writer filled it with an entry whose own check
would reject it, and applied it: its copy held a change that was never in
the log (seed 703 of the first long run). The spec let only writers meet a
trimmed slot; readers read the true log, not the bucket's bytes. Now a
reader holds what it read, reads the snapshot again, and if it covers any
of it, starts from the snapshot. Both the code and the spec have it, and a
seventh known-bad version (no second read) must fail in both.

`combd-spike catsim 5000`: 518,510 acknowledged writes, 45,000 snapshots,
15,000 crashes, every invariant held, every node caught up. Each of the
seven known-bad versions is caught, the same way TLC catches it, except
"trim too early", which the code shows is worse than the spec says: it
loses acknowledged writes, not just strands a node.

## Deploys

combd knows nothing about people: only tokens and what each may do. Users,
orgs, invites, logins, and roles live outside it (your CI and identity
system, or a hosted dashboard that holds the tokens and calls the same API).

- **The bucket** (`spike/src/deploy.rs`): `apps/<org>/<app>/versions/<hash>`
  holds a bundle (the manifest and `modules/*.wasm`, nothing else, so the
  dev env file can't ride along), named by its SHA-256; `current` says
  which is live and moves only by compare-and-swap; `vars` holds the vars.
- **A deploy is checked in full before it's live:** the manifest names the
  app it's deployed as, every module loads, every var resolves, and no
  domain clashes with the node's other apps. A bad deploy never moves the
  pointer.
- **Every node** (`serve-fleet`) loads every app at start, then every 2 s
  swaps in what changed (a version, or vars), one app at a time: an app
  that won't load is skipped and tried again, the rest go on.
- **On a running node** the app table is append-only: the new version's
  processes are added and the old ones retired, so in-flight work finishes
  on the old code, new requests get the new, and an object's next request
  runs the new code on its old storage.
- **The admin API** (`spike/src/admin.rs`), on its own port: deploy,
  status, rollback (an earlier version, checked again), take down, set
  vars (checked against their declared types; the CLI's `"8"` is an int
  only if the var is). Tokens are stored as SHA-256 hashes and compared in
  constant time; scopes look like `acme/shop:deploy`, `acme/*:vars`,
  `*/*:*`. The token is checked on the headers, before any of a body is
  read. Without TLS it listens on localhost only; beyond it, TLS with the
  fleet's certificates.
- **The CLI:** `combd-spike admin-token <scope>...`, `deploy <dir> <org>
  <url>`, `status`, `rollback`, `remove`, `vars <org>/<app> <url> K=V...`.
- **A node refuses to start if its id is in use:** it watches its own
  heartbeat for half a lease, and if it moves, someone else has the id.

**Test `deploys_reach_every_node`** (three D3 nodes that start empty, all
through node 0's admin API over TLS): v1 reaches every node; counts hold
across them; no token (401), a token for another app or org (403), and a
guessed one are refused, and a 60 MB upload with a weak token is refused
at once, before its body; a var reaches every node, and an undeclared one
is refused; a bundle whose manifest names another app is refused and the
pointer stays; v2's new route reaches every node and the object keeps its
count; rollback to v1 and forward again; a second app on its own domain;
taking the first down makes it a 404 everywhere while the second serves;
and a second node 1 can't start. Also `a_deploy_swaps_a_running_app`
(in-flight work across a swap), and the bundle decoder against 100,000
mangled bundles.

Found while building it: refusing before reading a body, then closing,
reset the connection, and the client could miss the answer. The server now
reads (and drops) up to 1 MB for 2 s after a refusal, never all of it.

A bug I'd left: fleet nodes started no Postgres pools at all, so a
deployed app could never reach its database. Now each app carries its pool
settings, and a node starts any pool it's missing whenever it takes a new
app table (at boot, or a deploy; a changed config gets a fresh pool). The
deploy test deploys an app with a pool and queries it on all three nodes.

Not yet: secrets, the AOT tier for deployed modules (it runs only modules
built in; deploy nodes run the interpreter), and a push to peers on deploy
(they poll every 2 s).

## Many tenants on one node (shared process)

`combd-spike serve-apps <engine> <root> <port>` serves every app under
`<root>/<org>/<app>/`. The org is the tenant, and comes from who deployed
(in a fleet, the account; here, the folder), never from an app's manifest,
where anyone could write any org. Everything is scoped to its app:
- objects are `<org>/<app>/<process>/<id>`, stored under `<org>/<app>/`;
- pools are `<org>/<app>/<pool>` (a cell still says `"db": "main"`);
- env vars and secrets are per app; `call`s and leases resolve only inside
  the caller's app, so there is no way to even name another app's process;
- each app lists its `domains`; its routes answer only on them, and no two
  apps may share one (an app with no domains is allowed only when alone).
- The same module bytes in many apps load once (hashed).
- **Fairness is per org:** when more than 64 slices are queued and more
  than one org has work, each round gives every org an equal share. An org
  that needs less gets all it needs. An org with 50 apps gets one share.

Object file names are now a SHA-256 of the name (in the app's folder), so
there's no length limit. **And a durability gap closed:** a new object's
folders (and the folder holding its file) were never fsynced, so on some
file systems a power cut right after its first answer could lose the new
file. Now its first commit fsyncs them before any answer leaves.

**Test `six_tenants_share_a_node_and_nothing_else`:** 3 orgs, 2 apps each,
all with the same `api`, `worker`, and `room` processes and the same object
id: each sees only its own env var, count, folder, and processes; none can
use the one app's pool; unknown hosts get 404s; six apps load two programs.
Fairness, 100 slow requests from acme against 6 from globex on one loop:
globex's batch finished in about 0.54 s and acme's in 1.41 s (three runs).
With fairness turned off, both finished together at 1.62 s, which the test
fails. `apps_must_not_share_domains` covers the load-time checks.

**Six tenants on three nodes** (`six_tenants_on_three_nodes`, D3 with two
peers per object): all three nodes serve all six apps, and requests go to
every node in turn. Every node answers every app with its own env var;
every app's `room/r1` takes increments through all three and they agree on
its count; `call` from any node reaches the object's owner, wherever it is,
and `send` too; then node 0 stops, and the other two take over every app's
objects with every write and go on. Until the stop, every request must
succeed on the first try.

That last rule found a real race. Three nodes asked at once for a new
object: one won the claim, and the requests still waiting on the losers
were answered "failed, retry" (503). A client retried, but a `send` has
nobody to retry it, so half the sends were lost. The ownership core
couldn't tell "failed" requests that may have run from ones that never
did, so the host couldn't safely retry either. Now it can: requests that
were still waiting are answered `NotRun`, and the host routes them again
(up to three times), so nobody sees the race. The simulator checks the new
answer strictly: a request answered `NotRun` must never have run (133,000
such answers in 4,000 seeds, with splits and lost disks: every one held).
Turning the retry off makes the test fail again.

**And two old test bugs, found when this test failed only in the full
suite.** Every fleet test named its bucket with `Instant::now().elapsed()`,
which is always about 0, so all fleet tests in a run shared one bucket and
met each other's nodes 0, 1, and 2: a stopped node looked alive because
another test's node 0 kept its heartbeat going (1,184 fences in 30 s). And
test ports came from the process id, so one fleet test's wire could land on
another's. Each test now gets its own bucket, and its ports from the OS.

Not yet: one process per tenant (needed before strangers share a node:
cells in one process can leak through CPU timing), per-org caps on memory
and in-flight requests, per-app stats, and environments in the name.

## Event cells: many requests per instance

A Worker that exports `on_request` is an event cell (`guest-ev/`), like a
Node process: one instance serves many requests at once. The host calls
`on_request(id, ptr, len)` for each new request and `on_event(ticket, len)`
when something the cell waited on is back (`ev_read` copies its data). The
async imports return a ticket at once: `timer_async(req, ms)`,
`pg_open_async(req, ...)`, `pg_query_async(req, handle, ...)`, plus
`pg_close_async`. `respond(req, answer)` answers; work may go on after it
(waitUntil). The guest keeps a table of ticket -> what to do next: its own
small event loop.

The host runs one callback at a time per instance, through the normal
slices. A new request goes to the least busy instance under the cap
(`COMBD_EV_MAX`, default 32), or a new one starts. Idle extra instances go
away; the first stays warm, with its state. A request is done when it has
answered and has nothing open, and then its Postgres handles close. 30 s to
answer (504 after), 60 s in all. A trap fails every unanswered request on
that instance, closes its handles, and the next request gets a fresh one.
Work after `respond` is best effort: a crash loses it, and nothing retries.
Works on all three engines; nothing needs to pause mid-call.

**Test `event_cells_on_the_interpreter` / `_on_aot`** (cap 8): 30 requests
that each wait 400 ms finish in about 400 ms on 4 instances; state is
shared across requests; waitUntil answers first and counts later; a crash
fails the 5 requests waiting on its instance and the next request gets
fresh state. **`event_cells_postgres_*`**, against a real Postgres: 10
transactions that each pause 300 ms halfway run interleaved on one instance
in about 300 ms; one that fails halfway rolls back alone; every pinned
connection goes back.

An instance's memory is its own: not shared with other instances, other
event loops, or other nodes (like a global in a Worker's isolate). State
that must be shared goes in a Durable Object.

Not yet: event-mode Durable Objects.

## Security: memory is always zero

Every byte a new cell can see starts as zero. Its buffer comes from `calloc`,
whether the kernel zeroes pages lazily or glibc zeroes them with `memset`.
Safe Rust can't expose leftover memory at all (that takes `unsafe`, which is
banned). The test `fresh_cells_never_see_old_memory` fills dead cells with a
pattern and checks that new cells never see it.

## A session per WebSocket: what a connection costs (2026-10-19)

A `[ws.x]` process starts a session per socket, in memory on the loop that
holds it, with no bucket (`plan.md`, "Socket processes"). Idle past
`hibernate_ms`, it keeps only the bytes it hands back and its cell goes;
objects push to its socket without waking it.

**Idle connections, measured** (`ws-hold`: N real sockets to one server,
each sending one message, then the server's RSS; Linux in Docker, one
event loop, the million app, each socket watching chunk 0 and sent its
3.9 KB snapshot):

| 20,000 sockets | Server memory per socket |
|---|---|
| `hibernate_ms = 2000`, while connecting (viewers awake) | up to 45 KB |
| `hibernate_ms = 2000`, settled (all asleep) | **4.1 KB** |
| `hibernate_ms = 0`, while connecting | 2.8 to 3.0 KB |
| `hibernate_ms = 0`, settled | **3.8 KB** |

An Erlang process starts at 2.7 KB; a Phoenix connection is a few of them
plus buffers. So an idle combd connection is at BEAM's scale already: the
socket, the session's host record, its ~30 bytes of state, and the chunk's
subscription entry. The "settled" rise is most likely heap left behind by
the cells that ran during the burst (glibc keeps small freed blocks, so
RSS can't tell). (A first try at releasing a connection's grown buffers
seemed to do nothing, 4.10 to 4.11 KB, and was dropped; RSS was the wrong
measure: see below, where it's worth 1.6 KB a connection.)

For scale: eieio's One Million Checkboxes peaked at over 20,000 unique
visitors an hour (he gives no concurrent count), on 8 worker VMs, an nginx
box, and a 4-CPU, 32 GB Redis. 20,000 sockets held open at once here take
about 80 MB on one core. That's idle sockets only: fan-out under load (his
millions of checks an hour, pushed to thousands of watchers) is the next
measurement, with bots.

**Live cells, measured** (`density-app`: N cells of a real module,
started, then sent real requests; Linux in Docker):

| Module | Started | After real work |
|---|---|---|
| million `viewer` (its own small module: 64 KB stack, 13 KB of data) | 20.9 KB | 41.2 KB (its 4 events) |
| million `web` (serves the page) | 41.0 KB | 109.7 KB (one page) |
| arena `lobby` | 65.2 KB | 163.6 KB (one page) |

At the start it's mostly the module's static data, copied into every cell
(13 KB for the viewer is 4 pages of 4 KB; the interpreter's own state is
about 5 KB). After work it's stack and heap pages that a warm cell keeps,
since Wasm memory never shrinks. Those are the next two steps: a shared
copy-on-write starting image, and a reset after each request for workers.

(macOS, with 16 KB pages and its own calloc, gave 1.1 to 1.3 MB per web
cell after one page: grown memory is copied into a new buffer, and reused
blocks get zeroed by hand. The Linux numbers are the ones to compare.)

**A shared starting image** (`combd-mem`, the interpreter's only `unsafe`
code): a module's data segments are written once, at load, into an
in-memory file (memfd; an unlinked temp file on macOS), and every cell maps
it copy-on-write. Pages a cell only reads stay shared; its first write to
a page copies that page. Growth stays inside a reservation (address space,
not RAM). Memory is now measured as PSS (a page shared by N cells counts
1/N; RSS would count it N times). Linux, same runs as above:

| Module | Started, before / after | After real work, before / after | Start time |
|---|---|---|---|
| million `viewer` | 20.9 / **0.9 KB** | 41.4 / **25.2 KB** | 9.6 / 2.1 us |
| million `web` | 41.0 / **1.0 KB** | 120.2 / **28.7 KB** | 10.9 / 2.3 us |
| arena `lobby` | 65.2 / **1.2 KB** | 177.2 / **72.9 KB** | 20.0 / 2.3 us |

A fresh cell is now about 1 KB, under a BEAM process's 2.7 KB. Serving the
page the first time got much faster too (million 394 to 16 us a cell,
arena 436 to 62): the page is read from shared pages, not from copies just
written. What a cell keeps after work is pages it really wrote (stack,
heap): reset per request is the next step for workers. On macOS (busy
laptop, alternating builds) a bare spawn is slower, 1.5 to 3.6 us (the
mmap calls), but a fresh cell serving its first hello is faster (156 to
115 to 130 us), and so is a crash restart (155 to 110 us); compute is the
same within noise. The spec testsuite (30,497 pass, 0 fail) and 20,000
differential fuzz seeds against Wasmtime agree. A cell is now two or three
mappings (the image, its own pages, the rest of its reservation), so
Linux's map limit (`vm.max_map_count`) is reached at a third as many live
cells as before; hibernated sessions have none. One region for every cell
(the next item on the list) fixes both.

**Down to BEAM's size: 2.24 KB a connection.** Two things, measured with
glibc's own count of heap in use (`heap_in_use` in `/_combd/stats`), since
RSS keeps freed memory and can't tell:
1. **The host freezes idle sessions** (`hibernate = "auto"`, the default
   for ws): an idle cell is kept as what differs from its module's image,
   its globals, and any changed table, and thawed into a fresh cell on the
   next event, with no app code. For a wasm-ld module (it exports
   `__data_end` and `__heap_base`, with nothing below where global 0, the
   stack pointer, starts) the dead stack below the stack pointer is left
   out. Run headers are LEB128. A frozen viewer: 1,141 bytes (1.8 KB before
   leaving out the stack and shortening the headers). A freeze cut short
   anywhere is refused (it says how many runs it holds).
2. **A connection gives back big buffers once they're empty** (over 1 KB):
   each socket here was sent a 3.9 KB snapshot, and kept a buffer that size.

20,000 sockets, Linux, one loop, `hibernate_ms = 0`:

| Viewer | Heap in use per socket | RSS per socket |
|---|---|---|
| keeps its own state (`hibernate = "app"`, 30 bytes), before | 3.89 KB | 3.78 KB |
| the same, with the buffers given back | **2.24 KB** | 2.20 KB |
| frozen by the host (`"auto"`, no app code), before | 4.92 KB | 4.80 KB |
| the same, with the buffers given back | **3.22 KB** | 3.11 KB |

An Erlang process starts at 2.7 KB. Here a whole connection (the socket,
its session, its state, the chunk's entry for it) is 2.24 KB, and it stays
sandboxed, preemptible, and its own crash.

**A fresh cell for every request** (`fresh = true` on an http process):
nothing one request leaves can reach the next. It costs what a new cell
pays, page faults for every page it touches plus whatever the app builds
on start, so it's opt-in, not the default (Linux, `fresh-bench`):

| Request | Warm cell | Fresh cell each time |
|---|---|---|
| the test guest, `GET /api/users/42` (it builds 100 users at start) | 5.2 us | 131 us |
| million's page | 4.3 us | 24.7 us |

Warm cells stay the default, as Cloudflare's isolates do: apps keep
caches in them. Memory creep is bounded by the idle trim instead.

## What a node holds: Caddy, HAProxy, or combd alone (2026-10-20)

A node like the arena's (a container capped at 1 GB and one CPU, Linux in
Docker on an Apple Silicon Mac, so arm64, not the droplets' x86), running
the million app on one loop, with `ws-load` in another container holding
N idle WebSockets through TLS (a test CA for `million.localhost`), each
sent its 3.9 KB snapshot. Memory is the container's cgroup total: every
process plus the kernel's socket memory, as a node pays for it.

| In front of combd | The node per connection | Where | The most in 1 GB |
|---|---|---|---|
| Caddy 2.8.4 (the arena's today) | **~106 KB** | Caddy ~97 KB | **~9,500** (killed for memory at 9,557) |
| HAProxy 2.6.12 | **~33 KB** | HAProxy ~21 to 23 KB, the kernel ~12 KB (three sockets) | **~28,700** (killed for memory) |
| HAProxy, `tune.bufsize 4096` | ~33 KB | the same: it's TLS state, not buffers | the same |
| nothing (combd alone, no TLS) | **6.0 to 6.5 KB** | combd ~2.4 KB, the kernel ~3.9 KB (one socket) | ~160,000 (40,000 held in 236 MB) |

So on the arena's nodes Caddy is the limit, not combd: combd's share of a
connection behind it is about 3%. HAProxy triples what a node holds;
combd alone holds about 17 times what it does behind Caddy. Ending TLS in
combd (`plan.md`, "The edge") adds rustls's state for an idle connection
to the 6 KB: not measured yet, but a few KB would put a 1 GB node at
70,000 to 120,000. These are idle connections; fan-out under load is
another measurement.

## Single mode: combd ends TLS, and holds the edge itself (2026-10-20)

`COMBD_TLS_CERT` and `COMBD_TLS_KEY` (PEM files, reloaded when the
certificate's file changes; a bad one keeps the old) put rustls on the main
port, TLS 1.2 and 1.3, ALPN `http/1.1`. A socket handed to another loop
takes its TLS with it. `COMBD_REDIRECT_PORT` answers plain HTTP with a 301
to HTTPS (only a plain host name goes in the `location`). The port is
taken with `SO_REUSEPORT`, so a new combd can listen before the old stops.

**Memory, the same 1 GB node as above:** ~10 KB a connection (combd ~6,
the kernel ~4); 40,000 held in 390 MB, so about 100,000 in 1 GB. Caddy's
~106 KB held 9,500; HAProxy's ~33 KB, 28,700. It was ~14 KB with rustls's
own buffering, which keeps a 4 KB read buffer for every connection, idle
or not; with its unbuffered API (`tls.rs`), the encrypted bytes in and out
are ours, and given back once empty. TLS is ~3.9 KB of an idle connection
now (it was ~7.6): rustls's protocol state and keys.

**The edge's limits** (`Limits`, node-wide): a body (`COMBD_MAX_BODY`, 1
MiB: 413), the head (`COMBD_MAX_HEADER_BYTES`, 16 KiB, and
`COMBD_MAX_HEADERS`, 64: 431), a WebSocket message (`COMBD_MAX_WS_MESSAGE`,
1 MiB: 1009), and time: a request must arrive within `COMBD_READ_TIMEOUT_MS`
(30 s: 408; slowloris), a kept connection idle past `COMBD_KEEPALIVE_MS`
(75 s) and a TLS handshake past `COMBD_TLS_HANDSHAKE_MS` (10 s) close.
There were no timeouts at all before: behind Caddy, they were Caddy's.

**Found on the way:** a `Transfer-Encoding` request was read as having no
body, so its chunks would be read as the next request (smuggling, RFC 9112;
behind Caddy too). It's refused now (400), as is a `Content-Length` of
anything but digits (`+5` parsed). And close codes no endpoint may send
(0, 1004 to 1006, 1015 to 2999) are a protocol error now (1002).

**The Autobahn TestSuite** (the WebSocket conformance suite), on an echo
`ws` process (`bench/autobahn`), plain and over TLS: every case passes but
those it can't: 238 OK, 11 non-strict, 3 informational of 252 plain (all
but 9.*); the 9.* cases over 1 MiB are refused with 1009, by design (and
the throughput ones under it pass). The 9 failures of the first run (close
codes, 7.9.*) are fixed. Non-strict: a bad frame right after a good message
can fail the socket before the good one's echo is out, and invalid UTF-8
in a fragmented message is caught when it ends, not at the first bad
fragment. A socket refused while its client was still sending sometimes
got a reset, not its 1009 (once in a run, over Docker's network): closing
with its bytes unread sent one, and its kernel threw away what we'd sent.
Now a closing connection lingers: all we owe goes out, we stop sending,
and read and drop the rest for up to 2 s. All 24 of the 9.* cases over
the limit get 1009, plain and over TLS.

**testssl.sh 3.2:** TLS 1.2 and 1.3 only; AEAD ciphers with forward secrecy
only; not vulnerable to anything it tests (Heartbleed, ROBOT, CRIME, POODLE,
SWEET32, FREAK, DROWN, LOGJAM, BEAST, LUCKY13, RC4). Its two "not ok"s are
the test certificate's (our own CA; no CRL or OCSP).

## A load test on a live node: 105,000 connections on a 1 GB droplet (2026-10-20)

n0 (DigitalOcean, nyc3, 1 vCPU, 1 GB RAM, 1 GB swap; combd in single mode,
one loop) against three load droplets in the same region (`bench/load`,
their own Terraform state, destroyed after), each holding 35,000 million
viewers over TLS (`ws-load`, the real Let's Encrypt certificate), each
sent one range message (a subscription and its 3.9 KB snapshot).

**What broke first, and was fixed on the way:**
1. **Files, at ~1,000 connections.** The container's soft open-file limit
   was 1024: combd couldn't open objects' storage, chunk 0 never restored,
   and the node stopped answering until the load stopped. combd now raises
   the limit to its ceiling (524,288) and keeps connections under three
   quarters of it.
2. **The kernel's connection tracking, at ~8,000.** Docker's rules turn it
   on, and a 1 GB droplet gets 8,192 entries: past that the kernel drops
   packets (new connections, and the node's own DNS and bucket traffic).
   Any server on these droplets stops there, Caddy too. Fixed on n0 by
   hand: 80 and 443 aren't tracked (`NOTRACK` in the raw table; combd uses
   the host's network, no NAT), and 65,536 entries for the rest.
3. **The load droplets' own memory.** A client that never read its sockets
   left 3.9 KB of each in the kernel and ran out of memory at ~34,000 (the
   OOM killer took sshd and the client). `ws-load` reads and drops now.

**Then:**

| On n0 | RAM used | Swap used | combd's measure | Ping |
|---|---|---|---|---|
| nothing | 390 MB | 48 MB | 12 MB | 0.24 s |
| 35,000 | 704 MB | 48 MB | 282 MB | 0.24 s |
| 70,000 | 847 MB | 267 MB | 409 MB | 0.24 s |
| 105,000 | 855 MB | 518 MB | 379 MB | 0.24 s |

105,000 held, none closed, none refused, and n0 answering as fast as idle
throughout (the ping is from Oregon). About 9 KB a connection in all
(RAM and swap, the kernel's included), as measured locally (~10). But it
took swap: in RAM alone a 1 GB droplet holds about 60,000 (the OS, Docker,
and the rest take 390 MB before anything else). The sessions are asleep,
so their memory is cold, and the kernel moved it to swap without harm.

**What it showed that isn't right yet:**
- **The budget doesn't count swap.** combd's measure is the cgroup's
  memory, and pages moved to swap leave it (409 MB at 70,000, 379 at
  105,000): the 85% and 95% lines were never reached. It should count the
  cgroup's swap too.
- **Everyone leaving at once took 7.5 minutes,** with the node not
  answering (and 49 new connections refused). Each closing socket woke its
  session (from swap, often) to unsubscribe, and million's chunk pruned its
  subscriptions on every request: with 105,000 on chunk 0, a scan of
  105,000 per request. The chunk should prune on a timer; a sleeping
  session whose app doesn't need to hear its close shouldn't be woken for it.
- **The droplet's own 390 MB.** Trimming Ubuntu's extras would fit more in RAM.

## Clicking: 10,000 pages on million, and batches (2026-10-20)

Each of 10,002 pages (`ws-click`, three load droplets in nyc3, against n0)
watches a random 1,000 boxes, renews that every 20 s as the page does,
clicks 1 to 30 of them 1 to 20 s apart, then sits and watches. Every click
is timed to its echo (the chunk sends each change to every watcher, the
clicker too), and one with none in 10 s counts as lost. At the peak that's
~620 clicks a second, each to ~160 watchers: ~100,000 pushes a second.

**First run: n0 fell behind and never came back.** 0% idle, 65% of it in
the interpreter. ~8,000 changes a second delivered of the ~100,000 asked;
echoes 4 to 10 s late or lost, thousands every 10 s. Then it fed itself:
renewals arrived after their subscriptions (60 s) ran out, so each was a
new one, and a new watcher got all 15,625 boxes hex-encoded again. Every
socket stayed up and memory stayed under budget, and once the clients
had gone, n0 still ran at 100%, its chunks' queues at ~320 MB: hours of
work. Restarted. What was wrong, and what changed:
- **Nothing pushed back** (a combd bug): queues grew without end, and the
  memory budget never saw it. Now `mailbox` (default 1,000): past it, a
  request is refused at once, 503 `overloaded`.
- **The chunk's own costs** (the app): a change built as JSON for each
  watcher, in Wasm; the boxes hex-encoded a `format!` per byte. Fixed.
- **One push per watcher per click** (both): now **batches**. An object
  that exports `handle_batch` takes what's waiting at once (up to `batch`,
  100), one transaction, an answer each; million's chunk sends each watcher
  one message for the whole batch. Batches form only from a queue, so
  they grow with the load.

**One core, no fleet** (a load droplet as the server, one loop, the same
10,000 pages from the other two):

| Chunk | Changes delivered | Echo p50 / p99 | Lost every 10 s |
|---|---|---|---|
| as it was | ~8,000/s | 1 to 10 s | ~9,000 |
| its costs fixed | ~15,000/s | 3 to 9 s | ~5,000 to 8,000 |
| and batches | ~140,000/s | 1.3 s / 2.2 s, falling | **0** |

**Second run, live, with batches.** n0 ran every chunk this time (after
its restart each started on the first node to ask, and every viewer was on
n0), so all 10,000 sessions, all 64 chunks, and the fleet's replication of
each batch were on its one core. 10,005 connections, none refused, memory
345 to 365 MB of 700. At the peak, echoes p50 4 to 6.6 s and 3 to 10% lost
(2,989 requests refused by full mailboxes); as the clicking slowed, p50
went to 270 ms with none lost, and sitting and watching kept n0 at ~50%
(the renewals: each wakes a sleeping viewer). Within 15 s of the clients
leaving, n0 was idle.

**Then it crashed.** A socket closing while its session ran a request
(`on_close = "drop"`) ended the session mid-request, and when the request
finished, the loop panicked: 10,000 leaving at once found it. Fixed, with
a test that panics without the fix.

**What's left:** n0 at the peak was ~44% interpreter, most of it viewers
waking (three runs each: wake, the message, sleep again) for clicks and
renewals; the chunks belong spread over the fleet, not on the node that
asked first; a push to another node is still one wire frame per watcher;
and `heap_in_use` counts cells' reserved address space (6.9 GB on a 1 GB
node), not the heap.

## Compiled: the same 10,000 pages again (2026-10-21)

Million's chunks, total, and viewers compiled (builds made by a builder
on a machine of ours, signed, loaded by the nodes); its page interpreted
(it waits on `call`, which compiled code can't yet). On one core (a load
droplet as the server, the other two the clients):

| | Interpreted | Compiled |
|---|---|---|
| CPU in app code, the run | 61 s (chunk 33, viewer 28) | 12 s (chunk 8, viewer 4) |
| Clicks lost | up to ~1,000 in a 10 s window | none |
| Echo p99 | 2.4 to 8.5 s | 2.1 to 2.6 s |
| A viewer, awake (2 messages in) | 89.5 KB, 34.7 us | 66.9 KB, 7.6 us |

Compiled first ran out of memory: a compiled cell's memory was a Vec, and
growing it wrote every new page. Now it's combd-mem's, as the
interpreter's (295.7 KB a viewer was 66.9).

Live on n0 (every chunk and viewer there, the fleet replicating each
batch to Frankfurt and Singapore): ~150,000 to 170,000 changes a second
delivered (twice the interpreted run with batches), 2% of clicks lost
(3 to 10% before), nothing refused, and n0 idle within seconds of the
clients leaving. Echoes at the peak were no better (p50 5 to 6 s): the
work moved. At the peak n0 was 43% kernel (a TLS record and a system
call per socket per frame), 24% combd's host code, 24% libc (allocating,
copying), and 8% the compiled apps; and each batch waits on its peers
(84 and 244 ms away). Coalescing a socket's frames per loop turn found
1.02 frames a flush: a socket gets ~3 a second, one per chunk batch.

## Where a node's CPU goes, and kernel TLS (2026-10-21)

One 1 vCPU node, million compiled, every page clicking. A one-node ramp
(fresh server per step, six client droplets):

| Users | Worst p50 | Worst p99 | Lost | Out of |
|---|---|---|---|---|
| 10,000 | 425 ms | 1.6 s | 0% | (69% CPU) |
| 20,000 | 2.1 s | 8.7 s | 0.13% | CPU |
| 30,000 | 5.6 s | 10 s | 43% | CPU, memory |
| 40,000 | | | 88% | memory (swapping) |

Before that ramp, million's chunks learned to send on a beat (150 ms) and
combd to send one frame to many sockets (`"ids"`): at 10,000 users the
echo went from p50 ~1.6 s, p99 ~2 s to p50 ~300 ms, p99 ~650 ms.

Where the CPU goes (the whole system, perf, 10,000 users; 20,000 alike):
kernel 75%: `sendto` 45% (TCP transmit 27%, the device queue 13%),
receiving 24% (network interrupts, the ACKs), the NIC kicks 4%, system
call entry 3%, copying in 0.3%. So the ceiling is packets a second on a
small VM: not the system calls, not the crypto.

Kernel TLS (`COMBD_KTLS=1`): a connection's session goes to the kernel
once its handshake is done. 20,000 idle TLS WebSockets on one node:

| | Memory a connection (combd's and the kernel's) |
|---|---|
| TLS in combd (rustls) | 7,533 bytes |
| Kernel TLS | 3,714 bytes |

Half: at a 550 MB budget, ~145,000 idle connections where ~73,000 fit.
The arena's nodes run it. (Corrected below: this counted only combd's
cgroup, and kTLS's own ~2.5 KB a connection is the kernel's, outside it.
It moves the memory rather than saving it.)

io_uring for the sends (`COMBD_URING=1`): one submission for every socket
with frames waiting. 10,000 active users, kTLS on, a fresh server each
run, 3 minutes:

| | Server CPU | Worst p50 | Worst p99 | Lost |
|---|---|---|---|---|
| Without, round 1 | 66% | 563 ms | 2.3 s | 0% |
| With, round 1 (98% of frames through the ring) | 67% | 424 ms | 1.4 s | 0% |
| Without, round 2 | 65% | 411 ms | 1.5 s | 0% |
| With, round 2 | 63% | 432 ms | 2.0 s | 0% |

No difference beyond the noise: the system calls weren't the cost, TCP's
work per packet is. Off by default; the arena doesn't run it.

The same ramp as above, now with kTLS, io_uring off and on at each step
(a fresh server each run, 3 minutes, 1 GB swap):

| Users | Lost (off / on) | Worst p50 (off / on) | Swap used (off / on) |
|---|---|---|---|
| 10,000 | 0% / 0% | 950 ms / 840 ms | 1 / 9 MB |
| 20,000 | 0.45% / 0.91% | 3.7 s / 2.7 s | 87 / 83 MB |
| 30,000 | 83% / 51% | 7.9 s / 5.7 s | 511 / 245 MB |
| 40,000 | 87% / 71% | 9.8 s / 6.4 s | 511 / 312 MB |

The wall is still between 20,000 and 30,000, both ways. At 30,000 the
server swaps and its CPU falls toward idle (it's waiting on pages, not
working): memory, not CPU.

Where the memory went (a 1 GB droplet, sampled every 10 s, then the kernel's
slab caches at 20,000 with kTLS off and on):

- **Before combd starts:** ~600 MB of the 961 MB is free. dockerd 106 MB,
  containerd 57 MB, multipathd, unattended-upgrades and packagekitd ~70 MB.
- **At 30,000:** the kernel's own unreclaimable memory went from 44 to
  361 MB, and the TCP queues hold 157 MB: receive queues 62 MB (clicks
  combd hadn't read yet, because it was already swapping), send queues
  only 3 MB. So not frames piling up for slow clients.
- **combd's count misses most of the kernel's part.** It reads its
  container's cgroup, and the cgroup isn't charged for the TCP sockets
  (`TCP` slab, ~2.3 KB a connection), their inodes (0.8 KB), the socket
  buffers (`sock` 0 MB), or kTLS's state. combd counted 460 MB at 30,000
  under a 550 MB budget while the box had nothing left: no pressure, so no
  shedding, so swap.

| 20,000 active, kTLS | combd's cgroup (anon) | Kernel slab (unreclaimable, whole box) | Box memory free |
|---|---|---|---|
| Off | 481 MB (302) | 109 MB | 271 MB |
| On | 474 MB (256) | 194 MB | 255 MB |

**A correction to the kTLS result above.** kTLS moves a connection's TLS
state into the kernel (`kmalloc-1k` 40 MB and `kmalloc-512` 10 MB at
20,000, ~2.5 KB a connection; neither cache exists with kTLS off), and the
idle hold counted only combd's cgroup, which isn't charged for it. combd's
side does shrink (~2.3 KB a connection here); the box's total doesn't. So
kTLS doesn't halve a connection's memory; it moves it. Active, it's no
better: 255 MB free with it, 271 MB without.

Both kTLS and io_uring are out of combd since (2026-10-22): no gain for
their Linux-only unsafe code.

## A security review, and what it changed (2026-10-22)

A review by Fable 5.1 (high effort) of every attack surface: the public
front, the admin API, the wire between nodes, the sandbox and host calls,
builds, bundles, secrets, storage, the update path, and CI. It found no way
in from the internet; the front's smuggling defenses, the admin tokens
(hashed, constant time, scoped), the wire's mutual TLS, signed builds, the
SQLite limits, and CI (no `pull_request` trigger on the self-hosted runner)
held. What it found, and what changed:

| Finding | Severity | Now |
|---|---|---|
| A DigitalOcean token in git history (a Terraform plan, committed by mistake) | critical | rotated (the old one is refused); plans and state are gitignored |
| Node updates trusted a sha256 from the same release | medium | the release note is signed (ed25519); nodes check it against a pinned key |
| Builds named by 64-bit FNV, not a cryptographic hash | low | SHA-256 (128 bits) |
| Postgres connections couldn't use TLS | low | `sslmode=require`: TLS with the certificate checked, or no connection |

Also: check.sh runs gitleaks on what's about to be pushed and `cargo
audit`; Rust 1.99.1 (wasmtime 50, its advisories fixed); and a node moves
to a new combd only once that combd's builds are in, so a pin change never
takes an app down. gitleaks can't see into binary files (it missed the
plan), so `.gitignore` is the guard there.

## Overload, without Docker (2026-10-22)

The same one-node ramp (million compiled, every page clicking, a fresh
server each run, 1 vCPU, 1 GB, 1 GB swap), combd now as the nodes run it:
under systemd, no Docker (~700 MB free before it starts), the whole
machine's memory watched.

| Users | In Docker (2026-10-21) | Without Docker | And the overload guard |
|---|---|---|---|
| 10,000 | 0% lost, 70% CPU | 0% lost, p50 419 ms, 62% CPU | |
| 20,000 | 0.45% lost | 0.04% lost, p50 1.6 s | |
| 30,000 | 83% lost (swapping) | 13 to 20% lost | 1.5% lost, p50 1.5 s |
| 40,000 | 87% lost | 94 to 98% lost, then killed for memory | ~30,000 held, the rest refused; 0.5% of their clicks lost, p50 913 ms |

**Without the guard, 40,000 ran a node out of memory with none of them
swapping in first.** Every message to a sleeping viewer woke it at once (a
cell back in memory, ~67 KB), and once the loop fell behind, more and more
were awake at the same time: swap went from 90 MB to 1 GB in three minutes
and the kernel killed combd. The memory check couldn't help: it runs on
the same loop, and pressure never reached "refuse".

**The guard** (`COMBD_MAX_AWAKE`, 1,000 a loop): no more cells at work
than that, running or waiting on a host call. A sleeping session with work
waits its turn, asleep; while that line is as long as the cap, the node
takes no new connections, and each session's mailbox is 8, not 1,000.
`sessions_ready` and `sessions_waited` show it working. A refused TLS
client is just closed (a plain 503 was noise to it), and this machine
itself is never refused, so its own checks see a full node. Since
2026-10-24 a full node also stops listening (at least a second each way),
so new connections are refused at TCP and a browser tries the name's next
address by itself; a listener on 127.0.0.1 stays (`a_full_node_refuses_at_tcp`;
not yet measured on the fleet).

**Idle, without Docker:** 57,854 idle million viewers held, then new ones
refused (memory pressure), with the node answering in 4 to 20 ms and swap
flat. In Docker it held 105,000, 1 GB deep in swap; now it refuses before
it swaps.

**Found on the way:** `ws-click` panicked on the first connection that
failed (a busy node's full accept queue) and took its pages with it; now
it waits a second and tries again, as a browser would.

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

0. ~~**A takeover can wait on an old owner that keeps renewing its lease.**~~
   Fixed: a peer that has told a newer owner's fence to wait stops promising
   the older epoch, and answers its appends with `Kept` (it has them, no
   promise), which the owner doesn't count toward its lease. The wait then
   ends within a lease. Simulator, objects beating with peers far away:
   seed 113's takeover went from 15.3 s to 1.9 s, and 280,000 seeds across
   every fault mix hold every invariant, liveness included; the owner
   counting `Kept` as a promise (`lease_trusts_kept`) is caught. The spec
   needs no change: peers renewing less is a subset of what it checks, and
   it already models an owner's lease as needing every peer's promise.
0b. ~~**Compact an object's segments.**~~ Done. An owner that restored
   from 16 segments or more (or listed 64 keys or more) writes an image
   (`obj/o/e<E>/i`, the object as one entry); the next owner starts from
   it. Once the image is written and a round proves it still owns the
   object, the owner deletes every older epoch's key. On the arena: the
   matchmaker's manifest went from 201 keys to 2, its keys in the bucket
   from over 300 to 3, and the bucket from 1,141 object keys to 612 (node 0
   deleted 537). Simulator: 100,000 seeds hold every invariant with 3.4
   million deletes; deleting with no proof (`gc_unproven`), deleting our own
   uploads, and deleting with no image are each caught. It found a bucket
   client bug on the way: every DELETE timed out (a 204 read "to the end"
   on a kept connection). Left: image a long-lived object mid-epoch, not
   only at a restore.
1. ~~Run the official Wasm spec test suite against the interpreter.~~ Done:
   30,497 checks pass, plus differential fuzzing against Wasmtime.
2. ~~Register-based interpreter, toward the 5x target.~~ Done (rounds 2 and
   3): 4x to 10x behind Wasmtime now. Next there: f64 constants as operands
   (about a quarter of JS fib's ops are still constant loads), and bigger fused ops.
3. One reserved memory region for all cells (audited FFI crate), to get past
   the per-process map limit.
4. A real Linux x86 server with pinned threads.
5. Add celld to the scoreboard (needs a yes to install and run it).

## The click race: combd, workerd, celld (2026-10-25)

Million on all three, on the same box, one at a time (`bench/click/run.sh`,
one command; summaries in `bench/click/results/`).
- **The apps:** combd runs `examples/million` with its chunks and viewers
  compiled (the nodes' build, `aot-lite`; the builds made by `aot-build`,
  the builder's command) and its page interpreted, as its manifest says.
  workerd 1.20260929.1 and celld v0.6.2 run `examples/million-workers`: the
  same app as a Workers app with Durable Objects (same page, protocol,
  chunk size, 20 sets a second, 150 ms beat, 60 s subscriptions), written
  for this race. Two ways to hold a page's socket on Workers:
  - **a Viewer object per socket** (hibernatable, as million's viewer
    session): a chunk can't write to a socket another object holds, so it
    calls each watcher's Viewer over RPC, every beat;
  - **the Worker holds it** (workerd only), with a hibernatable socket per
    watched chunk, so a beat is one `send` per watcher. celld v0.6.2 drops
    the Worker's end of such a socket right after the upgrade (the chunk's
    `getWebSockets()` is empty at its next beat), so it runs the first way.
- **The box:** each server in Docker with `--cpus=1 --memory=1g
  --memory-swap=1g` (a $6 droplet's size, no swap). celld as `celld dev`
  (one node, local storage), as in the Durable Object race. Plain
  WebSockets for all three: celld can't end TLS, so `ws-click` learned
  plain `ws://` (`ca` as `-`). On the droplets combd ends TLS itself
  (~4 KB more an idle connection).
- **This is not a droplet.** An Apple M4 Max (macOS 27), Docker Desktop
  29.9.2: a Linux arm64 VM with 8 CPUs and 16 GB for Docker, other
  containers idling beside it (databases, emulators), the load clients on
  the same VM. Droplets are x86 with a real NIC; their numbers will differ.
- **The load:** `ws-click` in containers of 2,000 pages, a new container
  every 2 s (~1,000 new pages a second at most). Idle pages never click (a
  first click due in about a week) but renew every 20 s as million's page
  does; clicking pages click 1 to 30 boxes 1 to 20 s apart, then watch.
  Memory is the server's cgroup less inactive page cache (as docker stats
  counts it), every second (idle) or 5 s (clicking); held sockets are
  counted in the server's own network namespace.
- **Two full runs** (a, b) of everything, a fresh server for each test.
  Where they differ, both are shown.

**Idle pages, until the server refuses or falls over:**

| Server | Held | Its memory then | A page | How it ended |
|---|---|---|---|---|
| **combd** | **156,000 / 156,565** | 932 / 931 MiB | **6.1 KB** | refused new ones at TCP (its memory guard) and kept serving the rest |
| workerd, a Viewer object per socket | 3,151 / 2,870 | 984 / 912 MiB | 270 to 300 KB | killed for memory |
| workerd, the Worker holds the socket | 6,000 / 6,000 | 841 / 869 MiB | 133 KB | killed for memory (in the next 2,000) |
| celld, a Viewer object per socket | 2,803 / 3,061 | 698 / 721 MiB | 216 to 222 KB | answered 500 to the rest ("shedding on the absolute memory cap") and stayed up, at 75% CPU (a new page got in on run a, not b) |

So in a GB: combd about 156,000 pages, workerd 3,000 to 6,000, celld about
3,000. With combd at 156,000, its CPU was 40 to 60%, most of it the
renewals (7,800 a second).

**Clicking pages** (changes delivered a second: every click times its
watchers, averaged over the 180 s and in the best 10 s; echo times are the
worst 10 s window's worst client; lost: no echo in 10 s):

| 1,000 pages | Changes/s (avg, best 10 s) | Echo p50 / p99 | Lost | Memory | CPU |
|---|---|---|---|---|---|
| **combd** | 1,200, 1,770 | **152 / 161 ms** | 0 | **63 MiB** | **4%** |
| workerd, Viewer objects | 1,180, 1,730 | 155 / 217 to 258 ms | 0 / 0.12% | 594 to 613 MiB | 10% |
| workerd, Worker-held | 1,190, 1,720 | 154 / 239 to 281 ms | 0 | 292 MiB | 7% |
| celld, Viewer objects | 1,200, 1,540 | 8.0 to 9.3 s / 9.7 to 10.0 s | 7.3% / 0.07% | 790 MiB | 95 to 99% |

| 5,000 pages | Changes/s | Echo p50 / p99 | Lost | Memory | CPU |
|---|---|---|---|---|---|
| **combd** | 28,700, 40,000 | **112 to 153 / 362 to 572 ms** | 0 | **140 to 146 MiB** | **24 to 25%** |
| workerd, Viewer objects | | | | killed for memory as pages arrived | |
| workerd, Worker-held | 28,700, 40,000 | 0.6 to 5.4 s / 4.0 to 8.3 s | 0 | 919 to 944 MiB | 49 to 57% |
| celld, Viewer objects | | | 27% / 61% | killed for memory mid-run | |

| 10,000 pages | Changes/s | Echo p50 / p99 | Lost | Memory | CPU |
|---|---|---|---|---|---|
| **combd** | 113,000, 151,000 to 161,000 | 115 ms to 1.2 s / 476 ms to 5.9 s | **0** | **206 to 221 MiB** | 55 to 57% |
| workerd, both ways | | | | killed for memory as pages arrived | |
| celld | 63 to 231 | 9.3 to 9.7 s / 10 s | 51% to 56% | up, at 99 to 100% CPU, most sockets closed | |

| 20,000 pages | Changes/s | Echo p50 / p99 | Lost | Memory | CPU |
|---|---|---|---|---|---|
| **combd** | **445,000, 580,000 to 587,000** | **299 to 302 / 810 to 865 ms** | **0** | **385 to 440 MiB** | 81 to 82% |
| workerd, both ways | | | | killed for memory as pages arrived | |
| celld | | | | no client got its 2,000 pages open in 10 minutes | |

What the numbers mean:
- **Memory is the difference.** An idle million page is 6 KB on combd and
  130 to 300 KB on workerd and celld: 20 to 50 times as many pages in the
  same GB. Clicking, combd at 20,000 pages used less memory than workerd
  at 1,000.
- **Where workerd held on, it kept up.** At 1,000 and (Worker-held) 5,000
  pages it delivered the same changes as combd and lost nothing; at 5,000
  its echoes were slower (p50 0.6 to 5.4 s against 112 to 153 ms) and it
  was at 920 to 940 MiB of 1,024. Then memory ended it.
- **celld fell behind at 1,000 pages,** CPU-bound (95 to 99%) with
  echoes 8 to 10 s late. Its Viewer objects pay an RPC per watcher per
  beat, the only way to reach a page's socket that works on celld.
- **combd's 10,000 run a** had one bad stretch (40 s with p50 1.2 s and
  p99 5.9 s, its CPU dropping to 11% in one sample: waiting, not working);
  run b, and 20,000 in both runs, didn't. Not investigated; we'd guess the
  VM.
- **combd at 20,000 clicking pages** delivered 445,000 changes a second on
  one core with nothing lost: about 4x the 10,000 pages' rate, as fan-out
  grows with pages times watchers.

**A burst that outran combd's guard.** Before the arrivals were paced, four
clients opened idle pages as fast as they could (~14,000 a second). In two
of four tries combd was killed for memory (at ~100,000 and ~115,000
pages: the cgroup's peak touched its 1 GiB within a second); in the other
two it held 162,800 and 163,270 and refused the rest, at 98% of its
memory. Paced at 1,000 a second it refused cleanly both times. The guard
checks memory on the loop, and a second of new connections at that rate is
~85 MB of sockets and snapshots: it needs a margin for the rate, or a cap
on new connections a second.

**Cloudflare, an estimate from public pricing** (not deployed, not
measured; developers.cloudflare.com/durable-objects/platform/pricing and
/workers/platform/pricing, checked 2026-10-24): Durable Object requests
$0.15 a million (an RPC call is one; incoming WebSocket messages count 1
in 20; outgoing are free), duration $12.50 a million GB-s at 128 MB an
object (hibernatable idle objects aren't billed), SQLite rows written $1 a
million; Workers $0.30 a million requests and $0.02 a million CPU-ms; $5 a
month minimum. Using the click rates measured above (67 to 1,360 clicks a
second), 64 chunks always busy, and the fan-out million does (a message
per watcher per beat with a change):

| Load | Viewer objects (RPC per watcher) | Worker-held sockets | combd |
|---|---|---|---|
| 1,000 pages | ~$1.20 an hour | ~$0.60 an hour | one $6/month droplet |
| 5,000 pages | ~$12.50 an hour | ~$1.60 an hour | the same droplet (25% CPU) |
| 10,000 pages | ~$34 an hour | ~$2.80 an hour | the same |
| 20,000 pages | ~$80 an hour | ~$5.30 an hour | the same (81% CPU) |

The RPC pushes are most of the first column (136,000 a second at 20,000
pages: ~$73 an hour); the second is mostly SQLite writes ($4.90) and the
chunks' duration ($0.36). Left out: Viewer objects' own duration while
they run a push, and the Worker's CPU for passing messages on (at 0.05 ms
a message, ~$0.50 an hour at 20,000 pages). On Cloudflare neither design
would fall over: objects spread over many machines. What it would cost to
run million's real traffic (bursty, mostly idle) is lower; these are the
rates while that many people click.

What this doesn't cover:
- **Not a droplet.** One Mac, arm64 in a VM, the clients on the same VM.
  Two runs each; earlier work here saw a busy laptop swing results 2x to
  5x. The droplet ramp (2026-10-21 and 22, TLS, real network) is the one
  to quote for a node's capacity; this one is for the comparison.
- **The Workers app is ours.** A better port may exist. The Viewer-object
  way is the faithful one (a session per socket, hibernating), and the
  Worker-held way trades hibernation for fewer calls; both are measured.
- **Hibernation on workerd didn't free memory here.** 2,000 sockets to
  Viewer objects, each sent one message and then left alone: 640 MB
  before and after 3 minutes. workerd as shipped, no settings changed; we
  didn't find out why. On Cloudflare a hibernated object leaves memory, so
  workerd's idle numbers say nothing about what Cloudflare holds.
- **celld ran as `celld dev`:** one node, its local storage. Its
  production mode (a bucket and peers) wasn't measured.
- **No TLS, no latency to Cloudflare.** Echo times here are all on one
  machine; the 150 ms beat is the floor for all three.
