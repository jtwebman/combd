//! Phase 0: the engine race. Same Rust cell, our interpreter vs Wasmtime.
//!
//!   cargo run --release -- all              every test, each in a fresh process
//!   cargo run --release -- <test> <engine>  tests: compute startup crash density fairness
//!                                           engines: interp wasmtime (compute also: native)

#![forbid(unsafe_code)]

mod engine;
mod interp;
#[cfg(feature = "wasmtime")]
mod wt;

use engine::{CellVm, Engine, Outcome};
use std::time::{Duration, Instant};

const GUEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm");
/// Reductions (loop iterations and calls) per slice for our interpreter.
const SLICE: u32 = 2_500;
/// Fuel (about one per wasm op) per slice for Wasmtime. Both are tuned so a
/// spinning cell's slice is about 50 us on an M4 Max.
const WT_FUEL: u64 = 1_000_000;
const CELLS: usize = 10_000;

fn wasm() -> Vec<u8> {
    std::fs::read(GUEST).expect("build the guest first: cd guest && cargo build --release --target wasm32-unknown-unknown")
}

/// Our only host import: `combd.counter_incr`.
fn host(counter: &mut i64) -> [u64; 1] {
    *counter += 1;
    [*counter as u64]
}

/// Run the current call to the end, slice by slice.
fn finish<V: CellVm>(vm: &mut V, slice: u32, counter: &mut i64) -> Result<Vec<u64>, String> {
    loop {
        match vm.run(slice) {
            Outcome::Done(r) => return Ok(r),
            Outcome::Yield => {}
            Outcome::HostCall { .. } => vm.resume(&host(counter)),
            Outcome::Trap(t) => return Err(t),
        }
    }
}

/// Copy a request into the cell and begin `handle`. Nothing runs yet.
fn start<V: CellVm>(vm: &mut V, req: &str, counter: &mut i64) -> Result<(), String> {
    vm.begin("alloc", &[req.len() as u64])?;
    let ptr = finish(vm, u32::MAX, counter)?[0] as u32;
    if !vm.write(ptr, req.as_bytes()) {
        return Err("bad alloc pointer".into());
    }
    vm.begin("handle", &[ptr as u64, req.len() as u64])
}

fn response<V: CellVm>(vm: &V, r: &[u64]) -> String {
    let r = r[0];
    String::from_utf8_lossy(&vm.read((r >> 32) as u32, r as u32).unwrap_or_default()).into_owned()
}

fn request<V: CellVm>(vm: &mut V, req: &str, counter: &mut i64) -> Result<String, String> {
    start(vm, req, counter)?;
    let r = finish(vm, SLICE, counter)?;
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
        let out = request(&mut vm, req, &mut 0).unwrap();
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
        request(&mut vm, "hello", &mut 0).unwrap();
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
        request(&mut vm, "panic", &mut 0).expect_err("panic must trap");
        let t = Instant::now();
        let mut fresh = eng.spawn(prog).unwrap();
        assert_eq!(request(&mut fresh, "hello", &mut 0).unwrap(), "hello #1");
        total += t.elapsed();
    }
    println!("crash    {name:<9} trap -> fresh cell -> hello served: {:>7.1} us avg over {n}, host still up", us(total / n));
}

