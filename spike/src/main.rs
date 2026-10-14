//! Phase 0: the engine race. Same Rust cell, our interpreter vs Wasmtime.
//!
//!   cargo run --release -- all              every test, each in a fresh process
//!   cargo run --release -- langs            every language's cell, on both engines
//!   cargo run --release -- js               the JS spike: QuickJS cell on both engines vs Node
//!   cargo run --release -- <test> <engine>  tests: compute startup crash density fairness
//!                                           engines: interp wasmtime wasmtime-pool (compute also: native)

#![forbid(unsafe_code)]

#[cfg(feature = "aot")]
mod aot;
mod aot_gen;
mod engine;
mod host;
mod interp;
#[cfg(feature = "wasmtime")]
mod wt;

use engine::{CellVm, Engine, Outcome};
use host::HostState;
use std::time::{Duration, Instant};

const GUEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm");
/// Reductions (loop iterations and calls) per slice for our interpreter.
const SLICE: u32 = 2_500;
/// Fuel (about one per wasm op) per slice for Wasmtime. Both are tuned so a
/// spinning cell's slice is about 50 us on an M4 Max.
const WT_FUEL: u64 = 1_000_000;
/// How many cells the density and fairness tests use. `COMBD_CELLS` overrides it.
fn cells() -> usize {
    std::env::var("COMBD_CELLS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000)
}

const LANGS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../langs/out");

/// The Rust cell, plus every cell built by `langs/Dockerfile`.
fn lang_cells() -> Vec<(String, Vec<u8>)> {
    let mut out = vec![("rust".to_string(), wasm())];
    let mut files: Vec<_> = std::fs::read_dir(LANGS).into_iter().flatten().flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files.into_iter().filter(|f| f.extension().is_some_and(|x| x == "wasm")) {
        out.push((f.file_stem().unwrap().to_string_lossy().into_owned(), std::fs::read(&f).unwrap()));
    }
    out
}

/// Reactor modules (C, Go, Zig, AssemblyScript) export `_initialize`, which
/// must run once before anything else.
fn init<V: CellVm>(vm: &mut V, st: &mut HostState) -> Result<(), String> {
    if vm.begin("_initialize", &[]).is_ok() {
        finish(vm, u32::MAX, st)?;
    }
    Ok(())
}

/// Run the same checks on every language's cell.
fn langs<E: Engine>(name: &str, eng: &E) -> bool {
    let mut all_ok = true;
    for (lang, wasm) in lang_cells() {
        let t = Instant::now();
        let prog = match eng.load(&wasm) {
            Ok(p) => p,
            Err(err) if err.starts_with("no AOT build") => continue,
            Err(err) => {
                println!("langs    {name:<9} {lang:<5} LOAD FAILED: {err}");
                all_ok = false;
                continue;
            }
        };
        let load = t.elapsed();
        let fresh = || -> Result<(E::Vm, HostState), String> {
            let mut vm = eng.spawn(&prog)?;
            let mut st = HostState::default();
            init(&mut vm, &mut st)?;
            Ok((vm, st))
        };
        let mut fails = vec![];
        let mut check = |what: &str, ok: Result<bool, String>| match ok {
            Ok(true) => {}
            Ok(false) => fails.push(what.to_string()),
            Err(e) => fails.push(format!("{what} ({e})")),
        };
        check("hello", fresh().and_then(|(mut vm, mut st)| Ok(request(&mut vm, "hello", &mut st)? == "hello #1")));
        check("fib", fresh().and_then(|(mut vm, mut st)| Ok(request(&mut vm, "fib 25", &mut st)? == "75025")));
        check(
            "print",
            fresh().and_then(|(mut vm, mut st)| {
                let answer = request(&mut vm, "print", &mut st)?;
                let out = [st.out, vm.host_out()].concat();
                // AssemblyScript and Rust on wasm32-unknown-unknown have no stdout.
                let has_stdout = lang != "as" && lang != "rust";
                Ok(answer == "printed" && (!has_stdout || String::from_utf8_lossy(&out).contains("hi from")))
            }),
        );
        check(
            "spin",
            fresh().and_then(|(mut vm, mut st)| {
                start(&mut vm, "spin", &mut st)?;
                if name == "aot" {
                    // Compiled cells can't pause yet: a spin must be killed at the CPU limit.
                    return Ok(matches!(vm.run(SLICE), Outcome::Trap(t) if t.contains("OutOfFuel")));
                }
                Ok((0..50).all(|_| matches!(vm.run(SLICE), Outcome::Yield)))
            }),
        );
        check("panic", fresh().map(|(mut vm, mut st)| request(&mut vm, "panic", &mut st).is_err()));
        let fib = fresh().and_then(|(mut vm, mut st)| {
            let t = Instant::now();
            request(&mut vm, "fib 30", &mut st)?;
            Ok(t.elapsed())
        });
        let fib = fib.map_or("failed".into(), |d| format!("{:.1} ms", d.as_secs_f64() * 1e3));
        let verdict = if fails.is_empty() { "all 5 checks pass".to_string() } else { format!("FAILED: {}", fails.join(", ")) };
        all_ok &= fails.is_empty();
        println!(
            "langs    {name:<9} {lang:<5} {:>7.1} KB  load {:>6.2} ms  fib 30 {fib:>9}  {verdict}",
            wasm.len() as f64 / 1024.0,
            load.as_secs_f64() * 1e3
        );
    }
    all_ok
}

const JS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../langs/js");

fn js_cell() -> (Vec<u8>, Vec<String>) {
    let wasm = std::fs::read(format!("{LANGS}/js.wasm")).expect("build langs/out first (see langs/README.md)");
    let reqs = std::fs::read_to_string(format!("{JS_DIR}/requests.txt")).unwrap();
    (wasm, reqs.lines().map(String::from).collect())
}

/// Spawn a JS cell and run its init (QuickJS boot + app.js).
fn js_fresh<E: Engine>(eng: &E, prog: &E::Program) -> E::Vm {
    let mut vm = eng.spawn(prog).unwrap();
    init(&mut vm, &mut HostState::default()).unwrap();
    vm
}

fn js_bench<E: Engine>(name: &str, eng: &E) {
    let (wasm, reqs) = js_cell();
    let t = Instant::now();
    let prog = eng.load(&wasm).unwrap();
    println!("js       {name:<9} load {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    let n = 20;
    let t = Instant::now();
    for _ in 0..n {
        let mut vm = js_fresh(eng, &prog);
        request(&mut vm, &reqs[0], &mut HostState::default()).unwrap();
    }
    println!("js       {name:<9} cold, no snapshot (spawn + QuickJS boot + app + first request) {:.1} us", us(t.elapsed() / n));
    let base = js_fresh(eng, &prog);
    if base.snapshot().is_some() {
        let n = 200;
        let t = Instant::now();
        for _ in 0..n {
            let mut vm = base.snapshot().unwrap();
            request(&mut vm, &reqs[0], &mut HostState::default()).unwrap();
        }
        println!("js       {name:<9} cold, from snapshot after init (copy + first request) {:.1} us", us(t.elapsed() / n));
    }
    let mut vm = js_fresh(eng, &prog);
    let mut st = HostState::default();
    for r in &reqs {
        let label: String = r.chars().take(22).collect();
        let iters = if r.starts_with("fib") { 3 } else { 200 };
        for _ in 0..20.min(iters) {
            request(&mut vm, r, &mut st).unwrap();
        }
        let t = Instant::now();
        for _ in 0..iters {
            request(&mut vm, r, &mut st).unwrap();
        }
        let per = t.elapsed() / iters;
        let out = request(&mut vm, r, &mut st).unwrap();
        let head: String = out.chars().take(40).collect();
        println!("js       {name:<9} {label:<22} {:>9.1} us   -> {} bytes: {head}", us(per), out.len());
    }
}

fn js_mem<E: Engine>(name: &str, eng: &E) {
    let (wasm, _) = js_cell();
    let prog = eng.load(&wasm).unwrap();
    let n = 1000;
    let before = rss_kb();
    let cells: Vec<E::Vm> = (0..n).map(|_| js_fresh(eng, &prog)).collect();
    let after = rss_kb();
    println!("js       {name:<9} memory per JS cell after init: {:.1} KB", (after - before) as f64 / n as f64);
    if let Some(base) = cells[0].snapshot() {
        let before = rss_kb();
        let copies: Vec<E::Vm> = (0..n).map(|_| base.snapshot().unwrap()).collect();
        let after = rss_kb();
        println!("js       {name:<9} memory per JS cell copied from snapshot: {:.1} KB [{}]", (after - before) as f64 / n as f64, copies.len());
    }
}

fn wasm() -> Vec<u8> {
    std::fs::read(GUEST).expect("build the guest first: cd guest && cargo build --release --target wasm32-unknown-unknown")
}

/// Guest memory of a cell that is waiting on a host call.
struct VmMem<'a, V>(&'a mut V);

impl<V: CellVm> host::Mem for VmMem<'_, V> {
    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        self.0.read(ptr, len)
    }
    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        self.0.write(ptr, bytes)
    }
}

