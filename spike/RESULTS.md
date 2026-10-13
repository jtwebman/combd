# Phase 0 results: the engine race

Date: 2026-10-13. Machine: Apple M4 Max (14 cores: 10 performance, 4
efficiency), 36 GB RAM, macOS 26.7.1. Rust 1.94.1.
Wasmtime 47.0.4 (49 needs Rust 1.96).

Same Rust cell (`guest/`), compiled once to `wasm32-unknown-unknown`, run on:
- **interp**: our own interpreter, about 1,000 lines of safe Rust
  (`#![forbid(unsafe_code)]`). `wasmparser` validates, we do the rest.
- **wasmtime**: Wasmtime with fuel yields, through the same engine interface.
- **native**: the same Rust code compiled normally, as a baseline.

Run it: `cd guest && cargo build --release --target wasm32-unknown-unknown`,
then `cd spike && cargo run --release -- all`. Two full runs gave the same
picture. The numbers below are from run 1, with run 2 in brackets when it
differed in a way worth seeing.

## Scoreboard

| Measure | interp (ours) | wasmtime | Winner |
|---|---|---|---|
| Load a module (validate + prepare) | **0.25 ms** | 22 ms | ours, ~85x |
| Spawn a cell | **0.6 us** | 16 us [21] | ours, ~25x |
| Spawn + first request | **5.5 us** | 20 us [24] | ours, ~4x |
| Crash, then fresh cell serves a request | **5.4 us** | 17 us [19] | ours, ~3x |
| Memory, idle cell | **20.5 KB** | 38.2 KB | ours, ~46% less |
| Memory, cell paused mid-loop | **68.9 KB** | 86.8 KB | ours, ~21% less |
| 10,000 paused cells, whole process | **675 MB** | 859 MB | ours |
| Hello latency, quiet node, p50 / p99 | **11 / 75 us** [12 / 161] | 23 / 806 us [36 / 1440] | ours |
| Hello latency, stuck neighbor, p50 / p99 | **36 / 296 us** [37 / 152] | 55 / 646 us [58 / 2418] | ours |
| Snapshot a cell mid-call, resume the copy | **works** | not possible | ours |
| Binary, stripped (gzipped) | **1.31 MB (0.56 MB)** | 7.55 MB (3.04 MB) | ours, ~5.5x smaller |
| fib 30 | 37 ms | **2.6 ms** | wasmtime, 15x |
| sieve 5,000,000 | 265 ms | **14 ms** | wasmtime, 19x |
| sort 1,000,000 | 566 ms | **15 ms** | wasmtime, 39x |

Native Rust for the same three: 1.7 ms, 9.6 ms, 12.6 ms. Our interpreter is
22x to 45x slower than native. Wasmtime is 1.2x to 1.5x slower than native.

celld itself is not in this table yet. Running it means installing and
running their binary, which needs a yes first.

## Fairness, the Erlang test

10,000 cells stay resident across 14 threads. Each thread serves one hello at a
time to its cells. In the "stuck neighbor" runs, each thread also hosts one cell
in `loop {}`. Every new hello waits behind one full neighbor slice, the worst
case. Slices are tuned to about 50 us for both engines.

| Neighbor | interp p50 | interp p99 | wasmtime p50 | wasmtime p99 |
|---|---|---|---|---|
| none | 11 us | 75 us | 23 us | 806 us |
| `loop {}`, preemption on | 36 us | 296 us | 55 us | 646 us |
| long sort, **preemption off** | **146,190 us** | **224,376 us** | n/a | n/a |

A stuck cell costs its neighbors about one slice (~40 us). Turn preemption off
and the same neighbor costs ~146 ms, about 4,000x worse. That is the whole case
for BEAM-style scheduling, measured. (Wasmtime's yield interval is fixed per
store, so only our engine can switch it off per call for this row.)

Max latencies (not shown) were 4 to 70 ms in some runs, on both engines.
14 busy threads on a 10+4 core chip with a desktop OS running means the OS
sometimes parks a thread. A Linux server with pinned threads should tell a
cleaner story.

## Linux (Docker)

Same code in a Docker container on the same Mac: Linux 7.0 (LinuxKit), ARM64,
4 KB pages, 8 CPUs given to the VM. Build and run with
`docker build -f spike/Dockerfile -t combd-spike .` then
`docker run --rm combd-spike`. All tests pass inside the container. Two full
runs, ranges shown.