fn density<E: Engine>(name: &str, eng: &E, prog: &E::Program) {
    let before = rss_kb();
    let mut cells: Vec<E::Vm> = (0..CELLS).map(|_| eng.spawn(prog).unwrap()).collect();
    let idle = rss_kb();
    for vm in &mut cells {
        start(vm, "spin", &mut 0).unwrap();
        assert!(matches!(vm.run(SLICE), Outcome::Yield));
    }
    let paused = rss_kb();
    println!(
        "density  {name:<9} {CELLS} cells: idle {:>6.1} KB/cell   paused mid-loop {:>6.1} KB/cell   (RSS {} MB)",
        (idle - before) as f64 / CELLS as f64,
        (paused - before) as f64 / CELLS as f64,
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
                    let mut cells: Vec<E::Vm> = (t..CELLS).step_by(threads).map(|_| eng.spawn(prog).unwrap()).collect();
                    let (job, budget) = match neighbor {
                        Neighbor::Quiet => ("", 0),
                        Neighbor::Spinner => ("spin", SLICE),
                        Neighbor::HeavyNoPreempt => ("sort 200000", u32::MAX),
                    };
                    let mut bad = (neighbor != Neighbor::Quiet).then(|| {
                        let mut vm = eng.spawn(prog).unwrap();
                        start(&mut vm, job, &mut 0).unwrap();
                        vm
                    });
                    // Without preemption every hello waits a whole sort, so serve fewer.
                    let hellos = if neighbor == Neighbor::HeavyNoPreempt { 10 } else { cells.len() };
                    let (mut lat, mut bad_time, mut bad_slices, mut bad_max) = (vec![], Duration::ZERO, 0u64, Duration::ZERO);
                    for vm in cells.iter_mut().take(hellos) {
                        let since = Instant::now();
                        let mut counter = 0;
                        start(vm, "hello", &mut counter).unwrap();
                        if let Some(b) = bad.as_mut() {
                            let ts = Instant::now();
                            match b.run(budget) {
                                Outcome::Yield => {}
                                Outcome::Done(_) => start(b, job, &mut 0).unwrap(),
                                _ => panic!("bad neighbor misbehaved in an unexpected way"),
                            }
                            let d = ts.elapsed();
                            bad_time += d;
                            bad_slices += 1;
                            bad_max = bad_max.max(d);
                        }
                        finish(vm, SLICE, &mut counter).unwrap();
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
                let engines: &[&str] = if test == "compute" { &["native", "interp", "wasmtime"] } else { &["interp", "wasmtime"] };
                for e in engines {
                    // A fresh process per run keeps the memory numbers clean.
                    let ok = std::process::Command::new(std::env::current_exe().unwrap()).args([test, e]).status().unwrap();
                    assert!(ok.success(), "{test} {e} failed");
                }
            }
        }
        ["compute", "native"] => compute_native(),
        [test, "interp"] => run(test, "interp", &interp::Interp { max_pages: 16384 }),
        #[cfg(feature = "wasmtime")]
        [test, "wasmtime"] => run(test, "wasmtime", &wt::Wt::new(WT_FUEL)),
        _ => eprintln!("usage: combd-spike all | <compute|startup|crash|density|fairness> <interp|wasmtime|native>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQS: [&str; 5] = ["hello", "fib 20", "sieve 10000", "sort 5000", "nope"];

    fn answers<E: Engine>(eng: &E) -> Vec<Result<String, String>> {
        let prog = eng.load(&wasm()).unwrap();
        REQS.iter().map(|r| request(&mut eng.spawn(&prog).unwrap(), r, &mut 0)).collect()
    }

    #[test]
    fn interp_matches_native() {
        let native: Vec<_> = REQS.iter().map(|r| Ok(combd_guest::run(r, || 1))).collect();
        assert_eq!(answers(&interp::Interp { max_pages: 16384 }), native);
    }

    #[cfg(feature = "wasmtime")]
    #[test]
    fn interp_matches_wasmtime() {
        assert_eq!(answers(&interp::Interp { max_pages: 16384 }), answers(&wt::Wt::new(WT_FUEL)));
    }

    #[test]
    fn panic_traps_and_spin_yields() {
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&wasm()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        assert_eq!(request(&mut vm, "panic", &mut 0), Err("unreachable".into()));
        let mut vm = eng.spawn(&prog).unwrap();
        start(&mut vm, "spin", &mut 0).unwrap();
        for _ in 0..100 {
            assert!(matches!(vm.run(1000), Outcome::Yield));
        }
    }

    #[test]
    fn snapshot_mid_call_resumes_to_same_answer() {
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&wasm()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        start(&mut vm, "fib 25", &mut 0).unwrap();
        assert!(matches!(vm.run(100), Outcome::Yield));
        let mut copy = vm.snapshot().unwrap();
        let a = finish(&mut vm, SLICE, &mut 0).unwrap();
        let b = finish(&mut copy, SLICE, &mut 0).unwrap();
        assert_eq!(response(&vm, &a), "75025");
        assert_eq!(response(&copy, &b), "75025");
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
            request(&mut old, "sieve 3000000", &mut 0).unwrap(); // grows memory well past 1 MiB
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
        assert!(request(&mut vm, "sieve 5000000", &mut 0).is_err());
    }
}