/// Answer a `HostCall` and hand the result back to the cell.
fn serve<V: CellVm>(vm: &mut V, import: u32, args: &[u64], st: &mut HostState) -> Result<(), String> {
    let imp = vm.import(import).ok_or("unknown import")?.clone();
    let r = host::call(&imp, args, &mut VmMem(vm), st)?;
    vm.resume(&r);
    Ok(())
}

/// Run the current call to the end, slice by slice.
fn finish<V: CellVm>(vm: &mut V, slice: u32, st: &mut HostState) -> Result<Vec<u64>, String> {
    loop {
        match vm.run(slice) {
            Outcome::Done(r) => return Ok(r),
            Outcome::Yield => {}
            Outcome::HostCall { import, args } => serve(vm, import, &args, st)?,
            Outcome::Trap(t) => return Err(t),
        }
    }
}

/// Copy a request into the cell and begin `handle`. Nothing runs yet.
fn start<V: CellVm>(vm: &mut V, req: &str, st: &mut HostState) -> Result<(), String> {
    vm.begin("alloc", &[req.len() as u64])?;
    let ptr = finish(vm, u32::MAX, st)?[0] as u32;
    if !vm.write(ptr, req.as_bytes()) {
        return Err("bad alloc pointer".into());
    }
    vm.begin("handle", &[ptr as u64, req.len() as u64])
}