| Measure | interp (ours) | wasmtime | Winner |
|---|---|---|---|
| Load a module | **0.35 ms** | 25 ms | ours, ~70x |
| Spawn a cell | 4.9 to 8.8 us | **4.8 to 5.0 us** | about a tie |
| Spawn + first request | **12 to 15 us** | 24 to 27 us | ours, ~2x |
| Crash, then fresh cell serves | **8.8 to 9.6 us** | 15 us | ours, ~1.6x |
| Memory, idle cell | 8.5 KB | **6.0 KB** | wasmtime |
| Memory, cell paused mid-loop | **20.8 KB** | 86.6 KB | ours, ~4x less |
| 10,000 paused cells, whole process | **207 MB** | 854 MB | ours, ~4x less |
| Hello, quiet, p50 / p99 | **8 / 114 to 203 us** | 97 to 239 / 2,100 to 4,600 us | ours |
| Hello, stuck neighbor, p50 / p99 | **37 / 79 to 252 us** | 211 / 1,900 to 2,400 us | ours |
| Hello, heavy neighbor, preemption off, p50 | 102,000 to 106,000 us | n/a | |
| fib 30 / sieve / sort | 40 / 295 / 568 ms | **2.7 / 16.5 / 16.5 ms** | wasmtime, 15x to 35x |

What changed on Linux:
- **Paused cells shrank from 69 KB to 21 KB on our engine,** thanks to 4 KB
  pages. Wasmtime stayed near 87 KB, likely because each paused call holds its
  own fiber stack. 10,000 paused cells: 207 MB vs 854 MB.
- **Our spawn got slower (0.6 us to ~5 us).** glibc serves each 1 MiB cell
  buffer with its own `mmap`, and that system call is the cost.
- **glibc needed one setting.** After big frees, glibc raises its mmap
  threshold and starts serving 1 MiB blocks from its heap, which it then
  zeroes by hand. Crash restart was 33 us until we pinned
  `MALLOC_MMAP_THRESHOLD_=131072` (set in the Dockerfile). Then it dropped to
  9 us. Zeroing still happens either way; the setting only moves it from
  glibc's `memset` to the kernel's lazy zero pages.
- **Wasmtime's latency is much worse here than on macOS** (p50 ~100 to 240 us
  vs ~25 us). We run it with default settings and no pooling allocator, so
  every call sets up a fresh async stack. A tuned Wasmtime would do better.
  Treat its latency rows as "Wasmtime untuned".

## Security: memory is always zero

Every byte a new cell can see starts as zero. Its buffer comes from `calloc`,
whether the kernel zeroes the pages lazily or glibc zeroes them with `memset`.
Safe Rust can't expose leftover memory at all (that takes `unsafe`, which is
banned). The test `fresh_cells_never_see_old_memory` fills dead cells with a
pattern and checks that new cells never see it.

## What we learned

1. **The Erlang shape works.** Preemption holds, crashes stay contained, a
   fresh cell is serving again in ~5 us, and mid-call snapshots work. Every
   test passes on both engines with identical answers.
2. **Our engine wins everything except raw compute,** and by a lot on startup,
   spawn, crash recovery, binary size, and latency.
3. **Raw compute is the real gap: 15x to 39x behind Wasmtime.** My earlier
   guess was 3x to 10x. This is a naive first interpreter: a stack machine with
   one big `match`, `Vec` push and pop on every op, and double bounds checks on
   memory. The known fixes (register bytecode, fused ops, fewer checks) are the
   first rung of the speed ladder in the plan. Mature interpreters like wasm3
   and wasmi show what is possible.
4. **Per-cell memory is mostly the guest's own pages, not the engine.** Both
   engines land at 70 to 90 KB per paused cell. Apple Silicon uses 16 KB pages,
   so a few touched pages add up fast. Linux on x86 uses 4 KB pages and should
   be smaller. A slimmer guest (stack size, allocator) is the next lever.
5. **Safe Rust can't reserve memory the way Wasmtime does.** Wasmtime reserves
   address space with `mmap` and lets pages appear lazily. We tried reserving
   1 GiB per cell with `calloc`. Spawn went from 0.6 us to 5.5 ms, because
   macOS charges ~9 ms per GB to free and reuse big blocks. What we do now:
   start at a 1 MiB buffer (macOS hands out lazy zero pages from 1 MiB up),
   double on growth, and copy only chunks that hold data. Real `mmap`
   reservation belongs in the small audited FFI crate the plan already allows.
6. **A bug in our test cell, caught by the native baseline.** `i * i` on a
   32-bit `usize` overflowed in the sieve. Both Wasm engines agreed with each
   other, and correctly. Only comparing against native caught it. Keep the
   native baseline in every future test.

## Verdict

**Keep our own interpreter as the default engine.** The things combd cares
about most (many small cells, fast start, cheap crashes, fairness, small
binary, snapshots) all came out in its favor. Close the compute gap in phase 1
with a target: within 5x of Wasmtime on these three workloads. Keep Wasmtime
behind the engine interface. It costs nothing to keep, and it gives cells that
are heavy on compute a fast option.

## Next

1. Run the official Wasm spec test suite against the interpreter.
2. Fast-interpreter work toward the 5x target. Profile `sort` first; it is the
   worst at 39x.
3. Same race on a real Linux x86 server (done on ARM64 in Docker, see above).
   Also rerun Wasmtime with its pooling allocator for a fairer latency row.
4. Add celld to the scoreboard (needs a yes to install and run it).
