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