fn response<V: CellVm>(vm: &V, r: &[u64]) -> String {
    let r = r[0];
    String::from_utf8_lossy(&vm.read((r >> 32) as u32, r as u32).unwrap_or_default()).into_owned()
}

fn request<V: CellVm>(vm: &mut V, req: &str, st: &mut HostState) -> Result<String, String> {
    start(vm, req, st)?;
    let r = finish(vm, SLICE, st)?;
    Ok(response(vm, &r))
}

fn rss_kb() -> u64 {
    // Linux: read it directly. macOS: ask ps.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap_or("");
        return line.split_whitespace().nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

const WORK: [&str; 3] = ["fib 30", "sieve 5000000", "sort 1000000"];

fn compute<E: Engine>(name: &str, eng: &E, prog: &E::Program) {
    for req in WORK {
        let mut vm = eng.spawn(prog).unwrap();
        let t = Instant::now();
        let out = request(&mut vm, req, &mut HostState::default()).unwrap();
        println!("compute  {name:<9} {req:<14} {:>9.1} ms  -> {out}", t.elapsed().as_secs_f64() * 1e3);
    }
}

fn compute_native() {
    for req in WORK {
        let t = Instant::now();
        let out = combd_guest::run(req, || 1);
        println!("compute  {:<9} {req:<14} {:>9.1} ms  -> {out}", "native", t.elapsed().as_secs_f64() * 1e3);
    }
}

fn startup<E: Engine>(name: &str, eng: &E, wasm: &[u8]) {
    let n = 20;
    let t = Instant::now();
    for _ in 0..n {
        eng.load(wasm).unwrap();
    }
    let load = t.elapsed() / n;
    let prog = eng.load(wasm).unwrap();
    let n = 2000;
    let t = Instant::now();
    for _ in 0..n {
        eng.spawn(&prog).unwrap();
    }
    let spawn = t.elapsed() / n;
    let t = Instant::now();
    for _ in 0..n {
        let mut vm = eng.spawn(&prog).unwrap();
        request(&mut vm, "hello", &mut HostState::default()).unwrap();
    }
    let cold = t.elapsed() / n;
    println!(
        "startup  {name:<9} load {:>8.2} ms   spawn {:>7.1} us   spawn+first hello {:>7.1} us",
        load.as_secs_f64() * 1e3,
        us(spawn),
        us(cold)
    );
}

fn crash<E: Engine>(name: &str, eng: &E, prog: &E::Program) {
    let n = 1000;
    let mut total = Duration::ZERO;
    for _ in 0..n {
        let mut vm = eng.spawn(prog).unwrap();
        request(&mut vm, "panic", &mut HostState::default()).expect_err("panic must trap");
        let t = Instant::now();
        let mut fresh = eng.spawn(prog).unwrap();
        assert_eq!(request(&mut fresh, "hello", &mut HostState::default()).unwrap(), "hello #1");
        total += t.elapsed();
    }
    println!("crash    {name:<9} trap -> fresh cell -> hello served: {:>7.1} us avg over {n}, host still up", us(total / n));
}

fn density<E: Engine>(name: &str, eng: &E, prog: &E::Program) {
    let n = cells();
    let before = rss_kb();
    let mut cells: Vec<E::Vm> = (0..n).map(|_| eng.spawn(prog).unwrap()).collect();
    let idle = rss_kb();
    for vm in &mut cells {
        start(vm, "spin", &mut HostState::default()).unwrap();
        assert!(matches!(vm.run(SLICE), Outcome::Yield));
    }
    let paused = rss_kb();
    println!(
        "density  {name:<9} {n} cells: idle {:>6.1} KB/cell   paused mid-loop {:>6.1} KB/cell   (RSS {} MB)",
        (idle - before) as f64 / n as f64,
        (paused - before) as f64 / n as f64,
        paused / 1024
    );
}

#[derive(Clone, Copy, PartialEq)]
enum Neighbor {
    /// No other work on the thread.
    Quiet,
    /// One cell per thread stuck in `loop {}`, preemption on.
    Spinner,
    /// One cell per thread doing a long sort, preemption off.
    HeavyNoPreempt,
}

/// 10,000 cells stay resident. Each thread serves one hello at a time to the
/// cells it owns, while sharing the thread with a bad neighbor (or none).
/// A new hello always waits behind one full neighbor slice: the worst case.
fn fairness<E>(name: &str, eng: &E, prog: &E::Program, neighbor: Neighbor)
where
    E: Engine + Sync,
    E::Program: Sync,
{
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let per_thread: Vec<(Vec<Duration>, Duration, u64, Duration)> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                sc.spawn(move || {
                    let mut cells: Vec<E::Vm> = (t..cells()).step_by(threads).map(|_| eng.spawn(prog).unwrap()).collect();
                    let (job, budget) = match neighbor {
                        Neighbor::Quiet => ("", 0),
                        Neighbor::Spinner => ("spin", SLICE),
                        Neighbor::HeavyNoPreempt => ("sort 200000", u32::MAX),
                    };
                    let mut bad = (neighbor != Neighbor::Quiet).then(|| {
                        let mut vm = eng.spawn(prog).unwrap();
                        start(&mut vm, job, &mut HostState::default()).unwrap();
                        vm
                    });
                    // Without preemption every hello waits a whole sort, so serve fewer.
                    let hellos = if neighbor == Neighbor::HeavyNoPreempt { 10 } else { cells.len() };
                    let (mut lat, mut bad_time, mut bad_slices, mut bad_max) = (vec![], Duration::ZERO, 0u64, Duration::ZERO);
                    for vm in cells.iter_mut().take(hellos) {
                        let since = Instant::now();
                        let mut st = HostState::default();
                        start(vm, "hello", &mut st).unwrap();
                        if let Some(b) = bad.as_mut() {
                            let ts = Instant::now();
                            match b.run(budget) {
                                Outcome::Yield => {}
                                Outcome::Done(_) => start(b, job, &mut HostState::default()).unwrap(),
                                _ => panic!("bad neighbor misbehaved in an unexpected way"),
                            }
                            let d = ts.elapsed();
                            bad_time += d;
                            bad_slices += 1;
                            bad_max = bad_max.max(d);
                        }
                        finish(vm, SLICE, &mut st).unwrap();
                        lat.push(since.elapsed());
                    }
                    (lat, bad_time, bad_slices, bad_max)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut lat: Vec<Duration> = per_thread.iter().flat_map(|p| p.0.iter().copied()).collect();
    lat.sort();
    let pct = |p: f64| us(lat[((lat.len() - 1) as f64 * p) as usize]);
    let (bt, bs, bm) = per_thread.iter().fold((Duration::ZERO, 0, Duration::ZERO), |a, p| (a.0 + p.1, a.1 + p.2, a.2.max(p.3)));
    let slices = if bs > 0 { format!("   neighbor slice avg {:.0} us, max {:.0} us", us(bt / bs as u32), us(bm)) } else { String::new() };
    let label = match neighbor {
        Neighbor::Quiet => "quiet",
        Neighbor::Spinner => "spinner, preempt on",
        Neighbor::HeavyNoPreempt => "heavy, preempt OFF",
    };
    println!(
        "fairness {name:<9} {label:<20} hello p50 {:>8.1} us  p99 {:>8.1} us  max {:>9.1} us  ({} hellos, {threads} threads){slices}",
        pct(0.5),
        pct(0.99),
        us(*lat.last().unwrap()),
        lat.len()
    );
}

fn run<E>(test: &str, name: &str, eng: &E)
where
    E: Engine + Sync,
    E::Program: Sync,
{
    let wasm = wasm();
    if test == "startup" {
        return startup(name, eng, &wasm);
    }
    let prog = eng.load(&wasm).unwrap();
    match test {
        "compute" => compute(name, eng, &prog),
        "crash" => crash(name, eng, &prog),
        "density" => density(name, eng, &prog),
        "fairness" => {
            fairness(name, eng, &prog, Neighbor::Quiet);
            fairness(name, eng, &prog, Neighbor::Spinner);
            // Wasmtime's yield interval is fixed per store, so only ours can turn it off per call.
            if name == "interp" {
                fairness(name, eng, &prog, Neighbor::HeavyNoPreempt);
            }
        }
        _ => panic!("unknown test {test}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["all"] => {
            for test in ["compute", "startup", "crash", "density", "fairness"] {
                let engines: &[&str] =
                    if test == "compute" { &["native", "interp", "wasmtime"] } else { &["interp", "wasmtime", "wasmtime-pool"] };
                for e in engines {
                    // A fresh process per run keeps the memory numbers clean.
                    let ok = std::process::Command::new(std::env::current_exe().unwrap()).args([test, e]).status().unwrap();
                    assert!(ok.success(), "{test} {e} failed");
                }
            }
        }
        ["compute", "native"] => compute_native(),
        ["aot-gen", input, output] => {
            let wasm = std::fs::read(input).unwrap();
            match aot_gen::generate(&wasm, input) {
                Ok(src) => {
                    std::fs::write(output, &src).unwrap();
                    println!("aot-gen  {input} -> {output} ({} KB of Rust)", src.len() / 1024);
                }
                Err(err) => {
                    eprintln!("aot-gen  {input}: {err}");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(feature = "aot")]
        ["js-bench", "aot"] => js_bench("aot", &aot::Aot),
        #[cfg(feature = "aot")]
        ["js-mem", "aot"] => js_mem("aot", &aot::Aot),
        #[cfg(feature = "aot")]
        ["langs", "aot"] => std::process::exit(if langs("aot", &aot::Aot) { 0 } else { 1 }),
        #[cfg(feature = "aot")]
        [test, "aot"] => run(test, "aot", &aot::Aot),
        ["js"] => {
            let me = std::env::current_exe().unwrap();
            for args in [["js-bench", "interp"], ["js-bench", "wasmtime"], ["js-mem", "interp"], ["js-mem", "wasmtime"]] {
                assert!(std::process::Command::new(&me).args(args).status().unwrap().success());
            }
            let node = std::process::Command::new("node").args(["--expose-gc", &format!("{JS_DIR}/bench.mjs")]).status();
            assert!(node.is_ok_and(|s| s.success()), "node baseline failed");
        }
        ["js-bench", "interp"] => js_bench("interp", &interp::Interp { max_pages: 16384 }),
        ["js-mem", "interp"] => js_mem("interp", &interp::Interp { max_pages: 16384 }),
        #[cfg(feature = "wasmtime")]
        ["js-bench", "wasmtime"] => js_bench("wasmtime", &wt::Wt::new(WT_FUEL, None)),
        #[cfg(feature = "wasmtime")]
        ["js-mem", "wasmtime"] => js_mem("wasmtime", &wt::Wt::new(WT_FUEL, None)),
        ["langs"] => {
            let mut ok = langs("interp", &interp::Interp { max_pages: 16384 });
            #[cfg(feature = "wasmtime")]
            {
                ok &= langs("wasmtime", &wt::Wt::new(WT_FUEL, None));
            }
            std::process::exit(if ok { 0 } else { 1 });
        }
        [test, "interp"] => run(test, "interp", &interp::Interp { max_pages: 16384 }),
        #[cfg(feature = "wasmtime")]
        [test, "wasmtime"] => run(test, "wasmtime", &wt::Wt::new(WT_FUEL, None)),
        #[cfg(feature = "wasmtime")]
        [test, "wasmtime-pool"] => run(test, "wt-pool", &wt::Wt::new(WT_FUEL, Some(cells() as u32 + 2_000))),
        _ => eprintln!("usage: combd-spike all | <compute|startup|crash|density|fairness> <interp|wasmtime|wasmtime-pool|native>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQS: [&str; 5] = ["hello", "fib 20", "sieve 10000", "sort 5000", "nope"];

    fn answers<E: Engine>(eng: &E) -> Vec<Result<String, String>> {
        let prog = eng.load(&wasm()).unwrap();
        REQS.iter().map(|r| request(&mut eng.spawn(&prog).unwrap(), r, &mut HostState::default())).collect()
    }

    #[test]
    fn interp_matches_native() {
        let native: Vec<_> = REQS.iter().map(|r| Ok(combd_guest::run(r, || 1))).collect();
        assert_eq!(answers(&interp::Interp { max_pages: 16384 }), native);
    }

    #[cfg(feature = "wasmtime")]
    #[test]
    fn interp_matches_wasmtime() {
        assert_eq!(answers(&interp::Interp { max_pages: 16384 }), answers(&wt::Wt::new(WT_FUEL, None)));
    }

    #[cfg(feature = "aot")]
    #[test]
    fn aot_matches_interp() {
        assert_eq!(answers(&aot::Aot), answers(&interp::Interp { max_pages: 16384 }));
        assert!(langs("aot", &aot::Aot));
    }

    #[test]
    fn panic_traps_and_spin_yields() {
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&wasm()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        assert_eq!(request(&mut vm, "panic", &mut HostState::default()), Err("unreachable".into()));
        let mut vm = eng.spawn(&prog).unwrap();
        start(&mut vm, "spin", &mut HostState::default()).unwrap();
        for _ in 0..100 {
            assert!(matches!(vm.run(1000), Outcome::Yield));
        }
    }

    #[test]
    fn snapshot_mid_call_resumes_to_same_answer() {
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&wasm()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        start(&mut vm, "fib 25", &mut HostState::default()).unwrap();
        assert!(matches!(vm.run(100), Outcome::Yield));
        let mut copy = vm.snapshot().unwrap();
        let a = finish(&mut vm, SLICE, &mut HostState::default()).unwrap();
        let b = finish(&mut copy, SLICE, &mut HostState::default()).unwrap();
        assert_eq!(response(&vm, &a), "75025");
        assert_eq!(response(&copy, &b), "75025");
    }

    fn run_wat(src: &str, export: &str) -> Outcome {
        let eng = interp::Interp { max_pages: 16 };
        let prog = eng.load(&wat::parse_str(src).unwrap()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        vm.begin(export, &[]).unwrap();
        vm.run(u32::MAX)
    }

    /// Regression: an `if` (or `br_if`) right after a stack op used to leave the
    /// runtime stack pointer one slot too high, so the next stack op wrote to the
    /// wrong slot. TinyGo hits this pattern all the time.
    #[test]
    fn if_after_a_stack_op_keeps_sp_right() {
        let src = r#"(module
            (global $g (mut i32) (i32.const 0))
            (func (export "if") (result i32)
                global.get $g  i32.const 1  i32.const 0  select   ;; a stack op: 1
                if
                    global.get $g  i32.eqz                         ;; must read 0 here
                    if (result i32) i32.const 7 else i32.const 9 end
                    return
                end
                i32.const 5)
            (func (export "br_if") (result i32)
                block
                    global.get $g  i32.const 1  i32.const 0  select
                    br_if 0
                end
                global.get $g  i32.eqz
                if (result i32) i32.const 7 else i32.const 9 end))"#;
        for export in ["if", "br_if"] {
            assert!(matches!(run_wat(src, export), Outcome::Done(r) if r == [7]), "{export}");
        }
    }

    /// Register forms against the plain stack meaning, for local reads that are
    /// still pending when the same local gets overwritten.
    #[test]
    fn pending_local_reads_see_the_old_value() {
        let src = r#"(module (func (export "f") (result i32)
            (local $x i32)
            i32.const 10  local.set $x
            local.get $x                       ;; pending read of the old x (10)
            i32.const 3  local.set $x          ;; overwrite x
            local.get $x                       ;; 3
            i32.sub))"#;
        assert!(matches!(run_wat(src, "f"), Outcome::Done(r) if r == [7]));
    }

    /// Every language cell built by `langs/Dockerfile` must pass on our engine.
    #[test]
    fn every_language_passes() {
        if lang_cells().len() == 1 {
            eprintln!("skipping: build langs/out first (see langs/README.md)");
            return;
        }
        assert!(langs("interp", &interp::Interp { max_pages: 16384 }));
    }

    /// Security: a new cell must never see bytes left by a dead one.
    #[test]
    fn fresh_cells_never_see_old_memory() {
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&wasm()).unwrap();
        let clean = eng.spawn(&prog).unwrap();
        let len = (0u32..).step_by(4096).take_while(|&p| clean.read(p, 1).is_some()).count() as u32 * 4096;
        let expected = clean.read(0, len).unwrap();
        for _ in 0..20 {
            let mut old = eng.spawn(&prog).unwrap();
            request(&mut old, "sieve 3000000", &mut HostState::default()).unwrap(); // grows memory well past 1 MiB
            let mut p = 0;
            while old.write(p, &[0xAA; 4096]) {
                p += 4096;
            }
            drop(old);
            let new = eng.spawn(&prog).unwrap();
            assert_eq!(new.read(0, len).unwrap(), expected, "a fresh cell saw old memory");
            assert!(new.read(len, 1).is_none());
        }
    }

    #[test]
    fn memory_limit_is_enforced() {
        let eng = interp::Interp { max_pages: 64 }; // 4 MiB
        let prog = eng.load(&wasm()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        assert!(request(&mut vm, "sieve 5000000", &mut HostState::default()).is_err());
    }
}
