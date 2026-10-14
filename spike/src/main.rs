//! Phase 0: the engine race. Same Rust cell, our interpreter vs Wasmtime.
//!
//!   cargo run --release -- all              every test, each in a fresh process
//!   cargo run --release -- langs            every language's cell, on both engines
//!   cargo run --release -- <test> <engine>  tests: compute startup crash density fairness
//!                                           engines: interp wasmtime wasmtime-pool (compute also: native)

#![forbid(unsafe_code)]

#[cfg(feature = "aot-lite")]
mod aot;
mod aot_gen;
mod engine;
mod host;
mod own;
mod s3;
mod sim;
mod store;
mod http;
mod interp;
#[cfg(feature = "wasmtime")]
mod wt;

use engine::{CellVm, Engine, Outcome};
use host::HostState;
use std::time::{Duration, Instant};

const GUEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm");
/// `langs/js/app.js`, compiled by jsc (`jsc/build.sh`).
const JS_CELL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../jsc/out/app/target/wasm32-unknown-unknown/release/combd_js_app.wasm");
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
    if let Ok(js) = std::fs::read(JS_CELL) {
        out.push(("js".to_string(), js));
    }
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
                if name == "aot" && !cfg!(feature = "aot-guard") {
                    // Without fibers, compiled cells can't pause: a spin is killed at the CPU limit.
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

/// The requests every cell answers (`langs/js/requests.txt`).
fn requests() -> Vec<String> {
    std::fs::read_to_string(format!("{JS_DIR}/requests.txt")).unwrap().lines().map(String::from).collect()
}

/// Node's exact answers to `requests()`: the oracle (`langs/js/expected.txt`).
#[cfg(test)]
fn expected() -> Vec<String> {
    std::fs::read_to_string(format!("{JS_DIR}/expected.txt")).unwrap().lines().map(String::from).collect()
}

/// The Rust cell or the JS one.
fn cell(name: &str) -> Vec<u8> {
    match name {
        "rust" => wasm(),
        "js" => std::fs::read(JS_CELL).expect("build the JS cell first: jsc/build.sh"),
        _ => panic!("cell must be rust or js"),
    }
}

/// The API requests, on one cell.
fn api_bench<E: Engine>(name: &str, eng: &E, lang: &str) {
    let reqs = requests();
    let prog = eng.load(&cell(lang)).unwrap();
    let mut vm = eng.spawn(&prog).unwrap();
    let mut st = HostState::default();
    init(&mut vm, &mut st).unwrap();
    let only = std::env::var("COMBD_BENCH_ONLY").unwrap_or_default();
    for r in reqs.iter().filter(|r| r.starts_with("api") && (only.is_empty() || r.split(' ').nth(2) == Some(only.as_str()))) {
        let label: String = r.chars().take(22).collect();
        for _ in 0..200 {
            request(&mut vm, r, &mut st).unwrap();
        }
        let n: u32 = std::env::var("COMBD_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(5000);
        // Best of five rounds: a laptop's noise only ever adds time.
        let best = (0..5)
            .map(|_| {
                let t = Instant::now();
                for _ in 0..n {
                    request(&mut vm, r, &mut st).unwrap();
                }
                t.elapsed() / n
            })
            .min()
            .unwrap();
        let out = request(&mut vm, r, &mut st).unwrap();
        println!("{lang:>4}-api {name:<9} {label:<22} {:>9.2} us   -> {} bytes", us(best), out.len());
    }
}

fn api_bench_native() {
    let reqs = requests();
    for r in reqs.iter().filter(|r| r.starts_with("api")) {
        let label: String = r.chars().take(22).collect();
        for _ in 0..200 {
            combd_guest::run(r, || 1);
        }
        let n = 5000;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(combd_guest::run(std::hint::black_box(r), || 1));
        }
        let out = combd_guest::run(r, || 1);
        println!("rust-api {:<9} {label:<22} {:>9.2} us   -> {} bytes", "native", us(t.elapsed() / n), out.len());
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
    let b = vm.read((r >> 32) as u32, r as u32).unwrap_or_default();
    // The fast check first; lossy decoding only for bad bytes.
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn request<V: CellVm>(vm: &mut V, req: &str, st: &mut HostState) -> Result<String, String> {
    start(vm, req, st)?;
    let r = finish(vm, SLICE, st)?;
    let answer = response(vm, &r);
    // Cells that export `release` free the answer once we've read it.
    if vm.begin("release", &[r[0] >> 32, r[0] & 0xffff_ffff]).is_ok() {
        finish(vm, u32::MAX, st)?;
    }
    Ok(answer)
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
    main_with(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

fn main_with(args: &[&str]) {
    match args[..] {
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
        ["api-bench", "native"] => api_bench_native(),
        ["api-bench", e] => main_with(&["api-bench", e, "rust"]),
        ["api-bench", "interp", lang] => api_bench("interp", &interp::Interp { max_pages: 16384 }, lang),
        #[cfg(feature = "wasmtime")]
        ["api-bench", "wasmtime", lang] => api_bench("wasmtime", &wt::Wt::new(WT_FUEL, None), lang),
        #[cfg(feature = "aot-lite")]
        ["api-bench", "aot", lang] => api_bench("aot", &aot::Aot, lang),
        // The startup storage test, on its own: COMBD_S3=http://host:port ...
        ["s3-check"] => {
            let s3 = s3::S3::from_env().expect("set COMBD_S3 (and COMBD_S3_KEY, COMBD_S3_SECRET)");
            match s3.check() {
                Ok(()) => println!("s3-check {}: conditional writes hold, also under a 16-way race", s3.host),
                Err(e) => {
                    println!("s3-check {}: UNSAFE: {e}", s3.host);
                    std::process::exit(1);
                }
            }
        }
        // The D2 ownership simulator: `sim [seeds] [bug]`.
        ["sim", ..] => {
            let n: u64 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(1000);
            let bugs = match args.get(2).copied() {
                Some("skip_proof") => own::Bugs { skip_proof: true, ..Default::default() },
                Some("load_before_cas") => own::Bugs { load_before_cas: true, ..Default::default() },
                Some("no_manifest") => own::Bugs { no_manifest: true, ..Default::default() },
                Some("reuse_epoch") => own::Bugs { reuse_epoch: true, ..Default::default() },
                _ => own::Bugs::default(),
            };
            let t = Instant::now();
            let (stats, bad) = sim::run_many(0..n, bugs);
            println!("sim      {n} seeds in {:.1} s: {stats:?}", t.elapsed().as_secs_f64());
            if let Some((seed, why)) = bad {
                println!("sim      seed {seed} broke an invariant: {why}");
                std::process::exit(1);
            }
            println!("sim      every invariant held");
        }
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
        #[cfg(feature = "aot-lite")]
        ["langs", "aot"] => std::process::exit(if langs("aot", &aot::Aot) { 0 } else { 1 }),
        #[cfg(feature = "aot-lite")]
        [test, "aot"] => run(test, "aot", &aot::Aot),
        ["serve", engine, cell, port] => {
            let wasm = self::cell(cell);
            let port: u16 = port.parse().unwrap();
            match engine {
                "interp" => {
                    let eng = interp::Interp { max_pages: 16384 };
                    http::serve(&eng, &eng.load(&wasm).unwrap(), port)
                }
                #[cfg(feature = "aot-lite")]
                "aot" => http::serve(&aot::Aot, &aot::Aot.load(&wasm).unwrap(), port),
                #[cfg(feature = "wasmtime")]
                "wasmtime" => {
                    let eng = wt::Wt::new(WT_FUEL, None);
                    http::serve(&eng, &eng.load(&wasm).unwrap(), port)
                }
                _ => panic!("unknown engine"),
            }
        }
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
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

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

    #[cfg(feature = "aot-lite")]
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

    /// jsc's build of app.js must answer every request byte-for-byte like Node.
    #[test]
    fn js_matches_node() {
        let Ok(js) = std::fs::read(JS_CELL) else {
            eprintln!("skipping: build the JS cell first (jsc/build.sh)");
            return;
        };
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&js).unwrap();
        for (r, want) in requests().iter().zip(expected()) {
            let (mut vm, mut st) = (eng.spawn(&prog).unwrap(), HostState::default());
            init(&mut vm, &mut st).unwrap();
            assert_eq!(request(&mut vm, r, &mut st).unwrap(), want, "{r}");
        }
    }

    /// The Rust API routes must answer byte-for-byte like Node running app.js.
    #[test]
    fn rust_api_matches_node() {
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&wasm()).unwrap();
        for (r, want) in requests().iter().zip(expected()).filter(|(r, _)| r.starts_with("api")) {
            let got = request(&mut eng.spawn(&prog).unwrap(), r, &mut HostState::default()).unwrap();
            assert_eq!(got, want, "{r}");
        }
    }

    /// A compiled cell yields mid-call (on a fiber) and resumes to the right answer.
    #[cfg(feature = "aot-guard")]
    #[test]
    /// The JS cell's fib is plain recursion with no loop: it must yield too.
    fn aot_yields_mid_call_and_resumes() {
        let mut cells = vec![("rust", wasm())];
        cells.extend(std::fs::read(JS_CELL).ok().map(|js| ("js", js)));
        for (lang, wasm) in cells {
            let Ok(prog) = aot::Aot.load(&wasm) else { continue };
            let mut vm = aot::Aot.spawn(&prog).unwrap();
            let mut st = HostState::default();
            init(&mut vm, &mut st).unwrap();
            start(&mut vm, "fib 27", &mut st).unwrap();
            let mut yields = 0;
            let r = loop {
                match vm.run(100) {
                    Outcome::Yield => yields += 1,
                    Outcome::Done(r) => break r,
                    other => panic!("{lang}: unexpected {:?}", matches!(other, Outcome::Trap(_))),
                }
            };
            eprintln!("{lang}: fib 27 yielded {yields} times");
            assert!(yields > 10, "{lang}: only {yields} yields");
            assert_eq!(response(&vm, &r), "196418", "{lang}");
        }
    }

    /// A stuck compiled cell is still killed fast: the CPU limit is the backstop.
    #[cfg(feature = "aot-lite")]
    #[test]
    fn aot_spin_is_killed_quickly() {
        let prog = aot::Aot.load(&wasm()).unwrap();
        let mut vm = aot::Aot.spawn(&prog).unwrap();
        let t = Instant::now();
        assert!(request(&mut vm, "spin", &mut HostState::default()).unwrap_err().contains("OutOfFuel"));
        assert!(t.elapsed() < Duration::from_millis(500), "took {:?}", t.elapsed());
    }

    fn oob_traps_then_recovers<E: Engine>(eng: &E) {
        let prog = eng.load(&wasm()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        assert!(request(&mut vm, "oob", &mut HostState::default()).is_err(), "out-of-bounds read must trap");
        let mut fresh = eng.spawn(&prog).unwrap();
        assert_eq!(request(&mut fresh, "hello", &mut HostState::default()).unwrap(), "hello #1");
    }

    /// Security: a read past a cell's memory traps on every engine, and the
    /// host carries on. With guard pages this goes through the fault handler.
    #[test]
    fn out_of_bounds_traps_on_every_engine() {
        oob_traps_then_recovers(&interp::Interp { max_pages: 16384 });
        #[cfg(feature = "wasmtime")]
        oob_traps_then_recovers(&wt::Wt::new(WT_FUEL, None));
        #[cfg(feature = "aot-lite")]
        for _ in 0..100 {
            oob_traps_then_recovers(&aot::Aot); // many faults in a row, same thread
        }
    }

    /// Regression: answers used to leak, so a busy cell hit its memory limit
    /// after about a million list requests and crashed.
    #[test]
    fn answers_are_released() {
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&wasm()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        let mut st = HostState::default();
        for _ in 0..30_000 {
            request(&mut vm, "api GET /api/users", &mut st).unwrap(); // 730 bytes each
        }
        assert!(vm.read(16 << 20, 1).is_none(), "memory grew past 16 MiB: answers are leaking");
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

    /// A server for Durable Object tests: (port, stop switch, thread).
    fn do_server<E: Engine + Sync + Send + 'static>(eng: E, cfg: http::Config) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>)
    where
        E::Program: Sync + Send,
    {
        static PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
        let port = 21000 + (std::process::id() % 2000) as u16 * 10 + PORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stop = cfg.stop.clone();
        let t = std::thread::spawn(move || {
            let prog = eng.load(&wasm()).unwrap();
            http::serve_with(&eng, &prog, port, cfg);
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return (port, stop, t);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("server on {port} didn't start");
    }

    fn cfg(data: &std::path::Path, threads: usize) -> http::Config {
        http::Config {
            threads,
            data: data.to_path_buf(),
            idle: Duration::from_secs(60),
            max_objects: 10_000,
            stop: Arc::default(),
            sync_delay: Duration::ZERO,
        }
    }

    fn stat(port: u16, key: &str) -> i64 {
        let (_, body) = http(port, "GET", "/_combd/stats", "");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()[key].as_i64().unwrap()
    }

    /// Wait up to `ms` for `f` to hold.
    fn eventually(ms: u64, f: impl Fn() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(ms) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    /// One HTTP request on its own connection: (status, body).
    fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "{method} {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut out = String::new();
        let r = s.read_to_string(&mut out);
        assert!(r.is_ok() && out.len() >= 12, "{method} {path}: no answer ({r:?}, {out:?})");
        let status = out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_string())
    }

    /// What's on disk for an object, read with a separate connection.
    fn on_disk(data: &std::path::Path, name: &str, sql: &str) -> String {
        let db = store::Db::open(&store::path_for(data, name)).unwrap();
        String::from_utf8(db.exec_json(format!("{{\"q\":{sql:?}}}").as_bytes())).unwrap()
    }

    /// Durable Objects: state lives across requests, every answered write is
    /// on disk (the output gate), and a crash rolls its request back.
    fn objects_keep_state_and_roll_back_crashes<E: Engine + Sync + Send + 'static>(eng: E, threads: usize)
    where
        E::Program: Sync + Send,
    {
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(eng, cfg(&dir.0, threads));
        for n in 1..=3 {
            assert_eq!(http(port, "POST", "/do/room1/incr", ""), (200, format!("{{\"count\":{n}}}")));
        }
        // The output gate: once we have the answer, the write is on disk.
        assert!(on_disk(&dir.0, "room1", "SELECT v FROM kv").contains("[[3]]"));
        assert_eq!(http(port, "POST", "/do/room1/msg", "hi").0, 201);
        // A crash after a write: 500, and the write is gone.
        assert_eq!(http(port, "POST", "/do/room1/crash", "").0, 500);
        assert!(!on_disk(&dir.0, "room1", "SELECT body FROM msgs").contains("never saved"));
        // A fresh cell loads its state back from storage.
        assert_eq!(http(port, "GET", "/do/room1/count", ""), (200, "{\"count\":3}".into()));
        assert_eq!(http(port, "GET", "/do/room1/msgs", ""), (200, "{\"msgs\":[\"hi\"]}".into()));
        // Objects are separate.
        assert_eq!(http(port, "GET", "/do/room2/count", ""), (200, "{\"count\":0}".into()));
        // Many clients at once on one object: it runs them one at a time, and
        // none are lost or doubled.
        let answers: Vec<String> = (0..8)
            .map(|_| std::thread::spawn(move || (0..10).map(|_| http(port, "POST", "/do/busy/incr", "").1).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let mut counts: Vec<i64> = answers.iter().map(|a| serde_json::from_str::<serde_json::Value>(a).unwrap()["count"].as_i64().unwrap()).collect();
        counts.sort();
        assert_eq!(counts, (1..=80).collect::<Vec<_>>());
        assert!(on_disk(&dir.0, "busy", "SELECT v FROM kv").contains("[[80]]"));
        // Group commit: concurrent writers share fsyncs.
        let (commits, fsyncs) = (stat(port, "commits"), stat(port, "fsyncs"));
        assert!(fsyncs < commits, "{fsyncs} fsyncs for {commits} commits");
    }

    /// The output gate with a slow disk: no answer leaves before the fsync
    /// that covers it, and a read waits behind a write that isn't on disk.
    #[test]
    fn answers_wait_for_the_fsync() {
        let dir = store::tempdir::Dir::new();
        let c = http::Config { sync_delay: Duration::from_millis(300), ..cfg(&dir.0, 1) };
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, c);
        // Opening the object writes its tables, so the first answer waits too.
        let t = Instant::now();
        assert_eq!(http(port, "POST", "/do/g/incr", "").0, 200);
        assert!(t.elapsed() >= Duration::from_millis(300), "answered in {:?}", t.elapsed());
        // With nothing pending, a read goes right out.
        let t = Instant::now();
        assert_eq!(http(port, "GET", "/do/g/count", ""), (200, "{\"count\":1}".into()));
        assert!(t.elapsed() < Duration::from_millis(250), "read took {:?}", t.elapsed());
        // A read right behind a write waits for the write's fsync.
        let w = std::thread::spawn(move || http(port, "POST", "/do/g/incr", ""));
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        assert_eq!(http(port, "GET", "/do/g/count", ""), (200, "{\"count\":2}".into()));
        assert!(t.elapsed() >= Duration::from_millis(150), "read didn't wait: {:?}", t.elapsed());
        assert_eq!(w.join().unwrap().0, 200);
    }

    #[test]
    fn objects_on_the_interpreter() {
        objects_keep_state_and_roll_back_crashes(interp::Interp { max_pages: 16384 }, 1);
    }

    #[cfg(feature = "aot-guard")]
    #[test]
    fn objects_on_aot() {
        objects_keep_state_and_roll_back_crashes(aot::Aot, 1);
    }

    /// Four event loops: requests for an object reach its one owner from any
    /// loop, so it still runs them one at a time.
    #[test]
    fn objects_across_event_loops() {
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, cfg(&dir.0, 4));
        let answers: Vec<String> = (0..8)
            .map(|_| std::thread::spawn(move || (0..10).map(|_| http(port, "POST", "/do/shared/incr", "").1).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let mut counts: Vec<i64> = answers.iter().map(|a| serde_json::from_str::<serde_json::Value>(a).unwrap()["count"].as_i64().unwrap()).collect();
        counts.sort();
        assert_eq!(counts, (1..=80).collect::<Vec<_>>());
        for i in 0..20 {
            assert_eq!(http(port, "POST", &format!("/do/o{i}/incr"), "").0, 200);
        }
        assert!(stat(port, "forwarded") > 0, "nothing was forwarded");
        assert_eq!(stat(port, "objects_live"), 21);
    }

    /// Alarms fire, a crashing one is retried, and one set before a restart
    /// still fires after it.
    #[test]
    fn alarms_fire_retry_and_survive_restarts() {
        let dir = store::tempdir::Dir::new();
        let eng = || interp::Interp { max_pages: 16384 };
        let (port, stop, t) = do_server(eng(), cfg(&dir.0, 2));
        let fired = |port: u16, o: &str| http(port, "GET", &format!("/do/{o}/alarms"), "").1;
        assert_eq!(http(port, "POST", "/do/a1/alarm", "100").0, 200);
        assert_eq!(fired(port, "a1"), r#"{"fired":0,"pending":true}"#);
        assert!(eventually(2000, || fired(port, "a1") == r#"{"fired":1,"pending":false}"#), "{}", fired(port, "a1"));
        // The handler crashes on its first try: rolled back, then retried.
        assert_eq!(http(port, "POST", "/do/a2/alarm-crash", "50").0, 200);
        assert!(eventually(3000, || fired(port, "a2") == r#"{"fired":1,"pending":false}"#), "{}", fired(port, "a2"));
        assert!(stat(port, "alarms_failed") >= 1);
        // Set one, stop the node, start a new one on the same data.
        assert_eq!(http(port, "POST", "/do/a3/alarm", "700").0, 200);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        t.join().unwrap();
        let (port, _, _) = do_server(eng(), cfg(&dir.0, 3));
        assert!(eventually(3000, || fired(port, "a3") == r#"{"fired":1,"pending":false}"#), "{}", fired(port, "a3"));
    }

    /// Idle objects leave memory and come back from storage; the cap holds.
    #[test]
    fn idle_objects_are_evicted() {
        let dir = store::tempdir::Dir::new();
        let c = http::Config { idle: Duration::from_millis(150), max_objects: 5, ..cfg(&dir.0, 1) };
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, c);
        assert_eq!(http(port, "POST", "/do/e1/incr", "").0, 200);
        assert!(eventually(2000, || stat(port, "objects_live") == 0), "never evicted");
        assert_eq!(http(port, "GET", "/do/e1/count", ""), (200, "{\"count\":1}".into()));
        for i in 0..20 {
            http(port, "POST", &format!("/do/m{i}/incr"), "");
        }
        assert!(eventually(1000, || stat(port, "objects_live") <= 5), "cap not held: {}", stat(port, "objects_live"));
        assert!(stat(port, "evictions") >= 16);
    }
}
