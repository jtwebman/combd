//! Phase 0: the engine race. Same Rust cell, our interpreter vs Wasmtime.
//!
//!   cargo run --release -- all              every test, each in a fresh process
//!   cargo run --release -- langs            every language's cell, on both engines
//!   cargo run --release -- <test> <engine>  tests: compute startup crash density fairness
//!                                           engines: interp wasmtime wasmtime-pool (compute also: native)

#![forbid(unsafe_code)]

mod admin;
#[cfg(feature = "aot-lite")]
mod aot;
#[cfg(feature = "aot-lite")]
mod both;
#[cfg(feature = "aot-lite")]
mod build;
#[cfg(feature = "aot-lite")]
mod builds;
mod aot_gen;
mod app;
mod catalog;
mod click;
mod catnode;
mod catsim;
mod deploy;
mod engine;
mod fleet;
mod host;
mod own;
mod s3;
mod secrets;
mod sim;
mod store;
mod tls;
mod wire;
mod ws;
mod manifest;
mod pg;
mod pool;
#[cfg(test)]
mod spectest;
#[cfg(feature = "fuzz")]
mod fuzz;
mod fuzz_host;
mod http;
mod interp;
mod kv;
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

/// One keep-alive HTTP connection, for `fleet-load`.
struct Client {
    addr: String,
    s: Option<std::io::BufReader<std::net::TcpStream>>,
}

impl Client {
    /// (status, body), or `None` if the connection failed (it reconnects next time).
    fn call(&mut self, method: &str, path: &str) -> Option<(u16, String)> {
        use std::io::{BufRead, Read, Write};
        if self.s.is_none() {
            let s = std::net::TcpStream::connect(&self.addr).ok()?;
            s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
            s.set_nodelay(true).ok()?;
            self.s = Some(std::io::BufReader::new(s));
        }
        let r = (|| -> std::io::Result<(u16, String)> {
            let s = self.s.as_mut().unwrap();
            write!(s.get_mut(), "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\n\r\n")?;
            let mut line = String::new();
            s.read_line(&mut line)?;
            let status = line.get(9..12).and_then(|c| c.parse().ok()).ok_or_else(|| std::io::Error::other("bad status"))?;
            let mut len = 0;
            loop {
                line.clear();
                s.read_line(&mut line)?;
                if line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            s.read_exact(&mut body)?;
            Ok((status, String::from_utf8_lossy(&body).into_owned()))
        })();
        if r.is_err() {
            self.s = None;
        }
        r.ok()
    }
}

/// Load a fleet with increments through random nodes, then check every
/// object: acknowledged <= count <= acknowledged + failed. The low side means
/// no acknowledged write was lost; the high side means none was applied
/// twice. (A failed answer may still have been applied, like a timeout.)
fn fleet_load(secs: u64, threads: usize, objects: usize, nodes: &str) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let nodes: Vec<String> = nodes.split(',').map(String::from).collect();
    // Fresh objects for every run, so runs don't add up.
    let run = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let acked: Vec<AtomicU64> = (0..objects).map(|_| AtomicU64::new(0)).collect();
    let failed: Vec<AtomicU64> = (0..objects).map(|_| AtomicU64::new(0)).collect();
    let start = Instant::now();
    let until = start + Duration::from_secs(secs);
    let results: Vec<(Vec<u32>, Vec<(u64, u64)>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let (nodes, acked, failed) = (&nodes, &acked, &failed);
                sc.spawn(move || {
                    let mut rng = sim::Rng::new(t as u64 + 1);
                    let mut clients: Vec<Client> = nodes.iter().map(|a| Client { addr: a.clone(), s: None }).collect();
                    let mut lat = vec![];
                    // Stretches of failures: (from, to) in ms since start.
                    let mut bad: Vec<(u64, u64)> = vec![];
                    while Instant::now() < until {
                        let o = rng.below(objects as u64) as usize;
                        let n = rng.below(nodes.len() as u64) as usize;
                        let t0 = Instant::now();
                        let r = clients[n].call("POST", &format!("/do/load-{run}-{o}/incr"));
                        let ms = start.elapsed().as_millis() as u64;
                        if matches!(r, Some((200, _))) {
                            acked[o].fetch_add(1, Relaxed);
                            lat.push(t0.elapsed().as_micros() as u32);
                        } else {
                            failed[o].fetch_add(1, Relaxed);
                            match bad.last_mut() {
                                Some(b) if ms - b.1 < 100 => b.1 = ms,
                                _ => bad.push((ms, ms)),
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                    (lat, bad)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let elapsed = start.elapsed().as_secs_f64();
    let mut lat: Vec<u32> = results.iter().flat_map(|(l, _)| l.iter().copied()).collect();
    lat.sort_unstable();
    let pct = |p: f64| lat.get(((lat.len() as f64 * p) as usize).min(lat.len().saturating_sub(1))).copied().unwrap_or(0) as f64 / 1000.0;
    let worst_gap = results.iter().flat_map(|(_, b)| b.iter()).map(|(a, b)| b - a).max().unwrap_or(0);
    let (ok, bad): (u64, u64) = (acked.iter().map(|a| a.load(Relaxed)).sum(), failed.iter().map(|f| f.load(Relaxed)).sum());
    println!(
        "load     {:.0} acknowledged writes/s ({ok} ok, {bad} failed) in {elapsed:.1} s; p50 {:.2} ms, p99 {:.2} ms; longest failure stretch {worst_gap} ms",
        ok as f64 / elapsed,
        pct(0.5),
        pct(0.99)
    );
    // Check every object, through any node that answers.
    let mut c: Vec<Client> = nodes.iter().map(|a| Client { addr: a.clone(), s: None }).collect();
    let mut good = true;
    for o in 0..objects {
        let (a, f) = (acked[o].load(Relaxed), failed[o].load(Relaxed));
        let count = (0..100).find_map(|i| {
            let k = i % c.len();
            match c[k].call("GET", &format!("/do/load-{run}-{o}/count")) {
                Some((200, b)) => serde_json::from_str::<serde_json::Value>(&b).ok()?["count"].as_u64(),
                _ => {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            }
        });
        match count {
            Some(n) if a <= n && n <= a + f => {}
            other => {
                good = false;
                println!("load     object load-{o}: count {other:?}, but {a} acknowledged and {f} failed");
            }
        }
    }
    println!("load     {} objects checked: {}", objects, if good { "acknowledged <= count <= acknowledged + failed, for every one" } else { "BROKEN" });
    good
}

/// The Rust cell (the only one, since jsc's JS cell retired).
fn cell(name: &str) -> Vec<u8> {
    match name {
        "rust" => wasm(),
        _ => panic!("cell must be rust"),
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

/// Run one admin API call from the command line, print its answer, and exit
/// non-zero if it failed.
fn cli(url: &str, f: impl FnOnce(&dyn Fn(&str, &str, &[u8]) -> Result<serde_json::Value, String>) -> Result<serde_json::Value, String>) {
    let token = std::env::var("COMBD_TOKEN").unwrap_or_default();
    let tls = std::env::var("COMBD_ADMIN_CA").ok().map(|ca| wire::admin_client(std::path::Path::new(&ca)));
    let name = std::env::var("COMBD_ADMIN_NAME").unwrap_or_else(|_| "node0.combd".into());
    let r = (|| {
        let tls = tls.transpose()?;
        let call = |method: &str, path: &str, body: &[u8]| -> Result<serde_json::Value, String> {
            let (status, v) = admin::request(url, tls.as_ref().map(|c| (c, name.as_str())), &token, method, path, body)?;
            if status == 200 { Ok(v) } else { Err(format!("{status}: {}", v["error"].as_str().unwrap_or("?"))) }
        };
        f(&call)
    })();
    match r {
        Ok(v) => println!("{v}"),
        Err(e) => {
            eprintln!("combd     {e}");
            std::process::exit(1);
        }
    }
}

/// Answer a `HostCall` and hand the result back to the cell. A call the host
/// must wait on (Postgres) leaves `st.pending` set and the cell paused; the
/// event loop resumes it with `resume_db` later.
fn serve<V: CellVm>(vm: &mut V, import: u32, args: &[u64], st: &mut HostState) -> Result<(), String> {
    let imp = vm.import(import).ok_or("unknown import")?.clone();
    st.can_park = true; // an interpreter cell can wait any time
    let r = host::call(&imp, args, &mut VmMem(vm), st)?;
    if st.pending.is_some() {
        st.pending_results = imp.results;
        return Ok(());
    }
    vm.resume(&r);
    Ok(())
}

/// Run the current call to the end, slice by slice.
fn finish<V: CellVm>(vm: &mut V, slice: u32, st: &mut HostState) -> Result<Vec<u64>, String> {
    loop {
        match vm.run(slice) {
            Outcome::Done(r) => return Ok(r),
            Outcome::Yield => {}
            Outcome::HostCall { import, args } => {
                serve(vm, import, &args, st)?;
                if st.pending.take().is_some() {
                    return Err("a database call can't wait here".into());
                }
            }
            Outcome::Trap(t) => return Err(t),
        }
    }
}

/// Both engines: each module runs as its manifest says.
#[cfg(feature = "aot-lite")]
fn both() -> both::Both {
    both::Both { interp: interp::Interp { max_pages: 16384 }, aot: aot::Aot, builds: None }
}

/// A fleet node's: its builds from the bucket, trusting COMBD_BUILDER_KEYS,
/// kept in COMBD_BUILDS_DIR (default `<data>/builds`).
#[cfg(feature = "aot-lite")]
fn both_in(fc: &fleet::FleetCfg, data: &std::path::Path) -> Result<both::Both, String> {
    let keys = match std::env::var("COMBD_BUILDER_KEYS") {
        Ok(f) => builds::parse_keys(&std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?),
        Err(_) => vec![],
    };
    let dir = std::env::var_os("COMBD_BUILDS_DIR").map_or(data.join("builds"), Into::into);
    let b = builds::Builds::new(fc.s3.clone(), dir, keys, fc.node)?;
    Ok(both::Both { builds: Some(std::sync::Arc::new(b)), ..both() })
}

/// Copy a request into the cell and begin `handle`. Nothing runs yet.
fn start<V: CellVm>(vm: &mut V, req: &str, st: &mut HostState) -> Result<(), String> {
    start_export(vm, "handle", req, st)
}

/// The same, with requests for `export` (`handle_batch` takes several).
fn start_export<V: CellVm>(vm: &mut V, export: &str, req: &str, st: &mut HostState) -> Result<(), String> {
    // A new request: a fresh host-time budget, wherever the engine keeps it.
    st.host_time = Default::default();
    if let Some(h) = vm.host_state() {
        h.host_time = Default::default();
    }
    vm.begin("alloc", &[req.len() as u64])?;
    let ptr = finish(vm, u32::MAX, st)?[0] as u32;
    if !vm.write(ptr, req.as_bytes()) {
        return Err("bad alloc pointer".into());
    }
    vm.begin(export, &[ptr as u64, req.len() as u64])
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

/// Linux: proportional memory, where a page shared by N mappings counts
/// 1/N (RSS would count it N times). Elsewhere: RSS.
fn pss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/smaps_rollup").ok().and_then(|s| s.lines().find(|l| l.starts_with("Pss:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())).unwrap_or_else(rss_kb)
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

fn ws_load(addr: &str, host: &str, path: &str, n: usize, msg: &str, ca: Option<&str>) {
    let tls = ca.map(|ca| wire::admin_client(std::path::Path::new(ca)).unwrap());
    let mut held = Vec::with_capacity(n);
    let t = Instant::now();
    for i in 0..n {
        let tcp = std::net::TcpStream::connect(addr).unwrap_or_else(|e| panic!("socket {i}: {e}"));
        let open = |s: &mut dyn ReadWrite| -> Result<(), String> {
            write!(s, "GET {path} HTTP/1.1\r\nhost: {host}\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n").map_err(|e| e.to_string())?;
            let (mut head, mut b) = (vec![], [0u8; 1]);
            while !head.ends_with(b"\r\n\r\n") {
                if s.read(&mut b).map_err(|e| e.to_string())? == 0 {
                    return Err("closed in the handshake".into());
                }
                head.push(b[0]);
            }
            if !head.starts_with(b"HTTP/1.1 101") {
                return Err(String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_string());
            }
            // One masked text frame (a client must mask; a zero mask keeps it as is).
            let p = msg.as_bytes();
            let mut f = if p.len() < 126 { vec![0x81, 0x80 | p.len() as u8] } else { vec![0x81, 0x80 | 126, (p.len() >> 8) as u8, p.len() as u8] };
            f.extend_from_slice(&[0, 0, 0, 0]);
            f.extend_from_slice(p);
            s.write_all(&f).map_err(|e| e.to_string())?;
            s.flush().map_err(|e| e.to_string())
        };
        let r = match &tls {
            // The TLS state goes once the message is out: the socket stays
            // open, and the server holds an idle connection, as measured.
            Some(cfg) => {
                let name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
                let mut s = rustls::StreamOwned::new(rustls::ClientConnection::new(cfg.clone(), name).unwrap(), tcp);
                let r = open(&mut s);
                held.push(s.into_parts().1);
                r
            }
            None => {
                let mut s = tcp;
                let r = open(&mut s);
                held.push(s);
                r
            }
        };
        if let Err(e) = r {
            println!("ws-load: socket {i} refused: {e}");
            break;
        }
        if (i + 1) % (n / 10).max(1) == 0 {
            println!("ws-load: {} open ({:.0} s)", i + 1, t.elapsed().as_secs_f64());
        }
    }
    // Read and drop what comes, as a browser would (TLS bytes, unread): a
    // socket left full holds its data in the kernel, and a client holding
    // tens of thousands runs out of memory. And say how many the server closed.
    println!("ws-load: holding {}", held.len());
    for s in &held {
        let _ = s.set_nonblocking(true);
    }
    let mut open = vec![true; held.len()];
    let mut buf = [0u8; 16 * 1024];
    for round in 1u64.. {
        for (s, up) in held.iter_mut().zip(open.iter_mut()).filter(|(_, up)| **up) {
            loop {
                match std::io::Read::read(s, &mut buf) {
                    Ok(0) => *up = false,
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => *up = false,
                }
                break;
            }
        }
        if round % 15 == 0 {
            println!("ws-load: holding {} ({} closed by the server)", open.iter().filter(|u| **u).count(), open.iter().filter(|u| !**u).count());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

trait ReadWrite: std::io::Read + std::io::Write {}
impl<T: std::io::Read + std::io::Write> ReadWrite for T {}

fn ws_hold(host: &str, port: u16, path: &str, n: usize, msg: &str, pid: &str) {
    use std::io::{Read, Write};
    let rss = || std::fs::read_to_string(format!("/proc/{pid}/status")).ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())).unwrap_or(0);
    // What the server's allocator has out, in KB (its stats).
    let heap = || {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /_combd/stats HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let body = out.split("\r\n\r\n").nth(1).unwrap_or("{}");
        serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| v["heap_in_use"].as_u64()).unwrap_or(0) / 1024
    };
    let (before, heap_before) = (rss(), heap());
    let mut held = Vec::with_capacity(n);
    for i in 0..n {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nhost: {host}\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n").unwrap();
        let mut head = vec![];
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            assert!(s.read(&mut b).unwrap() == 1, "socket {i}: closed in the handshake");
            head.push(b[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 101"), "socket {i}: {}", String::from_utf8_lossy(&head));
        // One masked text frame (a client must mask).
        let p = msg.as_bytes();
        let mut f = vec![0x81, 0x80 | 126, (p.len() >> 8) as u8, p.len() as u8, 0, 0, 0, 0];
        if p.len() < 126 {
            f = vec![0x81, 0x80 | p.len() as u8, 0, 0, 0, 0];
        }
        f.extend_from_slice(p); // the mask is 0: the payload stays as is
        s.write_all(&f).unwrap();
        held.push(s);
        if (i + 1) % (n / 4).max(1) == 0 {
            println!("  {:>7} sockets: server {:>7} KB, {:>5.2} KB/socket", i + 1, rss(), (rss() - before) as f64 / (i + 1) as f64);
        }
    }
    // Let them settle (hibernate), then measure again.
    std::thread::sleep(Duration::from_secs(5));
    println!("ws-hold: {n} sockets, settled: {:.2} KB/socket (server {} KB, {} KB before); heap in use {:.2} KB/socket", (rss() - before) as f64 / n as f64, rss(), before, (heap() - heap_before) as f64 / n as f64);
}

fn fresh_bench(wasm: &str, n: usize, rest: &[&str]) {
    let eng = interp::Interp { max_pages: 16384 };
    let prog = eng.load(&std::fs::read(wasm).unwrap()).unwrap();
    let environ = std::sync::Arc::new(rest.iter().filter_map(|a| a.strip_prefix("env=")).map(|e| e.as_bytes().to_vec()).collect::<Vec<_>>());
    let req = rest.iter().copied().find(|a| !a.starts_with("env=")).expect("a request");
    let cell = || {
        let mut vm = eng.spawn(&prog).unwrap();
        let mut st = HostState { environ: environ.clone(), ..HostState::default() };
        init(&mut vm, &mut st).unwrap();
        (vm, st)
    };
    for round in 0..2 {
        let (mut vm, mut st) = cell();
        let t = Instant::now();
        for _ in 0..n {
            request(&mut vm, req, &mut st).unwrap();
        }
        let warm = t.elapsed();
        let t = Instant::now();
        for _ in 0..n {
            let (mut vm, mut st) = cell();
            request(&mut vm, req, &mut st).unwrap();
        }
        let fresh = t.elapsed();
        println!("fresh-bench round {round}: warm {:>7.2} us/request, fresh cell each {:>7.2} us/request", us(warm) / n as f64, us(fresh) / n as f64);
    }
}

fn density_app<E: Engine>(eng: &E, wasm: &str, n: usize, rest: &[&str]) {
    let prog = eng.load(&std::fs::read(wasm).unwrap()).unwrap();
    let environ: Vec<Vec<u8>> = rest.iter().filter_map(|a| a.strip_prefix("env=")).map(|e| e.as_bytes().to_vec()).collect();
    let reqs: Vec<&str> = rest.iter().copied().filter(|a| !a.starts_with("env=")).collect();
    let environ = std::sync::Arc::new(environ);
    let before = pss_kb();
    let t = Instant::now();
    let mut cells: Vec<_> = (0..n)
        .map(|_| {
            let mut vm = eng.spawn(&prog).unwrap();
            let mut st = HostState { environ: environ.clone(), ..HostState::default() };
            init(&mut vm, &mut st).unwrap();
            (vm, st)
        })
        .collect();
    let spawned = (t.elapsed(), pss_kb());
    let t = Instant::now();
    for (vm, st) in &mut cells {
        for r in &reqs {
            if let Err(e) = request(vm, r, st) {
                panic!("{r}: {e}");
            }
            st.ws_out.clear();
        }
    }
    let served = (t.elapsed(), pss_kb());
    // What the host would keep if each went to sleep now (`hibernate = "auto"`).
    let frozen: Vec<usize> = cells.iter().filter_map(|(vm, _)| vm.freeze()).map(|f| f.len()).collect();
    if !frozen.is_empty() {
        println!("  frozen:   {:>6} bytes/cell", frozen.iter().sum::<usize>() / frozen.len());
    }
    let per = |kb: u64| (kb - before) as f64 / n as f64;
    println!("density-app {wasm}: {n} cells");
    println!("  started:  {:>6.1} KB/cell  {:>6.1} us/cell", per(spawned.1), us(spawned.0) / n as f64);
    println!("  served {} requests each: {:>6.1} KB/cell  {:>6.1} us/cell", reqs.len(), per(served.1), us(served.0) / n as f64);
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
        // Memory per cell of a real module, after real requests:
        // `density-app <module.wasm> <n> [env=K=V]... [request]...`.
        ["density-app", wasm, n, ref rest @ ..] => density_app(&interp::Interp { max_pages: 16384 }, wasm, n.parse().unwrap(), rest),
        // The same, compiled: a build in COMBD_AOT_DIR (or built in).
        #[cfg(feature = "aot-lite")]
        ["density-app-aot", wasm, n, ref rest @ ..] => density_app(&aot::Aot, wasm, n.parse().unwrap(), rest),
        // Hold `n` WebSockets open on a server, each after one message, and
        // print the server's memory per socket: `ws-hold <host> <port>
        // <path> <n> <message> <server pid>`.
        ["ws-hold", host, port, path, n, msg, pid] => ws_hold(host, port.parse().unwrap(), path, n.parse().unwrap(), msg, pid),
        // A warm cell against a fresh one for every request: `fresh-bench
        // <module.wasm> <n> [env=K=V]... <request>`.
        ["fresh-bench", wasm, n, ref rest @ ..] => fresh_bench(wasm, n.parse().unwrap(), rest),
        // Hold `n` WebSockets open on a server (through a proxy, maybe, and
        // TLS with `ca.pem`), each after one message, until killed; measure
        // the server from outside: `ws-load <ip:port> <host> <path> <n>
        // <message> [ca.pem]`.
        ["ws-load", addr, host, path, n, msg, ref tls @ ..] => ws_load(addr, host, path, n.parse().unwrap(), msg, tls.first().copied()),
        // Million's pages, clicking: `ws-click <ip:port> <host> <path> <n>
        // <ca.pem> [max clicks, 30] [max seconds between, 20]`.
        ["ws-click", addr, host, path, n, ca, ref rest @ ..] => {
            let arg = |i: usize, d: u64| rest.get(i).map_or(d, |v| v.parse().unwrap());
            click::run(addr, host, path, n.parse().unwrap(), ca, arg(0, 30), arg(1, 20))
        }
        ["compute", "native"] => compute_native(),
        ["api-bench", "native"] => api_bench_native(),
        ["api-bench", e] => main_with(&["api-bench", e, "rust"]),
        ["api-bench", "interp", lang] => api_bench("interp", &interp::Interp { max_pages: 16384 }, lang),
        #[cfg(feature = "wasmtime")]
        ["api-bench", "wasmtime", lang] => api_bench("wasmtime", &wt::Wt::new(WT_FUEL, None), lang),
        #[cfg(feature = "aot-lite")]
        ["api-bench", "aot", lang] => api_bench("aot", &aot::Aot, lang),
        // Load a fleet and check it: `fleet-load secs threads objects host:port,host:port,...`.
        ["fleet-load", secs, threads, objects, nodes] => {
            let ok = fleet_load(secs.parse().unwrap(), threads.parse().unwrap(), objects.parse().unwrap(), nodes);
            std::process::exit(if ok { 0 } else { 1 });
        }
        // Certificates for a fleet's wire: a CA and one per node.
        // The startup storage test, on its own: is this bucket safe to run a
        // fleet on? (COMBD_S3, COMBD_S3_BUCKET, COMBD_S3_KEY, COMBD_S3_SECRET.)
        ["bucket-check"] => {
            let Some(b) = s3::S3::from_env() else {
                eprintln!("combd     set COMBD_S3 (and COMBD_S3_BUCKET, COMBD_S3_KEY, COMBD_S3_SECRET, COMBD_S3_REGION)");
                std::process::exit(1);
            };
            let t = Instant::now();
            match b.check() {
                Ok(()) => println!("combd     {}/{} passed the storage test in {:.1} s: conditional writes hold, under a race too", b.host, b.bucket, t.elapsed().as_secs_f64()),
                Err(e) => {
                    eprintln!("combd     {}/{} failed the storage test: {e}", b.host, b.bucket);
                    std::process::exit(1);
                }
            }
        }
        ["wire-certs", dir, nodes] => match wire::make_certs(std::path::Path::new(dir), nodes.parse().unwrap()) {
            Ok(()) => println!("wire     a CA and {nodes} node certificates in {dir}"),
            Err(e) => {
                eprintln!("wire     {e}");
                std::process::exit(1);
            }
        },
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
        // The ownership simulator: `sim [seeds|A..B] [bug|none] [replicas] [splits|only-splits] [wipes]`.
        // The catalog's simulator: `catsim N [bug]`.
        ["catsim", n, ref rest @ ..] => {
            let n: u64 = n.parse().unwrap();
            let mut bugs = catalog::Bugs::default();
            match rest.first().copied() {
                Some("ack_before_write") => bugs.ack_before_write = true,
                Some("blind_slot") => bugs.blind_slot = true,
                Some("uncovered_check") => bugs.uncovered_check = true,
                Some("trim_no_gate") => bugs.trim_no_gate = true,
                Some("trim_early") => bugs.trim_early = true,
                Some("read_no_gate") => bugs.read_no_gate = true,
                Some(b) => panic!("unknown bug {b}"),
                None => {}
            }
            let t = Instant::now();
            let (st, bad) = catsim::run_many(0..n, bugs);
            println!("catsim   {n} seeds in {:.1} s: {st:?}", t.elapsed().as_secs_f64());
            match bad {
                Some((seed, e)) => {
                    println!("catsim   seed {seed}: {e}");
                    std::process::exit(1);
                }
                None => println!("catsim   every invariant held"),
            }
        }
        ["sim", ..] => {
            // `N` seeds from 0, or `A..B` to replay some.
            let seeds = match args.get(1).and_then(|v| v.split_once("..")) {
                Some((a, b)) => a.parse().unwrap()..b.parse().unwrap(),
                None => 0..args.get(1).and_then(|v| v.parse().ok()).unwrap_or(1000),
            };
            let n = seeds.end - seeds.start;
            let mut bugs = own::Bugs::default();
            match args.get(2).copied() {
                Some("skip_proof") => bugs.skip_proof = true,
                Some("load_before_cas") => bugs.load_before_cas = true,
                Some("no_manifest") => bugs.no_manifest = true,
                Some("reuse_epoch") => bugs.reuse_epoch = true,
                Some("ack_one") => bugs.ack_one = true,
                Some("no_fence") => bugs.no_fence = true,
                Some("trunc_early") => bugs.trunc_early = true,
                Some("seal_early") => bugs.seal_early = true,
                Some("trust_wiped") => bugs.trust_wiped = true,
                Some("wipe_forgets_fence") => bugs.wipe_forgets_fence = true,
                Some("assume_alive") => bugs.assume_alive = true,
                Some("wait_when_down") => bugs.wait_when_down = true,
                Some("lease_no_promise") => bugs.lease_no_promise = true,
                Some("lease_overtrust") => bugs.lease_overtrust = true,
                Some("lease_trusts_kept") => bugs.lease_trusts_kept = true,
                Some("gc_unproven") => bugs.gc_unproven = true,
                Some("gc_newest_image") => bugs.gc_newest_image = true,
                _ => {}
            }
            let replicas = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(0);
            let t = Instant::now();
            // Extra faults, any of: `splits` (network splits), `only-splits`
            // (splits and no other faults), `wipes` (a lost disk), `loss` (a
            // node lost for good), `beats` (each object beats every 50 ms),
            // `far` (peer round trips of 100 to 250 ms).
            let more = args.get(4..).unwrap_or_default();
            let beats = more.contains(&"beats");
            let peer_ms = more.contains(&"far").then_some((100, 250));
            let loss = more.contains(&"loss");
            let only = more.contains(&"only-splits");
            let splits = only || more.contains(&"splits");
            let wipes = more.contains(&"wipes");
            // `uploads=MS`: every seed waits this long (and 1 MB); else by seed.
            let uploads = more.iter().find_map(|a| a.strip_prefix("uploads=")?.parse().ok()).map(|ms| (ms, 1 << 20));
            let (stats, bad) = sim::run_with(seeds, &|| {
                let mut s = sim::Settings { replicas, bugs, splits, wipes, loss, uploads, beats, peer_ms, ..Default::default() };
                if only {
                    (s.lost, s.crash, s.freeze) = (0.0, 0.0, 0.0);
                }
                s
            });
            println!("sim      {n} seeds in {:.1} s: {stats:?}", t.elapsed().as_secs_f64());
            if let Some((seed, why)) = bad {
                println!("sim      seed {seed} broke an invariant: {why}");
                std::process::exit(1);
            }
            println!("sim      every invariant held");
        }
        // Differential fuzzing against Wasmtime: `fuzz N [start]`.
        #[cfg(feature = "fuzz")]
        ["fuzz", n, ref rest @ ..] => {
            let start: u64 = rest.first().and_then(|v| v.parse().ok()).unwrap_or(0);
            let n: u64 = n.parse().unwrap();
            let t = Instant::now();
            let st = fuzz::run(start..start + n, true);
            println!("fuzz     {n} seeds in {:.1} s: {} modules, {} calls ({} trapped; {} on a thawed cell), {} inconclusive", t.elapsed().as_secs_f64(), st.modules, st.calls, st.traps, st.thawed, st.inconclusive);
            for (why, count) in &st.refused {
                println!("fuzz     refused {count:5}  {why}");
            }
            for (kind, count) in &st.trap_kinds {
                println!("fuzz     trap    {count:5}  {kind}");
            }
            for (seed, why) in st.mismatches.iter().take(20) {
                println!("fuzz     MISMATCH seed {seed}: {why}");
            }
            if !st.mismatches.is_empty() {
                println!("fuzz     {} mismatches (modules saved as fuzz-<seed>.wasm)", st.mismatches.len());
                std::process::exit(1);
            }
            println!("fuzz     no mismatches");
        }
        // Fuzzing the host API: `fuzz-host N [start]`.
        ["fuzz-host", n, ref rest @ ..] => {
            let start: u64 = rest.first().and_then(|v| v.parse().ok()).unwrap_or(0);
            let n: u64 = n.parse().unwrap();
            let t = Instant::now();
            let st = fuzz_host::run(start..start + n, fuzz_host::SLOW);
            println!("fuzz-host {n} seeds in {:.1} s: {} calls ({} crashed their cell), slowest {:?}: {}", t.elapsed().as_secs_f64(), st.calls, st.crashed, st.slowest.0, st.slowest.1);
            for p in st.problems.iter().take(30) {
                println!("fuzz-host PROBLEM {p}");
            }
            if !st.problems.is_empty() {
                println!("fuzz-host {} problems", st.problems.len());
                std::process::exit(1);
            }
            println!("fuzz-host no problems");
        }
        // The AOT tier's fuzz batch: `fuzz-gen N [start]`, build, `fuzz-aot N [start]`.
        #[cfg(feature = "fuzz")]
        ["fuzz-dump", seed, out] => std::fs::write(out, fuzz::module(seed.parse().unwrap()).unwrap().0).unwrap(),
        #[cfg(feature = "fuzz")]
        ["fuzz-replay", path] => match fuzz::replay(std::path::Path::new(path)) {
            Ok(()) => println!("fuzz     {path}: the engines agree"),
            Err(e) => {
                println!("fuzz     {path}: MISMATCH {e}");
                std::process::exit(1);
            }
        },
        #[cfg(feature = "fuzz")]
        ["fuzz-gen", n, ref rest @ ..] => {
            let start: u64 = rest.first().and_then(|v| v.parse().ok()).unwrap_or(0);
            let st = fuzz::gen_aot(start..start + n.parse::<u64>().unwrap(), std::path::Path::new("aot/src"));
            println!("fuzz-gen {} modules written to aot/src/fuzz", st.modules);
            for (why, count) in &st.refused {
                println!("fuzz-gen aot-gen refused {count:5}  {why}");
            }
        }
        #[cfg(all(feature = "fuzz", feature = "aot-lite"))]
        ["fuzz-aot", n, ref rest @ ..] => {
            let start: u64 = rest.first().and_then(|v| v.parse().ok()).unwrap_or(0);
            let n: u64 = n.parse().unwrap();
            let t = Instant::now();
            let st = fuzz::run_aot(start..start + n);
            println!("fuzz-aot {n} seeds in {:.1} s: {} modules, {} calls ({} trapped), {} inconclusive", t.elapsed().as_secs_f64(), st.modules, st.calls, st.traps, st.inconclusive);
            for (seed, why) in st.mismatches.iter().take(20) {
                println!("fuzz-aot MISMATCH seed {seed}: {why}");
            }
            if !st.mismatches.is_empty() {
                std::process::exit(1);
            }
            println!("fuzz-aot no mismatches");
        }
        // A module's build, as a node loads it: `aot-build <module.wasm>
        // <out.so> [target] [cpu]`, in ./target/aot-builds.
        #[cfg(feature = "aot-lite")]
        ["aot-build", input, output, ref rest @ ..] => {
            let wasm = std::fs::read(input).unwrap();
            let aot = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("aot");
            let work = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/aot-builds");
            let t = std::time::Instant::now();
            match build::build(&wasm, &aot, &work, std::path::Path::new(output), rest.first().copied(), rest.get(1).copied()) {
                Ok(()) => println!("aot-build {input} -> {output} in {:.1} s", t.elapsed().as_secs_f64()),
                Err(e) => {
                    eprintln!("aot-build {input}: {e}");
                    std::process::exit(1);
                }
            }
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
        // A fleet node serving what's deployed to its bucket (COMBD_S3,
        // COMBD_NODE, COMBD_PEERS...), with the admin API if COMBD_ADMIN_PORT.
        ["serve-fleet", engine, port] => {
            let port = port.parse().unwrap();
            let cfg = http::Config::from_env();
            let Some(fc) = cfg.fleet.as_ref() else {
                eprintln!("combd     serve-fleet needs fleet mode: COMBD_S3, COMBD_NODE, COMBD_PEERS");
                std::process::exit(1);
            };
            let admin = admin::AdminCfg::from_env(fc.wire_certs.as_deref(), fc.node).unwrap_or_else(|e| {
                eprintln!("combd     {e}");
                std::process::exit(1);
            });
            let r = match engine {
                "interp" => http::serve_fleet_apps(&interp::Interp { max_pages: 16384 }, port, cfg, admin),
                // Each module as its manifest says (compiled: those in this build, for now).
                #[cfg(feature = "aot-lite")]
                "both" => both_in(fc, &cfg.data).and_then(|eng| http::serve_fleet_apps(&eng, port, cfg, admin)),
                _ => Err("serve-fleet: interp, or both (with the AOT tier)".into()),
            };
            if let Err(e) = r {
                eprintln!("combd     {e}");
                std::process::exit(1);
            }
        }
        // A builder's key (its private half into `file`), and the public half
        // for every node's COMBD_BUILDER_KEYS.
        #[cfg(feature = "aot-lite")]
        ["builder-key", file] => match builds::new_key(std::path::Path::new(file)) {
            Ok(public) => println!("a builder key is in {file}; for COMBD_BUILDER_KEYS: {public}"),
            Err(e) => {
                eprintln!("combd     {e}");
                std::process::exit(1);
            }
        },
        // Build compiled modules for the fleet in COMBD_S3 (for this binary's
        // pin), signing with COMBD_BUILDER_KEY, until killed.
        #[cfg(feature = "aot-lite")]
        ["builder"] => {
            let s3 = s3::S3::from_env().unwrap_or_else(|| {
                eprintln!("combd     builder: needs the fleet's bucket (COMBD_S3, ...)");
                std::process::exit(1);
            });
            let key = builds::load_key(std::path::Path::new(&std::env::var("COMBD_BUILDER_KEY").unwrap_or_else(|_| "builder.key".into()))).unwrap_or_else(|e| {
                eprintln!("combd     builder: {e}");
                std::process::exit(1);
            });
            let id = std::env::var("COMBD_BUILDER_ID").unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_else(|_| "builder".into()));
            let aot = std::env::var_os("COMBD_AOT_SRC").map_or(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("aot"), Into::into);
            let work = std::env::var_os("COMBD_BUILD_WORK").map_or(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/aot-builds"), Into::into);
            builds::builder(&s3, &id, &key, &aot, &work, &std::sync::atomic::AtomicBool::new(false));
        }
        // Before an update: exit 0 if every compiled module the fleet runs is
        // built for this binary (asking for those that aren't), 1 if not yet,
        // 2 if it can't tell. The node's own env: COMBD_S3..., COMBD_DATA.
        #[cfg(feature = "aot-lite")]
        ["builds-ready"] => {
            let r = s3::S3::from_env().ok_or_else(|| "no bucket (COMBD_S3)".to_string()).and_then(|s3| http::builds_ready(&s3, &http::Config::from_env().data));
            match r {
                Ok(true) => println!("combd     builds-ready: every compiled module is built for {}", builds::pin_id(combd_aot::PIN)),
                Ok(false) => {
                    println!("combd     builds-ready: not yet: builds for {} asked for", builds::pin_id(combd_aot::PIN));
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("combd     builds-ready: {e}");
                    std::process::exit(2);
                }
            }
        }
        // A new admin token, and its line for COMBD_ADMIN_TOKENS.
        ["admin-token", ref scopes @ ..] if !scopes.is_empty() => {
            let (token, line) = admin::new_token(scopes);
            println!("token (give it to your CI; it's shown once): {token}");
            println!("line for the tokens file:                   {line}");
        }
        // The admin API, from the command line: COMBD_TOKEN, and for https
        // COMBD_ADMIN_CA (the fleet's ca.pem) and COMBD_ADMIN_NAME (node0.combd).
        ["deploy", dir, org, url] => cli(url, |call| {
            let dir = std::path::Path::new(dir);
            let m = manifest::load(dir)?.ok_or("no manifest in that folder")?;
            let b = deploy::bundle(dir)?;
            call("PUT", &format!("/v1/apps/{org}/{}", m.app), &b)
        }),
        ["status", app, url] => cli(url, |call| call("GET", &format!("/v1/apps/{app}"), b"")),
        ["rollback", app, version, url] => cli(url, |call| call("POST", &format!("/v1/apps/{app}/rollback"), serde_json::json!({ "version": version }).to_string().as_bytes())),
        ["remove", app, url] => cli(url, |call| call("DELETE", &format!("/v1/apps/{app}"), b"")),
        ["vars", app, url, ref sets @ ..] => cli(url, |call| {
            let mut m = serde_json::Map::new();
            for kv in sets.iter() {
                let (k, v) = kv.split_once('=').ok_or(format!("{kv}: want NAME=value (NAME= to unset)"))?;
                m.insert(k.into(), if v.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(v.into()) });
            }
            call("PUT", &format!("/v1/apps/{app}/vars"), serde_json::Value::Object(m).to_string().as_bytes())
        }),
        // Secrets: the value from stdin (never the command line, which
        // other users and shell history can see).
        ["secret", "put", app, name, url] => cli(url, |call| {
            let mut v = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut v).map_err(|e| e.to_string())?;
            let v = v.strip_suffix('\n').unwrap_or(&v);
            call("PUT", &format!("/v1/apps/{app}/secrets/{name}"), v.as_bytes())
        }),
        ["secret", "rm", app, name, url] => cli(url, |call| call("DELETE", &format!("/v1/apps/{app}/secrets/{name}"), b"")),
        // A new master key for a fleet's secrets (every node gets a copy).
        ["master-key", file] => {
            if let Err(e) = secrets::new_master(std::path::Path::new(file)) {
                eprintln!("combd     {e}");
                std::process::exit(1);
            }
            println!("a new master key is in {file}: give every node a copy, with COMBD_MASTER_KEY_FILE");
        }
        // Every app under `root/<org>/<app>/`, one node, shared process.
        ["serve-apps", engine, root, port] => {
            let (root, port) = (std::path::Path::new(root), port.parse().unwrap());
            match engine {
                "interp" => http::serve_apps(&interp::Interp { max_pages: 16384 }, root, port),
                #[cfg(feature = "aot-lite")]
                "aot" => http::serve_apps(&aot::Aot, root, port),
                #[cfg(feature = "aot-lite")]
                "both" => http::serve_apps(&both(), root, port),
                _ => panic!("unknown engine (interp, aot, both)"),
            }
        }
        // An app: its manifest and modules in `dir`, the `dev` environment.
        ["serve-app", engine, dir, port] => {
            let (dir, port) = (std::path::Path::new(dir), port.parse().unwrap());
            match engine {
                "interp" => http::serve_app(&interp::Interp { max_pages: 16384 }, dir, port),
                #[cfg(feature = "aot-lite")]
                "aot" => http::serve_app(&aot::Aot, dir, port),
                #[cfg(feature = "aot-lite")]
                "both" => http::serve_app(&both(), dir, port),
                _ => panic!("unknown engine (interp, aot, both)"),
            }
        }
        ["serve", engine, cell, port] => {
            let wasm = self::cell(cell);
            let port: u16 = port.parse().unwrap();
            match engine {
                "interp" => {
                    let eng = interp::Interp { max_pages: 16384 };
                    http::serve(&eng, eng.load(&wasm).unwrap(), port)
                }
                #[cfg(feature = "aot-lite")]
                "aot" => http::serve(&aot::Aot, aot::Aot.load(&wasm).unwrap(), port),
                #[cfg(feature = "wasmtime")]
                "wasmtime" => {
                    let eng = wt::Wt::new(WT_FUEL, None);
                    http::serve(&eng, eng.load(&wasm).unwrap(), port)
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
    use std::collections::HashMap;
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

    /// The interpreter's fused ops, each at its edges (an address that wraps,
    /// a shift past 31, a trap halfway through a move, NaN), against Wasmtime.
    /// Random modules rarely make these exact patterns; compiled code always does.
    #[test]
    fn fused_ops_match_wasmtime() {
        let wasm = wat::parse_str(
            r#"(module
              (memory (export "memory") 1)
              (global $sp (mut i32) (i32.const 4096))
              (func (export "load_add") (param i32) (result i32) (i32.load (i32.add (local.get 0) (i32.const -8))))
              (func (export "load_add64") (param i32) (result i64) (i64.load (i32.add (local.get 0) (i32.const 8))))
              (func (export "add_shl") (param i32 i32) (result i32) (i32.add (local.get 0) (i32.shl (local.get 1) (i32.const 35))))
              (func (export "shl_add") (param i32 i32) (result i32) (i32.add (i32.shl (local.get 1) (i32.const 2)) (local.get 0)))
              (func (export "move64") (param i32 i32) (i64.store offset=8 (local.get 0) (i64.load offset=16 (local.get 1))))
              (func (export "move32") (param i32 i32) (i32.store (local.get 0) (i32.load (local.get 1))))
              (func (export "move8") (param i32 i32) (i32.store8 (local.get 0) (i32.load8_u (local.get 1))))
              (func (export "move_far") (param i32 i32) (i64.store offset=70000 (local.get 0) (i64.load (local.get 1))))
              (func (export "peek") (param i32) (result i64) (i64.load (local.get 0)))
              (func (export "poke") (param i32 i64) (i64.store (local.get 0) (local.get 1)))
              (func (export "rsub") (param i32) (result i32) (i32.sub (i32.const 5) (local.get 0)))
              (func (export "frame") (param i32) (result i32) (local i32)
                (global.set $sp (local.tee 1 (i32.sub (global.get $sp) (i32.const 16))))
                (i32.store (local.get 1) (local.get 0))
                (local.set 0 (i32.add (i32.load (local.get 1)) (global.get $sp)))
                (global.set $sp (i32.add (local.get 1) (i32.const 16)))
                (local.get 0))
              (func (export "sp") (result i32) (global.get $sp))
              (func (export "flt") (param f64 f64) (result i32) (block (br_if 0 (f64.lt (local.get 0) (local.get 1))) (return (i32.const 0))) (i32.const 1))
              (func (export "fge") (param f64 f64) (result i32) (block (br_if 0 (f64.ge (local.get 0) (local.get 1))) (return (i32.const 0))) (i32.const 1))
              (func (export "fmath") (param f64 f64) (result f64) (f64.div (f64.mul (f64.add (local.get 0) (local.get 1)) (f64.sub (local.get 0) (local.get 1))) (local.get 1)))
              (func (export "count") (param i32) (result i32) (local i32)
                (block (loop (br_if 1 (i32.ge_u (local.get 1) (local.get 0))) (local.set 1 (i32.add (local.get 1) (i32.const 1))) (br 0)))
                (local.get 1))
              (func (export "table") (param i32) (result i32)
                (block (result i32) (block (result i32) (i32.const 7) (local.get 0) (br_table 0 1 0)) (i32.const 100) (i32.add)))
              (func $deep (param i32) (result i32) (local i32 i32 i32 i32 i32 i32 i32 i32 i32)
                (if (result i32) (i32.eqz (local.get 0)) (then (local.get 9)) (else (i32.add (call $deep (i32.sub (local.get 0) (i32.const 1))) (i32.const 1)))))
              (func (export "deep") (param i32) (result i32) (call $deep (local.get 0)))
            )"#,
        )
        .unwrap();
        let f = |x: f64| x.to_bits();
        let calls: Vec<(&str, Vec<u64>)> = vec![
            ("poke", vec![0, 0x1122334455667788]),
            ("poke", vec![16, 0x99aabbccddeeff00]),
            ("load_add", vec![8]),
            ("load_add", vec![4]),
            ("load_add", vec![0x10004]),
            ("load_add64", vec![0]),
            ("load_add64", vec![0xffff_fff8]),
            ("load_add64", vec![65528]),
            ("add_shl", vec![1, 3]),
            ("add_shl", vec![0xffff_ffff, 0x8000_0001]),
            ("shl_add", vec![10, 0x4000_0001]),
            ("move64", vec![100, 0]),
            ("peek", vec![108]),
            ("move64", vec![65530, 0]),
            ("move64", vec![200, 65520]),
            ("peek", vec![208]),
            ("move32", vec![300, 16]),
            ("peek", vec![300]),
            ("move8", vec![400, 17]),
            ("peek", vec![400]),
            ("move_far", vec![8, 0]),
            ("move_far", vec![0, 0]),
            ("rsub", vec![7]),
            ("rsub", vec![0x8000_0000]),
            ("frame", vec![9]),
            ("sp", vec![]),
            ("flt", vec![f(1.0), f(2.0)]),
            ("flt", vec![f(f64::NAN), f(2.0)]),
            ("fge", vec![f(f64::NAN), f(2.0)]),
            ("fge", vec![f(2.0), f(2.0)]),
            ("fmath", vec![f(3.5), f(-0.25)]),
            ("count", vec![0]),
            ("count", vec![1000]),
            ("table", vec![0]),
            ("table", vec![1]),
            ("table", vec![9]),
            ("deep", vec![3]),
            ("deep", vec![500]),
        ];
        fn transcript<E: Engine>(eng: &E, wasm: &[u8], calls: &[(&str, Vec<u64>)]) -> Vec<String> {
            let prog = eng.load(wasm).unwrap();
            let mut vm = eng.spawn(&prog).unwrap();
            calls
                .iter()
                .map(|(name, args)| {
                    vm.begin(name, args).unwrap();
                    loop {
                        match vm.run(1_000) {
                            Outcome::Yield => continue,
                            Outcome::Done(r) => break format!("{name}{args:x?} = {r:x?}"),
                            Outcome::Trap(_) => break format!("{name}{args:x?} traps"),
                            Outcome::HostCall { .. } => panic!("no imports here"),
                        }
                    }
                })
                .collect()
        }
        let ours = transcript(&interp::Interp { max_pages: 16384 }, &wasm, &calls);
        assert_eq!(ours, transcript(&wt::Wt::new(WT_FUEL, None), &wasm, &calls));
        // And the edges land where they should: an address add wraps (it
        // doesn't trap), a move that traps on its load writes nothing.
        for want in ["load_add[4] traps", "load_add64[fffffff8] = [1122334455667788]", "move64[fffa, 0] traps", "move64[c8, fff0] traps", "peek[d0] = [0]", "move_far[0, 0] traps", "add_shl[1, 3] = [19]"] {
            assert!(ours.iter().any(|l| l == want), "{want}: {ours:#?}");
        }
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

    /// A cell that loops over slow SQL is stopped: each statement after
    /// 100 ms, and the request after a second of host time in all (the
    /// cell's fuel doesn't count host time, so without this it could loop
    /// for as long as it liked at almost no cost to itself).
    #[test]
    fn a_request_gets_a_second_of_host_time() {
        let q = r#"{"q":"WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT count(*) FROM r"}"#;
        let src = format!(r#"(module
            (import "combd" "sql" (func $sql (param i32 i32) (result i32)))
            (memory 1)
            (data (i32.const 0) "{}")
            (func (export "spin") (loop (drop (call $sql (i32.const 0) (i32.const {}))) (br 0))))"#, q.replace('"', "\\\""), q.len());
        let dir = store::tempdir::Dir::new();
        let db = std::sync::Arc::new(std::sync::Mutex::new(store::Db::open(&dir.0.join("o.sqlite")).unwrap()));
        db.lock().unwrap().begin().unwrap();
        let mut st = HostState { db: Some(db), ..HostState::default() };
        let eng = interp::Interp { max_pages: 16 };
        let mut vm = eng.spawn(&eng.load(&wat::parse_str(&src).unwrap()).unwrap()).unwrap();
        vm.begin("spin", &[]).unwrap();
        let t = Instant::now();
        let r = finish(&mut vm, SLICE, &mut st);
        assert!(r.as_ref().is_err_and(|e| e.contains("host time")), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    /// A cell picks the types of its own imports. One that declares a host
    /// function with fewer parameters than it really takes must crash only
    /// itself, never the host (it used to index past the arguments and panic).
    #[test]
    fn a_wrongly_typed_import_crashes_only_its_cell() {
        let src = r#"(module
            (import "combd" "sql" (func $sql (result i64)))
            (import "wasi_snapshot_preview1" "fd_write" (func $w (param i32) (result i32)))
            (memory 1)
            (func (export "sql") (result i64) call $sql)
            (func (export "write") (result i32) i32.const 1 call $w))"#;
        let eng = interp::Interp { max_pages: 16 };
        for export in ["sql", "write"] {
            let r = eng.load(&wat::parse_str(src).unwrap()).and_then(|prog| {
                let mut vm = eng.spawn(&prog)?;
                vm.begin(export, &[])?;
                finish(&mut vm, SLICE, &mut HostState::default())
            });
            assert!(r.is_err(), "{export}: {r:?}");
        }
    }

    /// Regression, found by differential fuzzing (seed 6831): `big == extend(0)`
    /// swapped its operands so the constant could be an immediate, but `big`
    /// doesn't fit one, so it went to the scratch slot, which held the other
    /// operand. It compared `big` with itself and said "equal".
    #[test]
    fn a_commuted_compare_keeps_both_operands() {
        let src = r#"(module
            (func (export "eq") (result i32)
                i64.const 910823536302486501  i32.const 0  i64.extend_i32_s  i64.eq)
            (func (export "add") (result i64)
                i64.const 0x100000000  i32.const 5  i64.extend_i32_s  i64.add))"#;
        assert!(matches!(run_wat(src, "eq"), Outcome::Done(r) if r == [0]));
        assert!(matches!(run_wat(src, "add"), Outcome::Done(r) if r == [0x100000005]));
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
    fn aot_yields_mid_call_and_resumes() {
        let cells = vec![("rust", wasm())];
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

    /// A cell frozen when idle is the bytes it changed; thawed, it goes on
    /// where it was, and the two never share.
    #[test]
    fn a_frozen_cell_thaws_where_it_was() {
        let eng = interp::Interp { max_pages: 16384 };
        let prog = eng.load(&wasm()).unwrap();
        let mut vm = eng.spawn(&prog).unwrap();
        for _ in 0..3 {
            request(&mut vm, "api GET /api/hits", &mut HostState::default()).unwrap();
        }
        assert!(eng.note(&prog).is_none(), "laid out by wasm-ld: nothing to say");
        let bare = eng.load(&wat::parse_str("(module (memory 1))").unwrap()).unwrap();
        assert!(eng.note(&bare).is_some_and(|n| n.contains("keeps its whole stack")));
        let saved = vm.freeze().expect("an idle cell freezes");
        // What it changed: its 100 users (built once, kept), and the heap.
        assert!(saved.len() < 16 * 1024, "{} bytes", saved.len());
        let mut back = eng.thaw(&prog, &saved).unwrap();
        assert_eq!(request(&mut back, "api GET /api/hits", &mut HostState::default()).unwrap(), "200 {\"hits\":4}");
        assert_eq!(request(&mut vm, "api GET /api/hits", &mut HostState::default()).unwrap(), "200 {\"hits\":4}", "the original goes its own way");
        for cut in [1, 3, 5, 9] {
            assert!(eng.thaw(&prog, &saved[..saved.len() - cut]).is_err(), "a freeze cut short by {cut} is refused");
        }
    }

    /// Past the connection cap, and while memory is short, a node answers
    /// 503 to new connections and sockets; the ones it has go on.
    #[test]
    fn a_full_node_refuses_new_work() {
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, http::Config { max_conns: 2, ..cfg(&dir.0, 1) });
        let keep = |port: u16| {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            write!(s, "GET /_combd/ping HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
            let mut b = [0u8; 12];
            s.read_exact(&mut b).unwrap();
            (s, String::from_utf8_lossy(&b).into_owned())
        };
        let (a, b) = (keep(port), keep(port));
        assert_eq!((a.1.as_str(), b.1.as_str()), ("HTTP/1.1 200", "HTTP/1.1 200"));
        assert_eq!(keep(port).1, "HTTP/1.1 503", "a third, past max_conns");
        drop((a, b));
        assert!(eventually(2000, || keep(port).1 == "HTTP/1.1 200"), "room again once they've gone");
        // A budget it's already over: it refuses, once it has measured.
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, http::Config { memory_budget: Some(1 << 20), ..cfg(&dir.0, 1) });
        // This machine itself still gets in (its checks see how full it is);
        // what's new from it is refused.
        assert!(eventually(3000, || stat(port, "pressure") == 2), "over its memory budget");
        let refused = Ws::open(port, "/do/full/ws").err().expect("no new socket on a full node");
        assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
    }

    /// Full, a node stops listening: a connection from elsewhere is refused
    /// at TCP (so a browser tries the name's next address), while this
    /// machine itself still gets in. With room again, it listens again.
    #[test]
    fn a_full_node_refuses_at_tcp() {
        // One of this machine's own addresses that isn't loopback (a UDP
        // "connect" sends nothing). None: nothing to test from.
        let Some(me) = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|u| u.connect("192.0.2.1:9").and(u.local_addr())).ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified()) else {
            eprintln!("skipping: no address but loopback");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, http::Config { max_conns: 2, ..cfg(&dir.0, 1) });
        let from_elsewhere = || std::net::TcpStream::connect_timeout(&(me, port).into(), Duration::from_secs(2)).map(|_| ()).map_err(|e| e.kind());
        assert_eq!(from_elsewhere(), Ok(()), "listening while there's room");
        let keep = || std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let held = (keep(), keep());
        assert!(eventually(3000, || from_elsewhere() == Err(std::io::ErrorKind::ConnectionRefused)), "at max_conns, it stops listening");
        drop(held);
        assert!(eventually(3000, || from_elsewhere() == Ok(())), "room again: it listens");
        assert_eq!(stat(port, "tcp_refusing"), 0);
        assert_eq!(http(port, "GET", "/_combd/ping", "").0, 200);
    }

    /// A socket closes while its session is mid-request, with `on_close =
    /// "drop"`: the session goes, and when the request finishes, the node
    /// goes on (it once took the loop down: "object of a running task").
    #[test]
    fn a_session_dropped_mid_request_doesnt_take_the_node_down() {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::write(app.0.join("combd.toml"), "[app]\nname = \"slow\"\n[ws.slow]\nmodule = \"classic\"\nroutes = [\"/slow\"]\non_close = \"drop\"\nhibernate = \"off\"\n").unwrap();
        std::fs::copy(GUEST, app.0.join("modules/classic.wasm")).unwrap();
        let eng = interp::Interp { max_pages: 16384 };
        let (a, _) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(eng, cfg(&dir.0, 1), move |_| a);
        let mut w = Ws::open_to(port, "slow.localhost", "/slow").unwrap();
        w.text("session-busy 600");
        std::thread::sleep(Duration::from_millis(100));
        drop(w);
        std::thread::sleep(Duration::from_millis(1000));
        assert_eq!(http(port, "GET", "/_combd/ping", "").0, 200, "the node is still up");
        assert_eq!(stat(port, "sessions_live"), 0);
        let mut w = Ws::open_to(port, "slow.localhost", "/slow").unwrap();
        w.text("session-busy 1");
        assert!(eventually(2000, || stat(port, "sessions_live") == 1), "and takes new sessions");
    }

    /// Stats count each process's time: an object busy for 300 ms shows it.
    #[test]
    fn stats_count_each_process_time() {
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, cfg(&dir.0, 1));
        assert_eq!(http(port, "POST", "/do/t/busy", "300").0, 200);
        let procs = || serde_json::from_str::<serde_json::Value>(&http(port, "GET", "/_combd/stats", "").1).unwrap()["procs"].clone();
        assert!(eventually(2000, || procs().as_object().is_some_and(|m| !m.is_empty())), "{}", procs());
        let (name, p) = procs().as_object().unwrap().iter().next().map(|(k, v)| (k.clone(), v.clone())).unwrap();
        assert!(p["cpu_us"].as_u64().unwrap() >= 100_000, "{name}: {p} (300 ms busy: most of it, even on a loaded machine)");
        assert!(p["slices"].as_u64().unwrap() >= 2, "{name}: {p}");
    }

    /// An app whose module wants to run compiled, and a node that only
    /// interprets: refused, saying why, not run some other way.
    #[test]
    fn a_compiled_module_never_runs_interpreted() {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::write(app.0.join("combd.toml"), "[app]\nname = \"c\"\n[modules.classic]\nengine = \"compiled\"\n[durable.room]\nmodule = \"classic\"\nroutes = [\"/room/:id/*\"]\n").unwrap();
        std::fs::copy(GUEST, app.0.join("modules/classic.wasm")).unwrap();
        let e = http::load_app(&interp::Interp { max_pages: 16384 }, &app.0).err().expect("refused");
        assert!(e.contains("interpreted only"), "{e}");
    }

    /// Both engines on a node: the same module, compiled for one process and
    /// interpreted for another, each the way it says.
    #[cfg(feature = "aot-lite")]
    #[test]
    fn each_module_runs_the_way_it_says() {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::write(app.0.join("combd.toml"), "[app]\nname = \"two\"\n[modules.fast]\nengine = \"compiled\"\n[modules.slow]\nengine = \"interpreted\"\n[durable.a]\nmodule = \"fast\"\nroutes = [\"/a/:id/*\"]\n[durable.b]\nmodule = \"slow\"\nroutes = [\"/b/:id/*\"]\n").unwrap();
        for m in ["fast", "slow"] {
            std::fs::copy(GUEST, app.0.join(format!("modules/{m}.wasm"))).unwrap();
        }
        let eng = both();
        let (a, _) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(eng, cfg(&dir.0, 1), move |_| a);
        for p in ["/a/x/incr", "/b/x/incr"] {
            assert_eq!(http_to(port, "two.localhost", "POST", p, ""), (200, "{\"count\":1}".into()), "{p}");
        }
        let procs = || serde_json::from_str::<serde_json::Value>(&http(port, "GET", "/_combd/stats", "").1).unwrap()["procs"].clone();
        assert!(eventually(2000, || procs()["local/two/b"]["engine"] == "interpreted"), "{}", procs());
        assert_eq!(procs()["local/two/a"]["engine"], "compiled", "{}", procs());
    }

    /// A module's build, loaded into this process the way a node does
    /// (`combd-load`), answers as the interpreter does; a library that isn't
    /// a build, or a build filed as another module, is refused.
    /// Slow (it compiles), so: `cargo test --features aot -- --ignored`.
    #[cfg(feature = "aot-lite")]
    #[test]
    #[ignore]
    fn a_build_loads_and_answers_as_interpreted() {
        let wasm = std::fs::read(format!("{MILLION}/modules/viewer.wasm")).expect("build it first: examples/million/build.sh");
        let dir = store::tempdir::Dir::new();
        let name = |w: &[u8]| dir.0.join(format!("{}.so", builds::module_id(w)));
        let aot_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("aot");
        let work = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/aot-builds");
        build::build(&wasm, &aot_src, &work, &name(&wasm), None, None).unwrap();
        let m = aot::built(&dir.0, &wasm).unwrap();
        let interp = interp::Interp { max_pages: 16384 };
        let ip = interp.load(&wasm).unwrap();
        let (mut a, mut i) = (aot::Aot.spawn(&m).unwrap(), interp.spawn(&ip).unwrap());
        let (mut sa, mut si) = (HostState::default(), HostState::default());
        init(&mut a, &mut sa).unwrap();
        init(&mut i, &mut si).unwrap();
        for req in ["do WSOPEN /ws 7", "do WSMSG /7 {\"t\":\"range\",\"from\":0,\"to\":500}", "do WSMSG /7 {\"t\":\"set\",\"i\":3,\"v\":true}", "do HIBERNATE / "] {
            assert_eq!(request(&mut a, req, &mut sa).unwrap(), request(&mut i, req, &mut si).unwrap(), "{req}");
        }
        // Not a build: a library without combd's two symbols.
        let lib = ["/usr/lib/libSystem.B.dylib", "/lib/x86_64-linux-gnu/libc.so.6", "/lib/aarch64-linux-gnu/libc.so.6"].into_iter().find(|p| std::path::Path::new(p).exists() || cfg!(target_os = "macos")).unwrap();
        assert!(combd_load::open(std::path::Path::new(lib)).err().unwrap().contains("not a combd build"));
        // Filed as another module: refused.
        let other = b"not the viewer";
        std::fs::copy(name(&wasm), name(other)).unwrap();
        assert!(aot::built(&dir.0, other).err().unwrap().contains("another module"));
        // Built against other combd-aot sources (an old build, after the
        // node moved on): its pin isn't ours, and nothing else is read.
        let old = store::tempdir::Dir::new();
        let mem_src = aot_src.join("../mem");
        for (from, to, f) in ["Cargo.toml", "build.rs", "src/lib.rs", "src/rt.rs", "src/fiber.rs", "src/guard.rs", "src/trap.c"].map(|f| (&aot_src, "aot", f)).into_iter().chain(["Cargo.toml", "src/lib.rs"].map(|f| (&mem_src, "mem", f))) {
            std::fs::create_dir_all(old.0.join(to).join(f).parent().unwrap()).unwrap();
            std::fs::copy(from.join(f), old.0.join(to).join(f)).unwrap();
        }
        let rt = std::fs::read_to_string(old.0.join("aot/src/rt.rs")).unwrap();
        std::fs::write(old.0.join("aot/src/rt.rs"), rt + "\n// another version\n").unwrap();
        let stale = old.0.join("stale.so");
        build::build(&wasm, &old.0.join("aot"), &old.0.join("work"), &stale, None, None).unwrap();
        let e = combd_load::open(&stale).err().unwrap();
        assert!(e.contains("built with") && e.contains("this node is"), "{e}");
    }

    /// A compiled module sees its process's env vars, as an interpreted one
    /// does (it once saw none: million's compiled web said "no process").
    #[cfg(feature = "aot")]
    #[test]
    fn a_compiled_module_sees_its_env() {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::write(app.0.join("combd.toml"), "[app]\nname = \"env\"\n[vars]\nV = \"one\"\n[modules.ev]\nengine = \"compiled\"\n[http.api]\nmodule = \"ev\"\nroutes = [\"/env/*\"]\n").unwrap();
        std::fs::copy(GUEST_EV, app.0.join("modules/ev.wasm")).unwrap();
        let eng = both();
        let (a, _) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(eng, cfg(&dir.0, 1), move |_| a);
        assert_eq!(http_to(port, "env.localhost", "GET", "/env/V", ""), (200, "{\"value\":\"one\"}".into()));
    }

    /// A busy object's mailbox fills: what comes past `mailbox` is refused
    /// at once (503, overloaded), not queued, and the object goes on.
    #[test]
    fn a_full_mailbox_refuses_new_requests() {
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(interp::Interp { max_pages: 16384 }, cfg(&dir.0, 1), |eng| {
            let mut a = app::App::single(eng.load(&wasm()).unwrap(), 4);
            a.procs.iter_mut().for_each(|p| p.mailbox = 2);
            a
        });
        let busy = std::thread::spawn(move || http(port, "POST", "/do/mb/busy", "1500"));
        assert!(eventually(1000, || stat(port, "objects_live") == 1));
        let rest: Vec<_> = (0..4).map(|_| std::thread::spawn(move || http(port, "GET", "/do/mb/count", "").0)).collect();
        let mut got: Vec<u16> = rest.into_iter().map(|t| t.join().unwrap()).collect();
        got.sort();
        assert_eq!(got, [200, 200, 503, 503], "two wait their turn, two are refused");
        assert_eq!(busy.join().unwrap().0, 200);
        assert_eq!(stat(port, "mailbox_refused"), 2);
        assert_eq!(http(port, "GET", "/do/mb/count", "").0, 200, "and it takes requests again");
    }

    /// Requests waiting on a busy object run as one batch (`handle_batch`):
    /// each gets its own answer, and all of them land.
    #[test]
    fn waiting_requests_run_as_a_batch() {
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, cfg(&dir.0, 1));
        assert_eq!(http(port, "POST", "/do/bt/incr", "").0, 200, "warm: a cell to batch on");
        let busy = std::thread::spawn(move || http(port, "POST", "/do/bt/busy", "800"));
        std::thread::sleep(Duration::from_millis(200));
        let incrs: Vec<_> = (0..6).map(|_| std::thread::spawn(move || http(port, "POST", "/do/bt/incr", ""))).collect();
        let mut counts: Vec<i64> = incrs.into_iter().map(|t| t.join().unwrap()).map(|(code, body)| {
            assert_eq!(code, 200, "{body}");
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["count"].as_i64().unwrap()
        }).collect();
        counts.sort();
        assert_eq!(counts, [2, 3, 4, 5, 6, 7], "an answer each, in the order they ran");
        assert_eq!(busy.join().unwrap().0, 200);
        assert_eq!(stat(port, "batched"), 6, "the six waited, then ran together");
        assert_eq!(http(port, "GET", "/do/bt/count", "").1, "{\"count\":7}");
    }

    #[test]
    fn batches_frame_and_unframe() {
        let parts = vec!["do POST /a 1".to_string(), String::new(), "héllo\n2".to_string()];
        assert_eq!(http::unframe(&http::frame(&parts), 3).unwrap(), parts);
        assert!(http::unframe(&http::frame(&parts), 2).unwrap_err().contains("3 answers for 2"));
        assert!(http::unframe("5\nabc", 1).is_err(), "shorter than it says");
        assert!(http::unframe("x\nabc", 1).is_err());
    }

    /// `memory_kb` on the AOT tier too: a cell capped under what it needs
    /// can't grow there, and one capped under where it starts won't start.
    #[cfg(feature = "aot-lite")]
    #[test]
    fn memory_kb_caps_the_aot_tier_too() {
        let prog = aot::Aot.load(&wasm()).unwrap();
        let mut vm = aot::Aot.spawn(&prog).unwrap();
        vm.cap_memory(64).unwrap(); // 4 MiB
        assert!(request(&mut vm, "sieve 5000000", &mut HostState::default()).is_err(), "5 MB of sieve under a 4 MiB cap");
        let mut ok = aot::Aot.spawn(&prog).unwrap();
        assert!(request(&mut ok, "sieve 5000000", &mut HostState::default()).is_ok(), "and fine without one");
        assert!(aot::Aot.spawn(&prog).unwrap().cap_memory(1).is_err(), "under where it starts");
    }

    /// A TLS connection to a test server, trusting `ca` (from `wire::make_certs`).
    fn tls_to(port: u16, ca: &std::path::Path) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
        // As a browser would: offering HTTP/1.1 by ALPN.
        let mut cfg = (*wire::admin_client(ca).unwrap()).clone();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let cfg = Arc::new(cfg);
        let tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        rustls::StreamOwned::new(rustls::ClientConnection::new(cfg, "node0.combd".try_into().unwrap()).unwrap(), tcp)
    }

    /// One answer's head and body (a content-length answer).
    fn read_answer(s: &mut impl std::io::Read) -> (String, String) {
        let (mut head, mut b) = (vec![], [0u8; 1]);
        while !head.ends_with(b"\r\n\r\n") {
            assert_eq!(s.read(&mut b).unwrap(), 1, "closed early: {}", String::from_utf8_lossy(&head));
            head.push(b[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let n: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
        let mut body = vec![0u8; n];
        s.read_exact(&mut body).unwrap();
        (head, String::from_utf8(body).unwrap())
    }

    /// The next frame from the server: (opcode, payload).
    fn read_frame(s: &mut impl std::io::Read) -> (u8, Vec<u8>) {
        let mut h = [0u8; 2];
        s.read_exact(&mut h).unwrap();
        let n = match h[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                s.read_exact(&mut b).unwrap();
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                s.read_exact(&mut b).unwrap();
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        let mut p = vec![0u8; n];
        s.read_exact(&mut p).unwrap();
        (h[0] & 0x0f, p)
    }

    /// HTTPS ended in combd: requests on a kept connection, sockets to
    /// objects on other loops (their TLS goes with them), plain HTTP turned
    /// away, and the redirect port.
    #[test]
    fn https_ended_here() {
        use std::io::{Read, Write};
        let dir = store::tempdir::Dir::new();
        let certs = dir.0.join("certs");
        wire::make_certs(&certs, 1).unwrap();
        let ca = certs.join("ca.pem");
        let redirect = free_ports(1)[0];
        let c = http::Config { tls: Some((certs.join("node0.pem"), certs.join("node0.key"))), redirect_port: Some(redirect), ..cfg(&dir.0.join("d"), 4) };
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, c);
        // Two requests on one connection.
        let mut s = tls_to(port, &ca);
        for _ in 0..2 {
            write!(s, "GET /_combd/ping HTTP/1.1\r\nhost: node0.combd\r\n\r\n").unwrap();
            assert!(read_answer(&mut s).0.starts_with("HTTP/1.1 200"));
        }
        assert_eq!(s.conn.alpn_protocol(), Some(&b"http/1.1"[..]));
        // Plain HTTP on the TLS port gets nothing that reads as HTTP.
        let mut p = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        p.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(p, "GET /_combd/ping HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
        let mut got = vec![];
        let _ = p.read_to_end(&mut got);
        assert!(!got.starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&got));
        // Sockets to eight rooms over TLS: some rooms live on other loops, so
        // their sockets move there, TLS and all. Each hears its own message.
        for r in 0..8 {
            let mut w = tls_to(port, &ca);
            write!(w, "GET /do/room{r}/ws HTTP/1.1\r\nhost: node0.combd\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n").unwrap();
            assert!(read_answer(&mut w).0.starts_with("HTTP/1.1 101"));
            w.write_all(&ws::tests::client(true, 1, format!("hi {r}").as_bytes())).unwrap();
            let (op, p) = read_frame(&mut w);
            assert_eq!((op, String::from_utf8(p).unwrap().contains(&format!("\"text\":\"hi {r}\""))), (1, true));
        }
        let mut st = tls_to(port, &ca);
        write!(st, "GET /_combd/stats HTTP/1.1\r\nhost: node0.combd\r\n\r\n").unwrap();
        let stats: serde_json::Value = serde_json::from_str(&read_answer(&mut st).1).unwrap();
        assert!(stats["ws_handoffs"].as_u64() > Some(0), "some sockets moved loops: {stats}");
        // The redirect port: plain HTTP, sent to HTTPS.
        let mut p = std::net::TcpStream::connect(("127.0.0.1", redirect)).unwrap();
        write!(p, "GET /a?b HTTP/1.1\r\nhost: shop.example.com\r\n\r\n").unwrap();
        let (head, _) = read_answer(&mut p);
        assert!(head.starts_with("HTTP/1.1 301") && head.contains("location: https://shop.example.com/a?b"), "{head}");
    }

    /// A connection that sends nothing, one that sends half a request (408),
    /// a kept one with nothing to do, and a TLS handshake that stalls are all
    /// closed in their time; a WebSocket isn't.
    #[test]
    fn slow_and_idle_connections_are_closed() {
        use std::io::{Read, Write};
        let lim = http::Limits { read_timeout: Duration::from_millis(400), keepalive: Duration::from_millis(600), tls_handshake: Duration::from_millis(400), ..http::Limits::default() };
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, http::Config { limits: lim.clone(), ..cfg(&dir.0.join("a"), 1) });
        let open = |port: u16| {
            let s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s
        };
        // Closed with what it's told (if anything), and how long it took.
        let rest = |s: &mut std::net::TcpStream| {
            let t = Instant::now();
            let mut got = vec![];
            let _ = s.read_to_end(&mut got);
            (String::from_utf8_lossy(&got).into_owned(), t.elapsed())
        };
        let (got, took) = rest(&mut open(port));
        assert!(got.is_empty() && took < Duration::from_secs(2), "nothing sent: closed in {took:?}");
        let mut s = open(port);
        write!(s, "GET /_combd/ping HTTP/1.1\r\nhost: x\r\n").unwrap();
        let (got, took) = rest(&mut s);
        assert!(got.starts_with("HTTP/1.1 408") && took < Duration::from_secs(2), "half a request: {got:?} in {took:?}");
        let mut s = open(port);
        write!(s, "GET /_combd/ping HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
        assert!(read_answer(&mut s).0.starts_with("HTTP/1.1 200"));
        let (got, took) = rest(&mut s);
        assert!(got.is_empty() && took >= Duration::from_millis(500) && took < Duration::from_secs(2), "kept, then idle: closed in {took:?}");
        // A WebSocket stays, past every one of them.
        let mut w = Ws::open(port, "/do/room/ws").unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        w.text("still here");
        assert!(String::from_utf8(w.recv().1).unwrap().contains("still here"));
        // A TLS handshake that never finishes.
        let certs = dir.0.join("certs");
        wire::make_certs(&certs, 1).unwrap();
        let (tport, _, _) = do_server(interp::Interp { max_pages: 16384 }, http::Config { limits: lim, tls: Some((certs.join("node0.pem"), certs.join("node0.key"))), ..cfg(&dir.0.join("b"), 1) });
        let mut s = open(tport);
        s.write_all(&[0x16, 0x03, 0x01]).unwrap(); // the start of a ClientHello, no more
        let (_, took) = rest(&mut s);
        assert!(took < Duration::from_secs(2), "a stalled handshake: closed in {took:?}");
    }

    /// A request refused while its client is still sending (a body over
    /// max_body) gets its answer: the connection lingers (stops sending,
    /// reads and drops the rest a while) before it closes, so the client's
    /// kernel isn't sent a reset that would throw the answer away.
    #[test]
    fn a_refused_client_still_sending_gets_its_answer() {
        use std::io::{Read, Write};
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, cfg(&dir.0, 1));
        for _ in 0..5 {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut w = s.try_clone().unwrap();
            let sender = std::thread::spawn(move || {
                let _ = write!(w, "POST /do/a/x HTTP/1.1\r\nhost: x\r\ncontent-length: 8000000\r\n\r\n");
                let chunk = vec![b'x'; 64 << 10];
                for _ in 0..120 {
                    if w.write_all(&chunk).is_err() {
                        break;
                    }
                }
            });
            let mut got = vec![];
            let r = s.read_to_end(&mut got);
            sender.join().unwrap();
            assert!(got.starts_with(b"HTTP/1.1 413"), "{r:?}: {:?}", String::from_utf8_lossy(&got[..got.len().min(60)]));
        }
    }

    /// A new certificate on disk reaches new connections within seconds;
    /// one that won't load leaves the old one.
    #[test]
    fn a_new_certificate_is_picked_up() {
        use std::io::Write;
        let dir = store::tempdir::Dir::new();
        let (old, new) = (dir.0.join("old"), dir.0.join("new"));
        wire::make_certs(&old, 1).unwrap();
        wire::make_certs(&new, 1).unwrap();
        let live = dir.0.join("live");
        std::fs::create_dir(&live).unwrap();
        for f in ["node0.pem", "node0.key"] {
            std::fs::copy(old.join(f), live.join(f)).unwrap();
        }
        let c = http::Config { tls: Some((live.join("node0.pem"), live.join("node0.key"))), ..cfg(&dir.0.join("d"), 1) };
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, c);
        let ping = |ca: &std::path::Path| {
            let mut s = tls_to(port, ca);
            write!(s, "GET /_combd/ping HTTP/1.1\r\nhost: node0.combd\r\nconnection: close\r\n\r\n").ok()?;
            Some(read_answer(&mut s).0)
        };
        let ok = |ca: &std::path::Path| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ping(ca))).ok().flatten().is_some_and(|h| h.starts_with("HTTP/1.1 200"));
        assert!(ok(&old.join("ca.pem")) && !ok(&new.join("ca.pem")));
        // A broken one first: the old stays.
        std::fs::write(live.join("node0.pem"), "not a certificate").unwrap();
        std::thread::sleep(Duration::from_secs(6));
        assert!(ok(&old.join("ca.pem")), "a bad file keeps the old certificate");
        std::fs::copy(new.join("node0.key"), live.join("node0.key")).unwrap();
        std::fs::copy(new.join("node0.pem"), live.join("node0.pem")).unwrap();
        assert!(eventually(10_000, || ok(&new.join("ca.pem"))), "the new one, for new connections");
        assert!(!ok(&old.join("ca.pem")));
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
        server_for(eng, cfg, wasm())
    }

    fn server_for<E: Engine + Sync + Send + 'static>(eng: E, cfg: http::Config, wasm: Vec<u8>) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>)
    where
        E::Program: Sync + Send,
    {
        let ev_max = cfg.ev_max;
        server_app(eng, cfg, move |eng| app::App::single(eng.load(&wasm).unwrap(), ev_max))
    }

    fn server_app<E: Engine + Sync + Send + 'static>(eng: E, cfg: http::Config, make: impl FnOnce(&E) -> app::App<E::Program> + Send + 'static) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>)
    where
        E::Program: Sync + Send,
    {
        let port = free_ports(1)[0];
        let stop = cfg.stop.clone();
        let t = std::thread::spawn(move || {
            let app = make(&eng);
            http::serve_with(&eng, app, port, cfg);
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return (port, stop, t);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("server on {port} didn't start");
    }

    /// A bucket no other test uses. (They used to share one: the name took
    /// `Instant::now().elapsed()`, which is always about 0, so every fleet
    /// test in a run met the others' nodes 0, 1, and 2 in the same bucket.)
    fn test_bucket() -> String {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!("combd-test-{}-{}-{}", std::process::id(), host::now_ms(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    /// `n` free ports from the OS, never handed out twice in this process.
    /// (Ports worked out from the process id used to collide: one fleet
    /// test's wire landed on another's.)
    /// Ports for test servers, below the range the OS hands out for
    /// outgoing connections (32768 and up on Linux, 49152 on macOS): a port
    /// picked there could be taken, between picking and listening, as some
    /// test client's local port. Each test binary starts at its own place.
    pub(crate) fn free_ports(n: usize) -> Vec<u16> {
        static NEXT: std::sync::Mutex<u16> = std::sync::Mutex::new(0);
        let mut next = NEXT.lock().unwrap();
        if *next == 0 {
            *next = 20_000 + (std::process::id() % 1000) as u16 * 12;
        }
        let mut out = vec![];
        while out.len() < n {
            let p = *next;
            *next = if *next >= 32_000 { 20_000 } else { *next + 1 };
            if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
                out.push(p);
            }
        }
        out
    }

    fn cfg(data: &std::path::Path, threads: usize) -> http::Config {
        http::Config {
            threads,
            data: data.to_path_buf(),
            idle: Duration::from_secs(60),
            max_objects: 10_000,
            max_awake: 1000,
            stop: Arc::default(),
            sync_delay: Duration::ZERO,
            fleet: None,
            pg: None,
            ev_max: 32,
            environ: Arc::default(),
            public: Default::default(),
            catalog: None,
            master_key: None,
            memory_budget: None,
            max_conns: u64::MAX,
            tls: None,
            redirect_port: None,
            limits: http::Limits::default(),
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
        http_to(port, "localhost", method, path, body)
    }

    fn http_to(port: u16, host: &str, method: &str, path: &str, body: &str) -> (u16, String) {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        // Never forever: a server that doesn't answer fails the test, named.
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        write!(s, "{method} {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut out = String::new();
        let r = s.read_to_string(&mut out);
        assert!(r.is_ok() && out.len() >= 12, "{method} {path} on {port}: no answer ({r:?}, {out:?})");
        let status = out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_string())
    }

    /// What's on disk for an object, read with a separate connection.
    fn on_disk(data: &std::path::Path, name: &str, sql: &str) -> String {
        let db = store::Db::open(&store::path_for(data, name)).unwrap();
        String::from_utf8(db.exec_json(format!("{{\"q\":{sql:?}}}").as_bytes())).unwrap()
    }

    #[test]
    fn uuid_v7s_are_well_formed_and_sort_by_time() {
        let a = http::uuid_v7();
        std::thread::sleep(Duration::from_millis(2));
        let b = http::uuid_v7();
        for u in [&a, &b] {
            assert_eq!(u.len(), 36);
            assert_eq!(&u[14..15], "7", "version 7: {u}");
            assert!("89ab".contains(&u[19..20]), "variant 10: {u}");
        }
        assert!(a < b, "{a} should sort before {b}");
        assert_ne!(a, http::uuid_v7());
    }

    /// Fleet mode on a real bucket (set COMBD_S3, COMBD_S3_KEY, COMBD_S3_SECRET):
    /// two nodes with mTLS. Alarms fire; an alarm set on a node that then
    /// stops fires on the other; an evicted object comes back from the bucket.
    #[test]
    fn fleet_alarms_and_eviction() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 2).unwrap();
        let bucket = test_bucket();
        let wports = free_ports(2);
        let peers: HashMap<own::Node, String> = (0..2).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        let node = |n: own::Node, idle_ms: u64| {
            let fc = fleet::FleetCfg {
                node: n,
                lease_ms: 1000,
                replicas: 0,
                s3: s3::S3 { bucket: bucket.clone(), ..base.clone() },
                peers: peers.clone(),
                wire_port: wports[n as usize],
                wire_certs: Some(dir.0.join("certs")),
                insecure: false,
                upload_ms: 0,
                upload_bytes: 1 << 20,
                read_lease: true,
                regions: HashMap::new(), place_ms: 0,
            };
            let c = http::Config { idle: Duration::from_millis(idle_ms), fleet: Some(fc), ..cfg(&dir.0.join(format!("d{n}")), 1) };
            do_server(interp::Interp { max_pages: 16384 }, c)
        };
        let (a, stop_a, ta) = node(0, 60_000);
        let (b, _, _) = node(1, 300);
        // Ask through `port` until it answers 200.
        let ok = |port: u16, method: &str, path: &str, body: &str| -> String {
            for _ in 0..100 {
                let (c, r) = http(port, method, path, body);
                if c == 200 {
                    return r;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("{method} {path} never answered 200");
        };
        // 1. An alarm fires on its owner.
        ok(a, "POST", "/do/al1/alarm", "300");
        assert!(eventually(5000, || ok(b, "GET", "/do/al1/alarms", "").contains("\"fired\":1")), "{}", ok(b, "GET", "/do/al1/alarms", ""));
        // 2. Node A sets an alarm, then stops: node B finds it in the index,
        //    takes the object over from the bucket, and fires it.
        ok(a, "POST", "/do/al2/alarm", "1500");
        stop_a.store(true, std::sync::atomic::Ordering::Relaxed);
        ta.join().unwrap();
        assert!(eventually(10_000, || http(b, "GET", "/do/al2/alarms", "").1.contains("\"fired\":1")), "{:?}", http(b, "GET", "/do/al2/alarms", ""));
        // 3. Eviction: B lets an idle object go; it comes back from the bucket.
        ok(b, "POST", "/do/ev/incr", "");
        ok(b, "POST", "/do/ev/incr", "");
        assert!(eventually(3000, || stat(b, "evictions") >= 1), "nothing was evicted");
        assert_eq!(ok(b, "GET", "/do/ev/count", ""), "{\"count\":2}");
    }

    /// Compaction on a real node: an object with 20 uploads, let go and back
    /// (an image, mid-epoch or at the restore, and the deletes); back again,
    /// it starts from the image, with every write.
    #[test]
    fn fleet_compaction() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 1).unwrap();
        let s3 = s3::S3 { bucket: test_bucket(), ..base };
        let wport = free_ports(1)[0];
        let fc = fleet::FleetCfg {
            node: 0,
            lease_ms: 1000,
            replicas: 0,
            s3: s3.clone(),
            peers: HashMap::from([(0, format!("127.0.0.1:{wport}"))]),
            wire_port: wport,
            wire_certs: Some(dir.0.join("certs")),
            insecure: false,
            upload_ms: 0,
            upload_bytes: 1 << 20,
            read_lease: true,
                regions: HashMap::new(), place_ms: 0,
        };
        let c = http::Config { idle: Duration::from_millis(300), fleet: Some(fc), ..cfg(&dir.0.join("d0"), 1) };
        let (p, _, _) = do_server(interp::Interp { max_pages: 16384 }, c);
        let ok = |method: &str, path: &str| -> String {
            for _ in 0..100 {
                let (c, r) = http(p, method, path, "");
                if c == 200 {
                    return r;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("{method} {path} never answered 200");
        };
        // One upload per write (no batching, no peers).
        for _ in 0..20 {
            ok("POST", "/do/cmp/incr");
        }
        let keys = |part: &str| s3.list("obj/").unwrap().into_iter().filter(|k| k.rsplit('/').next().is_some_and(|p| p.starts_with(part))).count();
        // Let go, then back.
        assert!(eventually(5000, || stat(p, "evictions") >= 1));
        assert_eq!(ok("GET", "/do/cmp/count"), "{\"count\":20}");
        assert!(eventually(5000, || keys("i") >= 1 && stat(p, "gc_deleted") >= 16), "images {}, deleted {}", keys("i"), stat(p, "gc_deleted"));
        assert!(keys("s") < 5, "{} segments left", keys("s"));
        // Back once more: from the image, with every write.
        ok("POST", "/do/cmp/incr");
        assert!(eventually(5000, || stat(p, "evictions") >= 2));
        assert_eq!(ok("GET", "/do/cmp/count"), "{\"count\":21}");
    }

    /// Compaction while an object stays up: every so many uploads, an image
    /// as of an entry, and the segments it holds go, with no restore.
    #[test]
    fn fleet_compaction_while_up() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 1).unwrap();
        let s3 = s3::S3 { bucket: test_bucket(), ..base };
        let wport = free_ports(1)[0];
        let fc = fleet::FleetCfg {
            node: 0,
            lease_ms: 1000,
            replicas: 0,
            s3: s3.clone(),
            peers: HashMap::from([(0, format!("127.0.0.1:{wport}"))]),
            wire_port: wport,
            wire_certs: Some(dir.0.join("certs")),
            insecure: false,
            upload_ms: 0,
            upload_bytes: 1 << 20,
            read_lease: true,
                regions: HashMap::new(), place_ms: 0,
        };
        let c = http::Config { idle: Duration::from_millis(1500), fleet: Some(fc), ..cfg(&dir.0.join("d0"), 1) };
        let (p, _, _) = do_server(interp::Interp { max_pages: 16384 }, c);
        let ok = |method: &str, path: &str| -> String {
            for _ in 0..100 {
                let (c, r) = http(p, method, path, "");
                if c == 200 {
                    return r;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("{method} {path} never answered 200");
        };
        let keys = |part: &str| s3.list("obj/").unwrap().into_iter().filter(|k| k.rsplit('/').next().is_some_and(|p| p.starts_with(part))).count();
        // One upload per write, the object up the whole time.
        for _ in 0..40 {
            ok("POST", "/do/up/incr");
        }
        assert!(eventually(5000, || stat(p, "gc_deleted") >= 16), "deleted {}, images {}, segments {}", stat(p, "gc_deleted"), keys("i"), keys("s"));
        assert_eq!(stat(p, "evictions"), 0, "it was let go: not the case under test");
        assert!(keys("i") >= 1 && keys("s") < 30, "images {}, segments {}", keys("i"), keys("s"));
        // Let go and back: from the image and what came after, every write.
        assert!(eventually(5000, || stat(p, "evictions") >= 1));
        assert_eq!(ok("GET", "/do/up/count"), "{\"count\":40}");
    }

    /// D3 on real nodes: three nodes, two peers per object. Writes are
    /// acknowledged by the peers; when the owner stops, another node fences
    /// the peers, recovers, and has every write.
    #[test]
    fn fleet_peer_writes() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 3).unwrap();
        let bucket = test_bucket();
        let wports = free_ports(3);
        let peers: HashMap<own::Node, String> = (0..3).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        let node = |n: own::Node| {
            let fc = fleet::FleetCfg {
                node: n,
                lease_ms: 1000,
                replicas: 2,
                s3: s3::S3 { bucket: bucket.clone(), ..base.clone() },
                peers: peers.clone(),
                wire_port: wports[n as usize],
                wire_certs: Some(dir.0.join("certs")),
                insecure: false,
                upload_ms: 60_000,
                upload_bytes: 1 << 20,
                read_lease: true,
                regions: HashMap::new(), place_ms: 0,
            };
            let c = http::Config { fleet: Some(fc), ..cfg(&dir.0.join(format!("d{n}")), 2) };
            do_server(interp::Interp { max_pages: 16384 }, c)
        };
        let (a, stop_a, ta) = node(0);
        let (b, _, _) = node(1);
        let (c, _, _) = node(2);
        // The HTTP port opens before the storage test and the wire: wait
        // until every node really serves.
        for p in [a, b, c] {
            stat(p, "fences");
        }
        let ok = |port: u16, method: &str, path: &str| -> String {
            for _ in 0..100 {
                let (c, r) = http(port, method, path, "");
                if c == 200 {
                    return r;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("{method} {path} never answered 200: {:?} stats {}", http(port, method, path, ""), http(port, "GET", "/_combd/stats", "").1);
        };
        for i in 1..=30 {
            assert_eq!(ok(a, "POST", "/do/p1/incr"), format!("{{\"count\":{i}}}"));
        }
        assert!(stat(a, "peer_acks") >= 25, "writes should be acknowledged by peers: {}", http(a, "GET", "/_combd/stats", "").1);
        // Uploads wait (60 s here) while every peer is up: the bucket has
        // none of these writes yet, only the peers do.
        eprintln!("uploads for 30 writes: {}", stat(a, "uploads"));
        assert!(stat(a, "uploads") <= 2, "{}", http(a, "GET", "/_combd/stats", "").1);
        // The owner stops (its peer store keeps running: it's the same
        // process). Another node takes over after the lease and has it all,
        // from the peers.
        stop_a.store(true, std::sync::atomic::Ordering::Relaxed);
        ta.join().unwrap();
        assert_eq!(ok(b, "GET", "/do/p1/count"), "{\"count\":30}");
        assert!(stat(b, "fences") >= 1, "the takeover should fence: {}", http(b, "GET", "/_combd/stats", "").1);
        assert!(stat(b, "tail_entries") >= 25, "the writes came from the peers: {}", http(b, "GET", "/_combd/stats", "").1);
        assert_eq!(ok(b, "POST", "/do/p1/incr"), "{\"count\":31}");
    }

    /// `n` fleet nodes on one bucket, each serving `make`'s app: start one
    /// with the closure returned, by number.
    fn fleet_of(dir: &std::path::Path, base: s3::S3, n: u32, make: impl Fn(&interp::Interp) -> app::App<<interp::Interp as Engine>::Program> + Clone + Send + 'static) -> impl Fn(own::Node) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        fleet_placing(dir, base, n, 0, make)
    }

    /// `fleet_of`, with placement looking every `place_ms` (0: never).
    fn fleet_placing(dir: &std::path::Path, base: s3::S3, n: u32, place_ms: u64, make: impl Fn(&interp::Interp) -> app::App<<interp::Interp as Engine>::Program> + Clone + Send + 'static) -> impl Fn(own::Node) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        wire::make_certs(&dir.join("certs"), n).unwrap();
        let bucket = test_bucket();
        let wports = free_ports(n as usize);
        let peers: HashMap<own::Node, String> = (0..n).map(|k| (k, format!("127.0.0.1:{}", wports[k as usize]))).collect();
        std::fs::write(dir.join("peers.json"), serde_json::to_vec(&peers).unwrap()).unwrap();
        let dir = dir.to_path_buf();
        move |k: own::Node| {
            let fc = fleet::FleetCfg {
                node: k,
                lease_ms: 1000,
                replicas: 2,
                s3: s3::S3 { bucket: bucket.clone(), ..base.clone() },
                peers: peers.clone(),
                wire_port: wports[k as usize],
                wire_certs: Some(dir.join("certs")),
                insecure: false,
                upload_ms: 60_000,
                upload_bytes: 1 << 20,
                read_lease: true,
                regions: HashMap::new(), place_ms,
            };
            let make = make.clone();
            server_app(interp::Interp { max_pages: 16384 }, http::Config { fleet: Some(fc), ..cfg(&dir.join(format!("d{k}")), 1) }, move |eng| make(eng))
        }
    }

    /// The wire addresses `fleet_placing` gave its nodes.
    fn fleet_peers_of(dir: &std::path::Path) -> HashMap<own::Node, String> {
        serde_json::from_slice(&std::fs::read(dir.join("peers.json")).unwrap()).unwrap()
    }

    /// Placement (`nearest`, the default): an object whose requests come in
    /// through another node moves there after two windows, keeping every
    /// write, and answers from there.
    #[test]
    fn objects_move_to_where_their_requests_come_in() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let node = fleet_placing(&dir.0, base, 3, 500, |eng| app::App::single(eng.load(&wasm()).unwrap(), 4));
        let ports: Vec<u16> = (0..3).map(|k| node(k).0).collect();
        for &p in &ports {
            stat(p, "fences");
        }
        assert!(eventually(10_000, || http(ports[0], "POST", "/do/mv/incr", "").0 == 200), "started on node 0");
        assert_eq!((stat(ports[0], "objects_live"), stat(ports[1], "objects_live")), (1, 0));
        // From now on, everything comes in through node 1.
        let (mut sent, mut ok) = (1, 1);
        let until = Instant::now() + Duration::from_secs(20);
        while stat(ports[1], "objects_live") == 0 && Instant::now() < until {
            sent += 1;
            ok += (http(ports[1], "POST", "/do/mv/incr", "").0 == 200) as i64;
        }
        assert_eq!((stat(ports[0], "objects_live"), stat(ports[1], "objects_live")), (0, 1), "moved to node 1");
        assert_eq!(stat(ports[0], "moves"), 1);
        // Its file left node 0 with it (the bucket has it all): a node
        // doesn't keep a copy of everything it ever served.
        let file = |k: u32| store::path_for(&dir.0.join(format!("d{k}")), "mv");
        assert!(file(1).exists() && !file(0).exists(), "on node 1 only");
        for _ in 0..5 {
            sent += 1;
            ok += (http(ports[1], "POST", "/do/mv/incr", "").0 == 200) as i64;
        }
        // Every write it answered is there (one it failed may have landed too).
        let count = |p| serde_json::from_str::<serde_json::Value>(&http(p, "GET", "/do/mv/count", "").1).unwrap()["count"].as_i64().unwrap();
        let n = count(ports[1]);
        assert!(n >= ok && n <= sent, "{n} counted, {ok} answered, {sent} sent");
        assert_eq!(count(ports[2]), n, "and found from anywhere");
        assert_eq!(stat(ports[1], "objects_live"), 1, "it stays");
    }

    /// `placement = "spread"`: objects that started while their home was
    /// down go home once it's up, with every write.
    #[test]
    fn spread_objects_go_home_once_it_is_up() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let node = fleet_placing(&dir.0, base, 3, 500, |eng| {
            let mut a = app::App::single(eng.load(&wasm()).unwrap(), 4);
            a.procs.iter_mut().for_each(|p| p.spread = Some(fleet::Scope::Region));
            a
        });
        // Node 2 isn't up: once the others have gone a lease without hearing
        // from it, every object starts on 0 or 1.
        let ports: Vec<u16> = (0..2).map(|k| node(k).0).collect();
        std::thread::sleep(Duration::from_millis(1500));
        for i in 0..12 {
            assert!(eventually(10_000, || http(ports[0], "POST", &format!("/do/home{i}/incr"), "") == (200, "{\"count\":1}".into())), "home{i}");
        }
        assert_eq!(stat(ports[0], "objects_live") + stat(ports[1], "objects_live"), 12);
        let two = node(2).0;
        assert!(eventually(15_000, || stat(two, "objects_live") >= 1), "some go home to node 2");
        assert!(stat(ports[0], "moves") + stat(ports[1], "moves") >= 1);
        for i in 0..12 {
            assert_eq!(http(two, "GET", &format!("/do/home{i}/count"), ""), (200, "{\"count\":1}".into()), "home{i}, with its write");
        }
    }

    /// A spread object that isn't on its home (it was down when the object
    /// started) answers through every node: entry, home, owner is three hops
    /// (it was a 503 at the home, with one, until placement moved it home).
    #[test]
    fn spread_objects_off_home_answer_from_every_node() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        // Placement looks only once an hour: the objects stay where they started.
        let node = fleet_placing(&dir.0, base, 3, 3_600_000, |eng| {
            let mut a = app::App::single(eng.load(&wasm()).unwrap(), 4);
            a.procs.iter_mut().for_each(|p| p.spread = Some(fleet::Scope::Region));
            a
        });
        let ports: Vec<u16> = (0..2).map(|k| node(k).0).collect();
        std::thread::sleep(Duration::from_millis(1500));
        for i in 0..12 {
            assert!(eventually(10_000, || http(ports[0], "POST", &format!("/do/home{i}/incr"), "") == (200, "{\"count\":1}".into())), "home{i}");
        }
        let two = node(2).0;
        std::thread::sleep(Duration::from_millis(1500)); // every node has heard node 2
        let mut bad = vec![];
        for i in 0..12 {
            for &p in &ports {
                let (status, body) = http(p, "GET", &format!("/do/home{i}/count"), "");
                if status != 200 {
                    bad.push(format!("home{i} via {p}: {status} {body}"));
                }
            }
        }
        assert_eq!(stat(two, "objects_live"), 0, "nothing moved");
        assert!(bad.is_empty(), "spread objects off their home don't answer through every node:\n{}", bad.join("\n"));
    }

    /// `_combd/claim` is only for what the node said yes to (`take`): a
    /// peer with the fleet's certificate can't make it claim whatever it
    /// names (any tenant's object, or names that never existed, each a
    /// record and manifest in the bucket, a database, memory).
    #[test]
    fn a_peer_cannot_make_a_node_claim_what_it_likes() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let node = fleet_placing(&dir.0, base, 3, 0, |eng| app::App::single(eng.load(&wasm()).unwrap(), 4));
        let ports: Vec<u16> = (0..2).map(|k| node(k).0).collect();
        assert_eq!(stat(ports[0], "objects_live"), 0);
        // "Node 2" is a rogue with the fleet's certificate: it never serves,
        // it only talks.
        struct Quiet;
        impl wire::Sink for Quiet {
            fn forward(&self, _: usize, _: u64, _: String, _: String, _: Option<own::Node>) {}
            fn reply(&self, _: usize, _: u64, _: (u16, String)) {}
            fn peer(&self, _: usize, _: u64, _: own::PeerCall) {}
            fn peer_reply(&self, _: usize, _: u64, _: Option<own::PeerRes>) {}
            fn ws_push(&self, _: String, _: u64, _: u8, _: Vec<u8>) {}
            fn ws_lost(&self, _: String, _: u64) {}
            fn catalog(&self, _: u64, _: Vec<u8>) {}
            fn heartbeat(&self, _: own::Node, _: u64) {}
            fn pool(&self, _: Vec<u8>) {}
        }
        let certs = dir.0.join("certs");
        let rogue = wire::start(0, fleet_peers_of(&dir.0), Some(wire::load_tls(&certs, 2).unwrap()), Quiet).unwrap();
        for i in 0..40 {
            rogue.send(0, fleet::Frame::Forward { id: i, name: "_combd/claim".into(), req: format!("nobody/asked/for/this{i}") }, 0);
        }
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(stat(ports[0], "objects_live"), 0, "claimed what a peer merely named");
    }

    /// `placement = "spread"`: objects asked for on one node start across
    /// the fleet, each where its name says, and answer from there.
    #[test]
    fn spread_objects_start_across_the_fleet() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let node = fleet_of(&dir.0, base, 3, |eng| {
            let mut a = app::App::single(eng.load(&wasm()).unwrap(), 4);
            a.procs.iter_mut().for_each(|p| p.spread = Some(fleet::Scope::Region));
            a
        });
        let ports: Vec<u16> = (0..3).map(|k| node(k).0).collect();
        for &p in &ports {
            stat(p, "fences");
        }
        for i in 0..12 {
            assert!(eventually(10_000, || http(ports[0], "POST", &format!("/do/sp{i}/incr"), "") == (200, "{\"count\":1}".into())), "sp{i}");
        }
        let live: Vec<i64> = ports.iter().map(|&p| stat(p, "objects_live")).collect();
        assert_eq!(live.iter().sum::<i64>(), 12, "{live:?}");
        assert!(live.iter().all(|&n| n >= 1), "all asked on node 0, started across the fleet: {live:?}");
        assert!(stat(ports[0], "forwarded") >= 1);
        for i in 0..12 {
            assert_eq!(http(ports[1], "GET", &format!("/do/sp{i}/count"), ""), (200, "{\"count\":1}".into()), "and found from anywhere");
        }
    }

    /// Fleet mode: a batch is one transaction the core proves once, and
    /// each request's answer waits for that. The owner stops right after:
    /// the next one has every write.
    #[test]
    fn fleet_batches_are_proven_before_they_answer() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let node = fleet_of(&dir.0, base, 3, |eng| app::App::single(eng.load(&wasm()).unwrap(), 4));
        let (a, stop_a, ta) = node(0);
        let (b, _, _) = node(1);
        let (_c, _, _) = node(2);
        for p in [a, b, _c] {
            stat(p, "fences");
        }
        assert!(eventually(10_000, || http(a, "POST", "/do/fb/incr", "").0 == 200), "a first write");
        let busy = std::thread::spawn(move || http(a, "POST", "/do/fb/busy", "800"));
        std::thread::sleep(Duration::from_millis(200));
        let incrs: Vec<_> = (0..6).map(|_| std::thread::spawn(move || http(a, "POST", "/do/fb/incr", ""))).collect();
        let mut counts: Vec<i64> = incrs.into_iter().map(|t| t.join().unwrap()).map(|(code, body)| {
            assert_eq!(code, 200, "{body}");
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["count"].as_i64().unwrap()
        }).collect();
        counts.sort();
        assert_eq!(counts, [2, 3, 4, 5, 6, 7]);
        assert_eq!(busy.join().unwrap().0, 200);
        assert_eq!(stat(a, "batched"), 6, "{}", http(a, "GET", "/_combd/stats", "").1);
        stop_a.store(true, std::sync::atomic::Ordering::Relaxed);
        ta.join().unwrap();
        assert!(eventually(15_000, || http(b, "GET", "/do/fb/count", "") == (200, "{\"count\":7}".into())), "{:?}", http(b, "GET", "/do/fb/count", ""));
    }

    /// Compile on deploy, end to end: an app with a compiled module is
    /// deployed to a fleet, waits (building) while a builder builds it for
    /// the fleet's kind of node, and goes live by itself, compiled on every
    /// node. Slow (a real build): `cargo test --features aot -- --ignored`.
    #[cfg(feature = "aot-lite")]
    #[test]
    #[ignore]
    fn a_compiled_deploy_waits_for_its_build_then_goes_live() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 3).unwrap();
        let bucket = s3::S3 { bucket: test_bucket(), ..base.clone() };
        bucket.make_bucket().unwrap();
        let wports = free_ports(3);
        let peers: HashMap<own::Node, String> = (0..3).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        let (token, line) = admin::new_token(&["*/*:*"]);
        let admin_port = free_ports(1)[0];
        let certs = dir.0.join("certs");
        let master = dir.0.join("master.key");
        secrets::new_master(&master).unwrap();
        let public = builds::new_key(&dir.0.join("builder.key")).unwrap();
        let keys = builds::parse_keys(&public);
        let ports = free_ports(3);
        // The guest, with a custom section: the same program, another hash,
        // so it isn't the copy built into this binary.
        let mut wasm = std::fs::read(GUEST).unwrap();
        wasm.extend_from_slice(&[0, 6, 4, b't', b'e', b's', b't', 1]);
        // A build left from before an update (another pin), where builds once
        // went: never taken for this pin's.
        std::fs::create_dir_all(dir.0.join("builds0")).unwrap();
        std::fs::write(dir.0.join("builds0").join(format!("{}.so", builds::module_id(&wasm))), b"an old pin's build").unwrap();
        // Node n on `port`, its builds in `builds`: its stop, and its thread.
        let start = |n: u32, port: u16, builds: &str| {
            let fc = fleet::FleetCfg { node: n, lease_ms: 1000, replicas: 1, s3: bucket.clone(), peers: peers.clone(), wire_port: wports[n as usize], wire_certs: Some(certs.clone()), insecure: false, upload_ms: 0, upload_bytes: 1 << 20, read_lease: true, regions: HashMap::new(), place_ms: 0 };
            let b = builds::Builds::new(bucket.clone(), dir.0.join(builds), keys.clone(), n).unwrap();
            let eng = both::Both { builds: Some(Arc::new(b)), ..both() };
            let c = http::Config { fleet: Some(fc), master_key: Some(master.clone()), ..cfg(&dir.0.join(format!("d{n}")), 1) };
            let admin = (n == 0).then(|| admin::AdminCfg { port: admin_port, public: false, tls: None, tokens: admin::parse_tokens(&line).unwrap() });
            let stop = c.stop.clone();
            (stop, std::thread::spawn(move || http::serve_fleet_apps(&eng, port, c, admin).unwrap()))
        };
        start(0, ports[0], "builds0");
        start(1, ports[1], "builds1");
        let url = format!("http://127.0.0.1:{admin_port}");
        assert!(eventually(20_000, || std::net::TcpStream::connect(("127.0.0.1", admin_port)).is_ok()));
        let api = |m: &str, p: &str, b: &[u8]| admin::request(&url, None, &token, m, p, b).unwrap();
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::write(app.0.join("modules/classic.wasm"), &wasm).unwrap();
        std::fs::write(app.0.join("combd.toml"), "[app]\nname = \"fast\"\ndomains = [\"fast.test\"]\n[modules.classic]\nengine = \"compiled\"\n[durable.room]\nmodule = \"classic\"\nroutes = [\"/room/:id/*\"]\n").unwrap();
        let (code, v) = api("PUT", "/v1/apps/acme/fast", &deploy::bundle(&app.0).unwrap());
        assert_eq!((code, v["building"].as_bool()), (200, Some(true)), "{v}");
        assert_eq!(http_to(ports[0], "fast.test", "POST", "/room/r/incr", "").0, 404, "not live while it builds");
        let (_, d) = api("GET", "/v1/deploys", b"");
        assert_eq!(d["pending"][0]["app"], "acme/fast", "{d}");
        // A builder comes along.
        let stop = Arc::new(AtomicBool::new(false));
        let builder = |stop: &Arc<AtomicBool>| {
            let (b, key, s) = (bucket.clone(), builds::load_key(&dir.0.join("builder.key")).unwrap(), stop.clone());
            let aot_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("aot");
            let work = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/aot-builds");
            std::thread::spawn(move || builds::builder(&b, "test-builder", &key, &aot_src, &work, &s))
        };
        let first_builder = builder(&stop);
        assert!(eventually(300_000, || api("GET", "/v1/apps/acme/fast", b"").1["version"] == v["version"]), "live once built: {}", api("GET", "/v1/deploys", b"").1);
        for &p in &ports[..2] {
            assert!(eventually(20_000, || http_to(p, "fast.test", "POST", "/room/r/incr", "").0 == 200), "node {p}");
        }
        // The object runs on its owner (the other node forwards): compiled there, and nowhere interpreted.
        let engines = || ports[..2].iter().map(|&p| serde_json::from_str::<serde_json::Value>(&http(p, "GET", "/_combd/stats", "").1).unwrap()["procs"]["acme/fast/room"]["engine"].clone()).collect::<Vec<_>>();
        assert!(eventually(3000, || engines().contains(&serde_json::json!("compiled"))), "{:?}", engines());
        assert!(!engines().contains(&serde_json::json!("interpreted")), "{:?}", engines());
        assert_eq!(api("GET", "/v1/deploys", b"").1["pending"].as_array().map(Vec::len), Some(0));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        first_builder.join().unwrap();
        // A node that starts where there's no build for it (as after an
        // update to a new combd: a new pin), with nothing new in the catalog:
        // it asks, and goes live once the build is there, by itself.
        for k in bucket.list("_combd/builds/").unwrap() {
            bucket.delete(&k).unwrap();
        }
        // What combd-update asks a new binary before a node moves to it: not
        // yet (and the build is asked for), from node 0's copy of the catalog.
        assert_eq!(http::builds_ready(&bucket, &dir.0.join("d0")), Ok(false), "no builds: not ready");
        assert!(bucket.list("_combd/builds/").unwrap().iter().any(|k| k.ends_with(".wasm")), "and the job is in");
        assert_eq!(http::builds_ready(&bucket, &dir.0.join("nowhere")), Ok(true), "a new node has nothing to wait for");
        start(2, ports[2], "builds2");
        assert!(eventually(60_000, || std::net::TcpStream::connect(("127.0.0.1", ports[2])).is_ok()), "node 2 is up");
        assert_eq!(http_to(ports[2], "fast.test", "POST", "/room/r/incr", "").0, 404, "not live on it while it builds");
        let stop = Arc::new(AtomicBool::new(false));
        builder(&stop);
        assert!(eventually(300_000, || http_to(ports[2], "fast.test", "POST", "/room/r/incr", "").0 == 200), "live on it once built");
        assert_eq!(http::builds_ready(&bucket, &dir.0.join("d0")), Ok(true), "built: ready");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// A tiny WebSocket client, for tests.
    struct Ws(std::net::TcpStream);

    impl Ws {
        /// Connect and handshake; `Err` holds the HTTP answer if refused.
        fn open(port: u16, path: &str) -> Result<Ws, String> {
            Ws::open_to(port, "localhost", path)
        }
        fn open_to(port: u16, host: &str, path: &str) -> Result<Ws, String> {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nhost: {host}\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n").unwrap();
            let mut head = vec![];
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if s.read(&mut b).unwrap_or(0) == 0 {
                    return Err(String::from_utf8_lossy(&head).into_owned());
                }
                head.push(b[0]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            if !head.starts_with("HTTP/1.1 101") {
                let mut rest = String::new();
                let _ = s.read_to_string(&mut rest);
                return Err(head + &rest);
            }
            assert!(head.contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{head}");
            Ok(Ws(s))
        }
        fn send(&mut self, op: u8, payload: &[u8]) {
            use std::io::Write;
            self.0.write_all(&ws::tests::client(true, op, payload)).unwrap();
        }
        fn text(&mut self, t: &str) {
            self.send(1, t.as_bytes());
        }
        /// The next frame from the server: (opcode, payload).
        fn recv(&mut self) -> (u8, Vec<u8>) {
            use std::io::Read;
            let mut h = [0u8; 2];
            self.0.read_exact(&mut h).expect("a frame");
            assert_eq!(h[1] & 0x80, 0, "a server never masks");
            let n = match h[1] & 0x7f {
                126 => {
                    let mut b = [0u8; 2];
                    self.0.read_exact(&mut b).unwrap();
                    u16::from_be_bytes(b) as usize
                }
                127 => {
                    let mut b = [0u8; 8];
                    self.0.read_exact(&mut b).unwrap();
                    u64::from_be_bytes(b) as usize
                }
                n => n as usize,
            };
            let mut p = vec![0u8; n];
            self.0.read_exact(&mut p).unwrap();
            (h[0] & 0x0f, p)
        }
        /// The next frame, or `None` if none comes in `ms`.
        fn recv_within(&mut self, ms: u64) -> Option<(u8, Vec<u8>)> {
            use std::io::Read;
            self.0.set_read_timeout(Some(Duration::from_millis(ms))).unwrap();
            let mut h = [0u8; 1];
            let r = self.0.peek(&mut h);
            self.0.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = Read::by_ref(&mut self.0);
            match r {
                Ok(n) if n > 0 => Some(self.recv()),
                _ => None,
            }
        }
        fn recv_text(&mut self) -> String {
            let (op, p) = self.recv();
            assert_eq!(op, 1, "wanted text, got opcode {op}: {p:?}");
            String::from_utf8(p).unwrap()
        }
    }

    /// WebSockets end to end: a chat room on two event loops, with
    /// broadcast, pings, hibernation (the object leaves memory, its sockets
    /// stay), sends from HTTP, a crash that sends nothing, and every kind of
    /// close.
    #[test]
    fn websockets_on_the_interpreter() {
        websockets_broadcast_hibernate_and_close(interp::Interp { max_pages: 16384 });
    }

    #[cfg(feature = "aot-guard")]
    #[test]
    fn websockets_on_aot() {
        websockets_broadcast_hibernate_and_close(aot::Aot);
    }

    fn websockets_broadcast_hibernate_and_close<E: Engine + Sync + Send + 'static>(eng: E)
    where
        E::Program: Sync + Send,
    {
        let dir = store::tempdir::Dir::new();
        let c = http::Config { idle: Duration::from_millis(300), ..cfg(&dir.0, 2) };
        let (port, _, _) = do_server(eng, c);
        let mut a = Ws::open(port, "/do/chat/ws").unwrap();
        let mut b = Ws::open(port, "/do/chat/ws").unwrap();
        // A message reaches everyone, the sender too.
        a.text("hello");
        assert!(a.recv_text().contains("\"text\":\"hello\""));
        assert!(b.recv_text().contains("\"text\":\"hello\""));
        // Binary, every byte value, both ways.
        let bytes: Vec<u8> = (0..=255u8).chain([0, 0, 255]).collect();
        a.send(2, &bytes);
        for c in [&mut a, &mut b] {
            let (op, got) = c.recv();
            assert_eq!(op, 2);
            let colon = got.iter().position(|&x| x == b':').unwrap();
            assert_eq!(&got[colon + 1..], &bytes[..]);
        }
        // The host answers pings itself.
        a.send(9, b"are you there");
        assert_eq!(a.recv(), (10, b"are you there".to_vec()));
        // Hibernation: the object leaves memory, the sockets stay open.
        assert!(eventually(5000, || stat(port, "evictions") >= 1), "the idle object should leave memory");
        assert_eq!(stat(port, "sockets_open"), 2);
        let opened = stat(port, "objects_opened");
        b.text("after a nap");
        assert!(a.recv_text().contains("after a nap"));
        assert!(b.recv_text().contains("after a nap"));
        assert!(stat(port, "objects_opened") > opened, "a message wakes the object");
        assert!(http(port, "GET", "/do/chat/msgs", "").1.contains("after a nap"));
        // Sends from a plain HTTP request.
        assert_eq!(http(port, "POST", "/do/chat/broadcast", "news"), (200, "{\"sent\":2}".into()));
        assert_eq!(a.recv_text(), "news");
        assert_eq!(b.recv_text(), "news");
        // A crash sends nothing: the next thing anyone gets is the next message.
        a.text("crash");
        b.text("still here");
        assert!(a.recv_text().contains("still here"));
        assert!(b.recv_text().contains("still here"));
        // The object closes one; it hears nothing back about it.
        a.text("bye");
        assert_eq!(a.recv(), (8, 4000u16.to_be_bytes().to_vec()));
        // The client closes the other: echoed, and the object hears it.
        b.send(8, &1000u16.to_be_bytes());
        assert_eq!(b.recv(), (8, 1000u16.to_be_bytes().to_vec()));
        assert!(eventually(3000, || http(port, "GET", "/do/chat/ws-stats", "").1.contains("\"closes\":1")), "{}", http(port, "GET", "/do/chat/ws-stats", "").1);
        assert!(eventually(3000, || stat(port, "sockets_open") == 0));
        // A refused handshake is plain HTTP.
        let refused = Ws::open(port, "/do/chat/ws-deny").err().expect("refused");
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        // Sockets on other rooms: some land on a loop that doesn't own their
        // room, and must be handed over (and still work).
        let mut rooms: Vec<Ws> = (0..8).map(|i| Ws::open(port, &format!("/do/room{i}/ws")).unwrap()).collect();
        for (i, r) in rooms.iter_mut().enumerate() {
            r.text(&format!("hi {i}"));
            assert!(r.recv_text().contains(&format!("hi {i}")));
        }
        assert!(stat(port, "ws_handoffs") >= 1, "no handshake was handed between loops");
        drop(rooms);
        // A frame that breaks the rules (not masked) gets a 1002.
        let mut c = Ws::open(port, "/do/chat/ws").unwrap();
        use std::io::Write;
        c.0.write_all(&[0x81, 0x02, b'h', b'i']).unwrap();
        assert_eq!(c.recv(), (8, 1002u16.to_be_bytes().to_vec()));
    }

    /// WebSockets in fleet mode: each connection stays on the node it
    /// reached, its events go to the object's owner like any request, and
    /// the socket list lives in the object's storage, so it survives a
    /// takeover. Two nodes; the owner stops; the other takes over and still
    /// reaches its socket.
    #[test]
    fn fleet_websockets_cross_nodes_and_survive_a_takeover() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 2).unwrap();
        let bucket = test_bucket();
        let wports = free_ports(2);
        let peers: HashMap<own::Node, String> = (0..2).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        let node = |n: own::Node| {
            let fc = fleet::FleetCfg {
                node: n,
                lease_ms: 1000,
                replicas: 0,
                s3: s3::S3 { bucket: bucket.clone(), ..base.clone() },
                peers: peers.clone(),
                wire_port: wports[n as usize],
                wire_certs: Some(dir.0.join("certs")),
                insecure: false,
                upload_ms: 0,
                upload_bytes: 1 << 20,
                read_lease: true,
                regions: HashMap::new(), place_ms: 0,
            };
            let c = http::Config { fleet: Some(fc), ..cfg(&dir.0.join(format!("d{n}")), 1) };
            do_server(interp::Interp { max_pages: 16384 }, c)
        };
        let (a, stop_a, ta) = node(0);
        let (b, _, _) = node(1);
        for p in [a, b] {
            stat(p, "fences");
        }
        // Node 0 takes the room; node 1's handshake goes there.
        let mut x = Ws::open(a, "/do/fws/ws").unwrap();
        let mut y = Ws::open(b, "/do/fws/ws").unwrap();
        y.text("from node 1");
        assert!(x.recv_text().contains("from node 1"));
        assert!(y.recv_text().contains("from node 1"));
        x.send(2, &[0, 1, 255]);
        for c in [&mut x, &mut y] {
            let (op, got) = c.recv();
            assert_eq!(op, 2);
            assert!(got.ends_with(&[0, 1, 255]));
        }
        // The owner stops. Node 1 takes the room over after the lease, with
        // its socket list, and still reaches y.
        stop_a.store(true, std::sync::atomic::Ordering::Relaxed);
        ta.join().unwrap();
        let mut back = false;
        for i in 0..20 {
            y.text(&format!("after {i}"));
            if let Some((1, m)) = y.recv_within(3000) {
                if String::from_utf8_lossy(&m).contains("after") {
                    back = true;
                    break;
                }
            }
        }
        assert!(back, "node 1 never took the room over");
        assert!(stat(b, "fences") + stat(b, "objects_opened") >= 1);
        assert!(http(b, "GET", "/do/fws/msgs", "").1.contains("from node 1"), "the room's state came along");
    }

    /// Bring your own database: Postgres through the host, from a Worker and
    /// from Durable Objects. Pooled statements, transactions within a request,
    /// and sessions an object holds across many messages, with row locks and
    /// advisory locks. Needs `COMBD_PG_TEST=postgres://...`.
    fn postgres_end_to_end<E: Engine + Sync + Send + 'static>(eng: E, tag: &str)
    where
        E::Program: Sync + Send,
    {
        let Ok(url) = std::env::var("COMBD_PG_TEST") else {
            eprintln!("skipping: set COMBD_PG_TEST to a Postgres URL");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let pg = pg::PgCfg { pools: vec![pg::PoolCfg { max: 3, statement_timeout_ms: 3000, session_idle_ms: 1500, ..pg::PoolCfg::new("main", &url) }] };
        let c = http::Config { pg: Some(pg), idle: Duration::from_millis(300), ..cfg(&dir.0, 2) };
        let (port, _, _) = do_server(eng, c);
        let t = format!("t_{tag}_{}", std::process::id());
        let q = |path: &str, sql: &str, p: serde_json::Value| -> serde_json::Value {
            let (code, body) = http(port, "POST", path, &serde_json::json!({ "q": sql, "p": p }).to_string());
            assert!(code == 200 || code == 500, "{path}: {code} {body}");
            serde_json::from_str(&body).unwrap_or(serde_json::json!({ "status": code, "body": body }))
        };
        let rows = |v: &serde_json::Value| v["rows"].clone();
        // 1. Pooled statements, every type we map, both ways.
        let r = q("/api/pg/q", &format!("CREATE TABLE {t} (k int PRIMARY KEY, v text, n bigint, f float8, b bool, j jsonb)"), serde_json::json!([]));
        assert!(r["error"].is_null(), "{r}");
        let r = q("/api/pg/q", &format!("INSERT INTO {t} VALUES ($1, $2, $3, $4, $5, $6)"), serde_json::json!([1, "one", 5_000_000_000i64, 1.5, true, {"a": [1, 2]}]));
        assert_eq!(r["changes"], 1, "{r}");
        let r = q("/api/pg/q", &format!("SELECT k, v, n, f, b, j FROM {t}"), serde_json::json!([]));
        assert_eq!(rows(&r), serde_json::json!([[1, "one", 5_000_000_000i64, 1.5, true, {"a": [1, 2]}]]), "{r}");
        let r = q("/api/pg/q", "SELEKT 1", serde_json::json!([]));
        assert_eq!(r["code"], "42601", "{r}");
        // 2. Pooled connections are reset: a `SET` never reaches the next user.
        for _ in 0..8 {
            q("/api/pg/q", "SET search_path TO nowhere", serde_json::json!([]));
        }
        for _ in 0..8 {
            assert!(q("/api/pg/q", &format!("SELECT count(*) FROM {t}"), serde_json::json!([]))["error"].is_null());
        }
        // ... and kept: a reset reuses the connection, it doesn't reconnect.
        let pid = || rows(&q("/api/pg/q", "SELECT pg_backend_pid()", serde_json::json!([])))[0][0].clone();
        assert_eq!(pid(), pid(), "every reset reconnected");
        // 3. A Worker's transaction, within one request; a crash rolls back,
        //    and its connection comes back (more crashes than pins).
        let tx = |path: &str, ks: &[i32]| {
            let mut list = vec![serde_json::json!({"q": "BEGIN"})];
            list.extend(ks.iter().map(|k| serde_json::json!({ "q": format!("INSERT INTO {t} (k, v) VALUES ($1, 'tx')"), "p": [k] })));
            // A crash comes before the COMMIT, mid-transaction.
            if path.ends_with("/tx") {
                list.push(serde_json::json!({"q": "COMMIT"}));
            }
            http(port, "POST", path, &serde_json::Value::Array(list).to_string())
        };
        assert_eq!(tx("/api/pg/tx", &[2, 3]).0, 200);
        for k in 10..15 {
            assert_eq!(tx("/api/pg/tx-crash", &[k]).0, 500);
        }
        assert_eq!(tx("/api/pg/tx", &[4]).0, 200, "crashed transactions leaked their connections");
        let ks = rows(&q("/api/pg/q", &format!("SELECT k FROM {t} ORDER BY k"), serde_json::json!([])));
        assert_eq!(ks, serde_json::json!([[1], [2], [3], [4]]), "the crashed inserts must be rolled back");
        // 4. A Durable Object's transaction over several messages, with a row
        //    lock. Another object's update waits for it, and nothing else does.
        assert_eq!(http(port, "POST", "/do/acct/pg/open", "").0, 200);
        q("/do/acct/pg/s", "BEGIN", serde_json::json!([]));
        let r = q("/do/acct/pg/s", &format!("SELECT v FROM {t} WHERE k = 1 FOR UPDATE"), serde_json::json!([]));
        assert_eq!(rows(&r), serde_json::json!([["one"]]), "{r}");
        let t2 = t.clone();
        let waiter = std::thread::spawn(move || {
            let st = Instant::now();
            let (code, body) = http(port, "POST", "/do/other/pg/q", &serde_json::json!({ "q": format!("UPDATE {t2} SET v = 'other' WHERE k = 1") }).to_string());
            (code, body, st.elapsed())
        });
        std::thread::sleep(Duration::from_millis(400));
        let st = Instant::now();
        assert_eq!(http(port, "GET", "/api/users/42", "").0, 200);
        assert!(st.elapsed() < Duration::from_millis(200), "a lock wait stalled the event loop: {:?}", st.elapsed());
        assert!(!waiter.is_finished(), "the other update should wait for the row lock");
        q("/do/acct/pg/s", &format!("UPDATE {t} SET v = 'acct' WHERE k = 1"), serde_json::json!([]));
        q("/do/acct/pg/s", "COMMIT", serde_json::json!([]));
        let (code, body, waited) = waiter.join().unwrap();
        assert!(code == 200 && body.contains("\"changes\":1") && waited >= Duration::from_millis(400), "{code} {body} {waited:?}");
        assert_eq!(rows(&q("/api/pg/q", &format!("SELECT v FROM {t} WHERE k = 1"), serde_json::json!([]))), serde_json::json!([["other"]]));
        // 5. An advisory lock held across messages.
        // Lock keys are global to the database: each run gets its own range.
        let base: i64 = (std::process::id() as i64 % 10_000) * 1000 + if tag == "aot" { 500 } else { 0 };
        let lock = |path: &str, f: &str, k: i64| rows(&q(path, &format!("SELECT {f}($1)"), serde_json::json!([base + k])))[0][0].clone();
        lock("/do/acct/pg/s", "pg_advisory_lock", 42);
        assert_eq!(lock("/do/other/pg/q", "pg_try_advisory_lock", 42), false);
        assert_eq!(lock("/do/acct/pg/s", "pg_advisory_unlock", 42), true);
        assert_eq!(lock("/do/other/pg/q", "pg_try_advisory_lock", 42), true);
        // A pooled statement's session is reset with the next statement on
        // its connection, or alone within a moment: a lock it took is gone by then.
        assert!(eventually(100, || lock("/do/other/pg/q", "pg_try_advisory_lock", 42) == serde_json::json!(true)), "a pooled lock must not outlive its statement by more than a moment");
        // Cached statements that went stale are prepared again: a table
        // altered under one, and the user deallocating everything.
        let star = format!("SELECT * FROM {t} WHERE k = 1");
        let row = |r: serde_json::Value| r[0].as_array().cloned().unwrap_or_default();
        let before = row(rows(&q("/api/pg/q", &star, serde_json::json!([]))));
        assert_eq!(before[1], "other");
        q("/api/pg/q", &format!("ALTER TABLE {t} ADD COLUMN extra int DEFAULT 7"), serde_json::json!([]));
        let raw = q("/api/pg/q", &star, serde_json::json!([]));
        let after = row(rows(&raw));
        assert_eq!((after.len(), after.last().cloned()), (before.len() + 1, Some(serde_json::json!(7))), "{raw:?}");
        q("/api/pg/q", "DEALLOCATE ALL", serde_json::json!([]));
        assert_eq!(row(rows(&q("/api/pg/q", &star, serde_json::json!([])))), after);
        // 6. A crash releases the object's session and its locks.
        lock("/do/acct/pg/s", "pg_advisory_lock", 43);
        assert_eq!(http(port, "POST", "/do/acct/pg/crash", "").0, 500);
        assert_eq!(lock("/do/other/pg/q", "pg_try_advisory_lock", 43), true);
        assert_eq!(http(port, "POST", "/do/acct/pg/s", "{\"q\":\"SELECT 1\"}").0, 409, "a fresh cell has no session");
        // 7. A blocking lock wait parks the cell; the loop goes on.
        assert_eq!(http(port, "POST", "/do/acct/pg/open", "").0, 200);
        lock("/do/acct/pg/s", "pg_advisory_lock", 44);
        assert_eq!(http(port, "POST", "/do/other2/pg/open", "").0, 200);
        let w2 = std::thread::spawn(move || http(port, "POST", "/do/other2/pg/s", &format!("{{\"q\":\"SELECT pg_advisory_lock({})\"}}", base + 44)));
        std::thread::sleep(Duration::from_millis(300));
        let st = Instant::now();
        assert_eq!(http(port, "GET", "/api/users/42", "").0, 200);
        assert!(st.elapsed() < Duration::from_millis(200));
        assert!(!w2.is_finished());
        lock("/do/acct/pg/s", "pg_advisory_unlock", 44);
        assert_eq!(w2.join().unwrap().0, 200);
        assert_eq!(http(port, "POST", "/do/other2/pg/close", "").1, "{\"closed\":true}");
        // 8. An idle session closes after `session_idle`, and so do its locks;
        //    until then it keeps its object in memory.
        lock("/do/acct/pg/s", "pg_advisory_lock", 45);
        let ev = stat(port, "evictions");
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(lock("/do/other/pg/q", "pg_try_advisory_lock", 45), false, "the session keeps its object (and lock) while it's fresh");
        assert!(eventually(4000, || lock("/do/other/pg/q", "pg_try_advisory_lock", 45) == true), "an idle session must close");
        assert!(eventually(2000, || stat(port, "evictions") > ev));
        q("/api/pg/q", &format!("DROP TABLE {t}"), serde_json::json!([]));
    }

    /// The pool's rules on a real Postgres: it grows to max, then asks wait in
    /// line and fail after the wait; a release serves the next in line; and
    /// a lease held too long is taken back (rolled back) by its member.
    #[test]
    fn postgres_pool_limits() {
        let Ok(url) = std::env::var("COMBD_PG_TEST") else {
            eprintln!("skipping: set COMBD_PG_TEST to a Postgres URL");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let pg = pg::PgCfg { pools: vec![pg::PoolCfg { max: 2, statement_timeout_ms: 3000, lease_ms: 1500, wait_ms: 300, ..pg::PoolCfg::new("main", &url) }] };
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, http::Config { pg: Some(pg), ..cfg(&dir.0, 2) });
        let q = |path: &str, sql: &str| http(port, "POST", path, &serde_json::json!({ "q": sql }).to_string());
        // Two sessions: the pool is at max.
        assert_eq!(http(port, "POST", "/do/a/pg/open", "").0, 200);
        assert_eq!(q("/do/a/pg/s", "BEGIN").0, 200);
        assert_eq!(http(port, "POST", "/do/b/pg/open", "").0, 200);
        let st = Instant::now();
        let (_, body) = q("/api/pg/q", "SELECT 1");
        assert!(body.contains("free in time") && st.elapsed() >= Duration::from_millis(300), "{body} {:?}", st.elapsed());
        // A release serves the one waiting.
        let w = std::thread::spawn(move || http(port, "POST", "/api/pg/q", "{\"q\":\"SELECT 7\"}"));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(http(port, "POST", "/do/b/pg/close", "").1, "{\"closed\":true}");
        assert!(w.join().unwrap().1.contains("[[7]]"));
        // Held past its lease: taken back, and the holder hears about it.
        std::thread::sleep(Duration::from_millis(1500));
        let (_, body) = q("/do/a/pg/s", "SELECT 1");
        assert!(body.contains("lease ran out"), "{body}");
        // Its member is free again: two more fit.
        assert_eq!(http(port, "POST", "/do/c/pg/open", "").0, 200);
        assert_eq!(http(port, "POST", "/do/d/pg/open", "").0, 200);
    }

    const GUEST_EV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../guest-ev/target/wasm32-unknown-unknown/release/combd_guest_ev.wasm");

    /// Many requests at once on one instance (event cells): fetch them all.
    fn many(port: u16, n: usize, path: &str) -> Vec<(u16, String)> {
        let path = path.to_string();
        let hs: Vec<_> = (0..n).map(|_| {
            let p = path.clone();
            std::thread::spawn(move || http(port, "GET", &p, ""))
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    }

    /// Event cells: one instance holds many requests that wait on I/O, shares
    /// state between them, keeps working after it answers, and a crash fails
    /// only the requests on that instance that weren't answered yet.
    fn event_cells<E: Engine + Sync + Send + 'static>(eng: E)
    where
        E::Program: Sync + Send,
    {
        let dir = store::tempdir::Dir::new();
        let wasm = std::fs::read(GUEST_EV).expect("build guest-ev first: cd guest-ev && cargo build --release --target wasm32-unknown-unknown");
        let (port, _, _) = server_for(eng, http::Config { ev_max: 8, ..cfg(&dir.0, 1) }, wasm);
        // 30 requests that each wait 400 ms: about 400 ms in all, on 4 instances (8 each).
        let t = Instant::now();
        let got = many(port, 30, "/sleep/400");
        assert!(got.iter().all(|r| *r == (200, "{\"slept\":400}".into())), "{got:?}");
        assert!(t.elapsed() < Duration::from_millis(1500), "took {:?}", t.elapsed());
        assert_eq!(stat(port, "ev_instances"), 4);
        // The extra instances are gone once idle; the first stays, with its state.
        for n in 1..=3 {
            assert_eq!(http(port, "GET", "/hello", ""), (200, format!("{{\"hits\":{n}}}")));
        }
        assert_eq!(stat(port, "ev_instances"), 4);
        // waitUntil: the answer comes first, the work after.
        let t = Instant::now();
        assert_eq!(http(port, "GET", "/later/300", "").0, 200);
        assert!(t.elapsed() < Duration::from_millis(250), "waited for the work: {:?}", t.elapsed());
        assert_eq!(http(port, "GET", "/later-count", ""), (200, "{\"later\":0}".into()));
        assert!(eventually(2000, || http(port, "GET", "/later-count", "").1 == "{\"later\":1}"));
        // A crash: the instance's unanswered requests fail with it.
        let waiting = std::thread::spawn(move || many(port, 5, "/sleep/600"));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(http(port, "GET", "/crash/50", "").0, 500);
        assert!(waiting.join().unwrap().iter().all(|r| r.0 == 500));
        // A fresh instance takes the next request, with fresh state.
        assert_eq!(http(port, "GET", "/hello", ""), (200, "{\"hits\":1}".into()));
        assert_eq!(http(port, "GET", "/nope", "").0, 404);
        assert!(stat(port, "ev_requests") >= 30 + 3 + 3 + 6 + 2);
    }

    /// Event cells and Postgres: transactions on pinned connections interleave
    /// on one instance, and one that fails rolls back alone.
    fn event_cells_postgres<E: Engine + Sync + Send + 'static>(eng: E, tag: &str)
    where
        E::Program: Sync + Send,
    {
        let Ok(url) = std::env::var("COMBD_PG_TEST") else {
            eprintln!("skipping: set COMBD_PG_TEST to a Postgres URL");
            return;
        };
        let dir = store::tempdir::Dir::new();
        let wasm = std::fs::read(GUEST_EV).unwrap();
        let pg = pg::PgCfg { pools: vec![pg::PoolCfg { max: 10, statement_timeout_ms: 3000, ..pg::PoolCfg::new("main", &url) }] };
        let (port, _, _) = server_for(eng, http::Config { pg: Some(pg), ..cfg(&dir.0, 1) }, wasm);
        let t = format!("ev_{tag}_{}", std::process::id());
        let q = |sql: &str| http(port, "POST", "/pg/q", &serde_json::json!({ "q": sql }).to_string());
        assert_eq!(q(&format!("CREATE TABLE {t} (k int PRIMARY KEY)")).0, 200);
        // 10 transactions, each pausing 300 ms halfway: about 300 ms in all.
        let start = Instant::now();
        let hs: Vec<_> = (0..10).map(|k| {
            let body = serde_json::json!([{ "q": format!("INSERT INTO {t} VALUES ($1)"), "p": [k] }, { "q": format!("INSERT INTO {t} VALUES ($1)"), "p": [k + 100] }]).to_string();
            std::thread::spawn(move || http(port, "POST", "/pg/tx/300", &body))
        }).collect();
        for h in hs {
            let r = h.join().unwrap();
            assert_eq!(r.0, 200, "{r:?}");
        }
        assert!(start.elapsed() < Duration::from_millis(1500), "took {:?}", start.elapsed());
        assert_eq!(stat(port, "ev_instances"), 1);
        // A failing second half (a duplicate key) rolls back the first half too.
        let body = serde_json::json!([{ "q": format!("INSERT INTO {t} VALUES (50)") }, { "q": format!("INSERT INTO {t} VALUES (1)") }]).to_string();
        assert_eq!(http(port, "POST", "/pg/tx/0", &body).0, 400);
        let (code, rows) = q(&format!("SELECT count(*)::int AS n, count(*) FILTER (WHERE k = 50)::int AS bad FROM {t}"));
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_str(&rows).unwrap();
        assert_eq!(v["rows"][0], serde_json::json!([20, 0]), "{v}");
        // Every pinned connection went back: 10 more fit.
        assert!(eventually(2000, || many(port, 10, "/hello").iter().all(|r| r.0 == 200)));
        let body = serde_json::json!([{ "q": "SELECT 1" }, { "q": "SELECT 2" }]).to_string();
        let hs: Vec<_> = (0..10).map(|_| { let b = body.clone(); std::thread::spawn(move || http(port, "POST", "/pg/tx/50", &b).0) }).collect();
        assert!(hs.into_iter().all(|h| h.join().unwrap() == 200));
        assert_eq!(q(&format!("DROP TABLE {t}")).0, 200);
    }

    #[test]
    fn event_cells_postgres_on_the_interpreter() {
        event_cells_postgres(interp::Interp { max_pages: 16384 }, "interp");
    }

    #[cfg(feature = "aot-guard")]
    #[test]
    fn event_cells_postgres_on_aot() {
        event_cells_postgres(aot::Aot, "aot");
    }

    /// The manifest end to end: typed vars and secrets reach cells as env
    /// vars, and a pool is set up from it. A mistake stops everything.
    #[test]
    fn the_manifest_sets_up_vars_secrets_and_pools() {
        let app = store::tempdir::Dir::new();
        let pg_url = std::env::var("COMBD_PG_TEST").unwrap_or("postgres://nobody@127.0.0.1:1/x".into());
        std::fs::write(app.0.join("combd.yaml"), r#"
app: { name: shop }
vars:
  REGION: { type: string, pattern: "^[a-z]+-[a-z]+$" }
  PG_MAX: { type: int, min: 1, default: 4 }
  "MY_FLAG?": { type: bool }
secrets:
  PG_URL: { secret: pg-url, type: url }
http:
  api: { module: api, routes: ["/*"] }
  other: { module: api, routes: ["other.test/*"] }
pool:
  main: { module: "combd:postgres", url: $PG_URL, connection: { retries: 0 }, callers: [api], max: $PG_MAX, wait_ms: 2000 }
"#).unwrap();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::copy(GUEST_EV, manifest::module_path(&app.0, "api")).unwrap();
        std::fs::write(app.0.join("combd.dev.toml"), format!("[vars]\nREGION = \"us-east\"\n[secrets]\npg-url = {pg_url:?}\n")).unwrap();
        let eng = interp::Interp { max_pages: 16384 };
        let (a, pg) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let p = &pg.as_ref().unwrap().pools[0];
        assert_eq!((p.name.as_str(), p.max, p.wait_ms, p.retries), ("local/shop/main", 4, 2000, 0), "pools are named for their app");
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(eng, http::Config { pg, ..cfg(&dir.0, 1) }, move |_| a);
        assert_eq!(http(port, "GET", "/env/REGION", ""), (200, "{\"value\":\"us-east\"}".into()));
        assert_eq!(http(port, "GET", "/env/PG_MAX", ""), (200, "{\"value\":\"4\"}".into()));
        assert_eq!(http(port, "GET", "/env/PG_URL", "").1, serde_json::json!({ "value": pg_url }).to_string(), "secrets are env vars too");
        assert_eq!(http(port, "GET", "/env/MY_FLAG", "").0, 404, "an unset optional isn't set");
        if std::env::var("COMBD_PG_TEST").is_ok() {
            assert!(http(port, "POST", "/pg/q", "{\"q\":\"SELECT 41 + 1\"}").1.contains("[[42]]"));
            // Only the pool's owners may use it.
            let (_, body) = http_to(port, "other.test", "POST", "/pg/q", "{\"q\":\"SELECT 1\"}");
            assert!(body.contains("denied: other isn't one of pool main's callers"), "{body}");
        }
        // A mistake in the environment: nothing starts.
        std::fs::write(app.0.join("combd.dev.toml"), "[vars]\nREGION = \"Not Valid\"\n").unwrap();
        // (No unwrap_err: that would need Debug, and a pool's config holds its password.)
        let Err(e) = http::load_app(&interp::Interp { max_pages: 16384 }, &app.0) else { panic!("a bad environment must not start") };
        assert!(e.contains("REGION") && e.contains("doesn't match") && e.contains("PG_URL: required"), "{e}");
    }

    /// An app of several processes, from its manifest: routes by host and
    /// path, each process its own cells and state, objects scoped to their
    /// process, and nothing else reachable.
    fn an_app_of_processes<E: Engine + Sync + Send + 'static>(eng: E)
    where
        E::Program: Sync + Send,
    {
        let app = store::tempdir::Dir::new();
        std::fs::write(app.0.join("combd.toml"), r#"
[app]
name = "shop"
[http.edge]
module = "ev"
routes = ["/hello", "/sleep/*"]
[http.admin]
module = "ev"
routes = ["admin.test/hello"]
[http.api]
module = "classic"
routes = ["/api/*"]
[http.fresh]
module = "classic"
routes = ["fresh.test/api/*"]
fresh = true
[durable.room]
module = "classic"
routes = ["/room/:id/*"]
"#).unwrap();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::copy(GUEST_EV, manifest::module_path(&app.0, "ev")).unwrap();
        std::fs::copy(GUEST, manifest::module_path(&app.0, "classic")).unwrap();
        let (a, pg) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.progs.len(), 2, "one program per kind");
        let dir = store::tempdir::Dir::new();
        // One loop: an instance's memory is its own, so with two loops the
        // hit counts would depend on which loop took the connection.
        let c = http::Config { pg, ..cfg(&dir.0, 1) };
        let (port, _, _) = server_app(eng, c, move |_| a);
        // Two processes on one module: their own instances, their own state.
        for n in 1..=3 {
            assert_eq!(http(port, "GET", "/hello", ""), (200, format!("{{\"hits\":{n}}}")));
        }
        assert_eq!(http_to(port, "admin.test", "GET", "/hello", ""), (200, "{\"hits\":1}".into()));
        assert_eq!(http_to(port, "ADMIN.test:8080", "GET", "/hello", ""), (200, "{\"hits\":2}".into()), "hosts ignore case and port");
        assert_eq!(http(port, "GET", "/sleep/20", ""), (200, "{\"slept\":20}".into()));
        // A classic worker on its routes.
        assert_eq!(http(port, "GET", "/api/users/42", "").0, 200);
        // A warm worker keeps its instance's state; a `fresh` one never does.
        assert_eq!((http(port, "GET", "/api/hits", "").1, http(port, "GET", "/api/hits", "").1), ("{\"hits\":1}".into(), "{\"hits\":2}".into()));
        let fresh = || http_to(port, "fresh.test", "GET", "/api/hits", "").1;
        assert_eq!((fresh(), fresh()), ("{\"hits\":1}".into(), "{\"hits\":1}".into()));
        // Objects, scoped to their process.
        for n in 1..=2 {
            assert_eq!(http(port, "POST", "/room/r1/incr", ""), (200, format!("{{\"count\":{n}}}")));
        }
        assert_eq!(http(port, "GET", "/room/r2/count", ""), (200, "{\"count\":0}".into()));
        assert!(on_disk(&dir.0, "local/shop/room/r1", "SELECT v FROM kv").contains("[[2]]"));
        // Nothing else: no route, the old fixed routes, a route's prefix.
        for path in ["/nope", "/do/r1/count", "/room", "/hello/extra"] {
            assert_eq!(http(port, "GET", path, "").0, 404, "{path}");
        }
    }

    /// `call` and `send` between processes: only where `callers` allows,
    /// always one answer (the target's, a timeout, or a crash's 500), and a
    /// request's sends go out only if it succeeds (an object's: once committed).
    fn processes_call_and_send<E: Engine + Sync + Send + 'static>(eng: E)
    where
        E::Program: Sync + Send,
    {
        let app = store::tempdir::Dir::new();
        std::fs::write(app.0.join("combd.toml"), r#"
[app]
name = "shop"
[http.front]
module = "ev"
routes = ["/call", "/send"]
[http.sleeper]
module = "ev"
callers = ["front"]
[http.worker]
module = "classic"
callers = ["front", "room"]
[durable.room]
module = "classic"
routes = ["/room/:id/*"]
callers = ["front", "room"]
[http.admin]
module = "classic"
routes = ["/admin/*"]
"#).unwrap();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::copy(GUEST_EV, manifest::module_path(&app.0, "ev")).unwrap();
        std::fs::copy(GUEST, manifest::module_path(&app.0, "classic")).unwrap();
        let (a, pg) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(eng, http::Config { pg, ..cfg(&dir.0, 2) }, move |_| a);
        let call = |msg: serde_json::Value| -> serde_json::Value {
            let (code, body) = http(port, "POST", "/call", &msg.to_string());
            assert_eq!(code, 200, "{body}");
            serde_json::from_str(&body).unwrap_or_else(|_| panic!("{body}"))
        };
        let count = |id: &str| http(port, "GET", &format!("/room/{id}/count"), "").1;
        // A worker, and an object.
        let r = call(serde_json::json!({ "to": "worker", "method": "GET", "path": "/api/users/42" }));
        assert_eq!(r["status"], 200, "{r}");
        let r = call(serde_json::json!({ "to": "room", "id": "r1", "path": "/incr" }));
        assert_eq!((r["status"].clone(), r["body"].clone()), (200.into(), "{\"count\":1}".into()), "{r}");
        assert_eq!(count("r1"), "{\"count\":1}");
        // Only where `callers` allows.
        assert_eq!(call(serde_json::json!({ "to": "admin", "path": "/admin/x" }))["error"], "denied: front may not call admin");
        assert_eq!(call(serde_json::json!({ "to": "nobody" }))["error"], "denied: front may not call nobody");
        assert!(call(serde_json::json!({ "to": "room", "path": "/count" }))["error"].as_str().unwrap().contains("name the object"));
        // Always one answer: a timeout, or a crash's 500.
        let t = Instant::now();
        assert_eq!(call(serde_json::json!({ "to": "sleeper", "method": "GET", "path": "/sleep/3000", "timeout_ms": 200 }))["error"], "timeout");
        assert!(t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
        assert_eq!(call(serde_json::json!({ "to": "sleeper", "method": "GET", "path": "/crash/0" }))["status"], 500);
        // Fire and forget.
        assert_eq!(http(port, "POST", "/send", &serde_json::json!({ "to": "room", "id": "r2", "path": "/incr" }).to_string()).0, 202);
        assert!(eventually(2000, || count("r2") == "{\"count\":1}"));
        // An object calls, waiting (parked); and sends, after its commit.
        let (code, body) = http(port, "POST", "/room/r3/call", &serde_json::json!({ "to": "worker", "method": "GET", "path": "/api/users/7" }).to_string());
        assert!(code == 200 && body.contains("\"status\":200"), "{code} {body}");
        assert_eq!(http(port, "POST", "/room/r4/send", &serde_json::json!({ "to": "room", "id": "r5", "path": "/incr" }).to_string()).0, 202);
        assert!(eventually(2000, || count("r5") == "{\"count\":1}"));
        // A request that crashes never sends.
        assert_eq!(http(port, "POST", "/room/r6/send-crash", &serde_json::json!({ "to": "room", "id": "r7", "path": "/incr" }).to_string()).0, 500);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(count("r7"), "{\"count\":0}");
    }

    #[test]
    fn processes_call_and_send_on_the_interpreter() {
        processes_call_and_send(interp::Interp { max_pages: 16384 });
    }

    #[cfg(feature = "aot-guard")]
    #[test]
    fn processes_call_and_send_on_aot() {
        processes_call_and_send(aot::Aot);
    }

    /// Six apps of three orgs on one node, all with the same processes and
    /// the same object ids: nothing crosses between them, and one busy org
    /// can't crowd out another.
    /// Six apps of three orgs under `root`, all with the same processes
    /// (`api`, `worker`, durable `room`) and their own domain
    /// (`<app>.<org>.test`) and `TENANT` var. acme/shop alone has a pool.
    fn six_apps(root: &std::path::Path) -> (Vec<(String, String)>, Vec<(String, std::path::PathBuf)>) {
        let apps: Vec<(String, String)> = ["acme", "globex", "initech"].iter().flat_map(|o| ["shop", "blog"].iter().map(move |a| (o.to_string(), a.to_string()))).collect();
        let manifest = |org: &str, app: &str| {
            format!(r#"
[app]
name = "{app}"
domains = ["{app}.{org}.test"]
[vars]
TENANT = {{ type = "string" }}
[http.api]
module = "ev"
routes = ["/env/*", "/call", "/send", "/pg/q"]
[http.worker]
module = "classic"
routes = ["/api/*"]
callers = ["api"]
[durable.room]
module = "classic"
routes = ["/room/:id/*"]
callers = ["api"]
"#)
        };
        let mut dirs = vec![];
        for (org, app) in &apps {
            let d = root.join(org).join(app);
            std::fs::create_dir_all(d.join("modules")).unwrap();
            let mut m = manifest(org, app);
            // One app has a pool; the others must never reach it.
            if (org.as_str(), app.as_str()) == ("acme", "shop") {
                m = m.replace("[durable.room]", "[pool.main]\nmodule = \"combd:postgres\"\nurl = \"postgres://nobody@127.0.0.1:1/x\"\ncallers = [\"api\"]\n[durable.room]");
            }
            std::fs::write(d.join("combd.toml"), m).unwrap();
            std::fs::write(d.join("combd.dev.toml"), format!("[vars]\nTENANT = \"{org}/{app}\"\n")).unwrap();
            std::fs::copy(GUEST_EV, manifest::module_path(&d, "ev")).unwrap();
            std::fs::copy(GUEST, manifest::module_path(&d, "classic")).unwrap();
            dirs.push((org.clone(), d));
        }
        (apps, dirs)
    }

    #[test]
    fn six_tenants_share_a_node_and_nothing_else() {
        let root = store::tempdir::Dir::new();
        let (apps, dirs) = six_apps(&root.0);
        let eng = interp::Interp { max_pages: 16384 };
        let (a, pg) = http::load_apps(&eng, &dirs).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((a.tenants.len(), a.procs.len(), a.progs.len()), (6, 18, 2), "six apps, two programs: the same bytes load once");
        let data = store::tempdir::Dir::new();
        // One loop, so the fairness check below shares one run queue.
        let (port, _, _) = server_app(eng, http::Config { pg, ..cfg(&data.0, 1) }, move |_| a);
        let host = |org: &str, app: &str| format!("{app}.{org}.test");
        for (i, (org, app)) in apps.iter().enumerate() {
            let h = host(org, app);
            // Its own env vars.
            assert_eq!(http_to(port, &h, "GET", "/env/TENANT", ""), (200, format!("{{\"value\":\"{org}/{app}\"}}")));
            // Its own objects: the same id, a different count in each app.
            for _ in 0..=i {
                assert_eq!(http_to(port, &h, "POST", "/room/r1/incr", "").0, 200);
            }
        }
        for (i, (org, app)) in apps.iter().enumerate() {
            let h = host(org, app);
            assert_eq!(http_to(port, &h, "GET", "/room/r1/count", "").1, format!("{{\"count\":{}}}", i + 1), "{org}/{app}");
            assert!(on_disk(&data.0, &format!("{org}/{app}/room/r1"), "SELECT v FROM kv").contains(&format!("[[{}]]", i + 1)));
            assert!(data.0.join(org).join(app).is_dir(), "its own folder");
            // Its own processes: `room` and `worker` are always its own.
            let (_, body) = http_to(port, &h, "POST", "/call", &serde_json::json!({ "to": "room", "id": "r1", "method": "GET", "path": "/count" }).to_string());
            assert!(body.contains(&format!("{{\\\"count\\\":{}}}", i + 1)), "{org}/{app}: {body}");
            let (_, body) = http_to(port, &h, "POST", "/call", &serde_json::json!({ "to": "worker", "method": "GET", "path": "/api/users/1" }).to_string());
            assert!(body.contains("\"status\":200"), "{body}");
            // Never another app's pool.
            if (org.as_str(), app.as_str()) != ("acme", "shop") {
                let (_, body) = http_to(port, &h, "POST", "/pg/q", "{\"q\":\"SELECT 1\"}");
                assert!(body.contains("isn't one of pool main's callers"), "{org}/{app}: {body}");
            }
        }
        // Only its domains: an unknown host, or another app's path, is a 404.
        assert_eq!(http_to(port, "nobody.test", "GET", "/env/TENANT", "").0, 404);
        assert_eq!(http_to(port, "x", "GET", "/room/r1/count", "").0, 404);
        // Fairness: acme floods with 100 slow requests while globex sends 6.
        // Shared by request, globex would finish with everyone else; shared
        // by org, its batch is done long before acme's.
        let mut n = 20;
        loop {
            let t = Instant::now();
            assert_eq!(http_to(port, &host("acme", "shop"), "GET", &format!("/api/fib/{n}"), "").0, 200);
            if t.elapsed() >= Duration::from_millis(20) || n >= 32 {
                break;
            }
            n += 1;
        }
        let start = Instant::now();
        let batch = |h: String, count: usize| {
            let path = format!("/api/fib/{n}");
            std::thread::spawn(move || {
                let hs: Vec<_> = (0..count).map(|_| {
                    let (h, p) = (h.clone(), path.clone());
                    std::thread::spawn(move || http_to(port, &h, "GET", &p, "").0)
                }).collect();
                assert!(hs.into_iter().all(|h| h.join().unwrap() == 200));
                start.elapsed()
            })
        };
        let acme = batch(host("acme", "shop"), 100);
        std::thread::sleep(Duration::from_millis(30));
        let globex = batch(host("globex", "shop"), 6);
        let (ta, tg) = (acme.join().unwrap(), globex.join().unwrap());
        eprintln!("fairness: globex's 6 done in {tg:?}, acme's 100 in {ta:?}");
        assert!(tg.as_secs_f64() < ta.as_secs_f64() * 0.6, "globex took {tg:?}, acme {ta:?}");
    }

    /// Six apps on three nodes (D3: two peers confirm every write), every
    /// node getting requests for every app: env vars and workers answer
    /// locally, objects are found on their owner wherever it is, `call` and
    /// `send` reach across nodes, and when a node stops the other two take
    /// over every app's objects with every write.
    #[test]
    fn six_tenants_on_three_nodes() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let root = store::tempdir::Dir::new();
        let (apps, dirs) = six_apps(&root.0);
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 3).unwrap();
        let bucket = test_bucket();
        let wports = free_ports(3);
        let peers: HashMap<own::Node, String> = (0..3).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        let node = |n: own::Node| {
            let fc = fleet::FleetCfg {
                node: n,
                lease_ms: 1000,
                replicas: 2,
                s3: s3::S3 { bucket: bucket.clone(), ..base.clone() },
                peers: peers.clone(),
                wire_port: wports[n as usize],
                wire_certs: Some(dir.0.join("certs")),
                insecure: false,
                upload_ms: 0,
                upload_bytes: 1 << 20,
                read_lease: true,
                regions: HashMap::new(), place_ms: 0,
            };
            let eng = interp::Interp { max_pages: 16384 };
            let (a, pg) = http::load_apps(&eng, &dirs).unwrap_or_else(|e| panic!("{e}"));
            let c = http::Config { pg, fleet: Some(fc), ..cfg(&dir.0.join(format!("d{n}")), 2) };
            server_app(eng, c, move |_| a)
        };
        let (n0, stop0, t0) = node(0);
        let (n1, _, _) = node(1);
        let (n2, _, _) = node(2);
        for p in [n0, n1, n2] {
            stat(p, "fences"); // every node really serves
        }
        let host = |org: &str, app: &str| format!("{app}.{org}.test");
        // Ask until it answers 200 (a takeover answers 503 for about a lease;
        // much longer only on a starved machine).
        let ok = |port: u16, h: &str, method: &str, path: &str, body: &str| -> String {
            for _ in 0..300 {
                let (c, r) = http_to(port, h, method, path, body);
                if c == 200 {
                    return r;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let last = http_to(port, h, method, path, body);
            panic!("{h} {method} {path} never answered 200 on {port}: last {last:?}; stats {}", http(port, "GET", "/_combd/stats", "").1);
        };
        let nodes = [n0, n1, n2];
        // Until a node stops, nothing may need a retry: a request that lost a
        // race for a new object never ran, so the host routes it again.
        let first = |port: u16, h: &str, method: &str, path: &str, body: &str| -> String {
            let (c, r) = http_to(port, h, method, path, body);
            assert_eq!(c, 200, "{h} {method} {path} on {port}: {r}");
            r
        };
        // Every node answers for every app, with that app's env vars.
        for (org, app) in &apps {
            for &p in &nodes {
                assert_eq!(first(p, &host(org, app), "GET", "/env/TENANT", ""), format!("{{\"value\":\"{org}/{app}\"}}"));
            }
        }
        // Every app's `room/r1` gets increments through all three nodes, in
        // turn: they're forwarded to its owner, wherever that is. Each app
        // counts on its own.
        for round in 0..3 {
            for (i, (org, app)) in apps.iter().enumerate() {
                for _ in 0..=i {
                    first(nodes[(round + i) % 3], &host(org, app), "POST", "/room/r1/incr", "");
                }
            }
        }
        for (i, (org, app)) in apps.iter().enumerate() {
            let want = format!("{{\"count\":{}}}", 3 * (i + 1));
            for &p in &nodes {
                assert_eq!(first(p, &host(org, app), "GET", "/room/r1/count", ""), want, "{org}/{app} on {p}");
            }
        }
        // `call` from any node reaches the owner; `send` too.
        for (i, (org, app)) in apps.iter().enumerate() {
            let h = host(org, app);
            for &p in &nodes {
                let body = first(p, &h, "POST", "/call", &serde_json::json!({ "to": "room", "id": "r1", "method": "GET", "path": "/count" }).to_string());
                assert!(body.contains(&format!("{{\\\"count\\\":{}}}", 3 * (i + 1))), "{org}/{app} via {p}: {body}");
            }
            for &p in &nodes {
                assert_eq!(http_to(p, &h, "POST", "/send", &serde_json::json!({ "to": "room", "id": "sent", "path": "/incr" }).to_string()).0, 202);
            }
        }
        for (org, app) in &apps {
            assert!(eventually(5000, || http_to(n1, &host(org, app), "GET", "/room/sent/count", "").1 == "{\"count\":3}"), "{org}/{app}: {:?}", http_to(n1, &host(org, app), "GET", "/room/sent/count", ""));
        }
        assert!(nodes.iter().map(|&p| stat(p, "peer_acks")).sum::<i64>() > 0, "D3: peers confirm writes");
        // Node 0 stops. The other two take over its objects, in every app,
        // with every write, and go on.
        stop0.store(true, std::sync::atomic::Ordering::Relaxed);
        t0.join().unwrap();
        for (i, (org, app)) in apps.iter().enumerate() {
            let h = host(org, app);
            assert_eq!(ok(n1, &h, "GET", "/room/r1/count", ""), format!("{{\"count\":{}}}", 3 * (i + 1)), "{org}/{app} after the stop");
            assert_eq!(ok(n2, &h, "POST", "/room/r1/incr", ""), format!("{{\"count\":{}}}", 3 * (i + 1) + 1));
            assert_eq!(ok(n1, &h, "GET", "/room/r1/count", ""), format!("{{\"count\":{}}}", 3 * (i + 1) + 1));
            assert_eq!(ok(n2, &h, "GET", "/room/sent/count", ""), "{\"count\":3}");
        }
    }

    /// A deploy onto a running node: in-flight work finishes on the old
    /// version, new requests get the new one, an object keeps its state and
    /// runs the new code, and a bad version never goes live.
    #[test]
    fn a_deploy_swaps_a_running_app() {
        let root = store::tempdir::Dir::new();
        let v = |n: u32, api_kind: &str, api_routes: &str| {
            let d = root.0.join(format!("v{n}")).join("shop");
            std::fs::create_dir_all(d.join("modules")).unwrap();
            std::fs::copy(GUEST_EV, manifest::module_path(&d, "ev")).unwrap();
            std::fs::copy(GUEST, manifest::module_path(&d, "classic")).unwrap();
            std::fs::write(d.join("combd.toml"), format!("[app]\nname = \"shop\"\n[vars]\nVERSION = \"{n}\"\n[http.api]\nmodule = \"{api_kind}\"\nroutes = {api_routes}\n[http.edge]\nmodule = \"ev\"\nroutes = [\"/env/*\", \"/sleep/*\"]\n[durable.room]\nmodule = \"classic\"\nroutes = [\"/room/:id/*\"]\n")).unwrap();
            d
        };
        let (v1, v2) = (v(1, "ev", "[\"/hello\"]"), v(2, "classic", "[\"/api/*\"]"));
        let eng = interp::Interp { max_pages: 16384 };
        let (a, _) = http::load_app(&eng, &v1).unwrap_or_else(|e| panic!("{e}"));
        let live = Arc::new(http::Live::new(a));
        let dir = store::tempdir::Dir::new();
        let port = free_ports(1)[0];
        let (l2, c) = (live.clone(), cfg(&dir.0, 1));
        std::thread::spawn(move || http::serve_live(&interp::Interp { max_pages: 16384 }, &l2, port, c));
        assert!(eventually(2000, || std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()));
        assert_eq!(http(port, "GET", "/env/VERSION", ""), (200, "{\"value\":\"1\"}".into()));
        assert_eq!(http(port, "GET", "/hello", "").0, 200);
        for n in 1..=3 {
            assert_eq!(http(port, "POST", "/room/r1/incr", ""), (200, format!("{{\"count\":{n}}}")));
        }
        // A slow request on v1, in flight across the deploy.
        let slow = std::thread::spawn(move || http(port, "GET", "/sleep/600", ""));
        std::thread::sleep(Duration::from_millis(100));
        // A bad version: refused, and v1 keeps serving.
        let bad = v(3, "missing", "[\"/x\"]");
        assert!(http::deploy_dir(&eng, &live, "local", &bad).unwrap_err().contains("no module modules/missing.wasm"));
        assert_eq!(http(port, "GET", "/hello", "").0, 200);
        // v2: `api` is another module on other routes, and VERSION is 2.
        http::deploy_dir(&eng, &live, "local", &v2).unwrap_or_else(|e| panic!("{e}"));
        assert!(eventually(1000, || http(port, "GET", "/env/VERSION", "").1 == "{\"value\":\"2\"}"));
        assert_eq!(http(port, "GET", "/hello", "").0, 404, "v1's route is gone");
        assert_eq!(http(port, "GET", "/api/users/7", "").0, 200, "v2's route is there");
        assert_eq!(slow.join().unwrap(), (200, "{\"slept\":600}".into()), "in flight across the deploy, it finished");
        // The object keeps its state, on v2's code.
        assert_eq!(http(port, "GET", "/room/r1/count", ""), (200, "{\"count\":3}".into()));
        assert_eq!(http(port, "POST", "/room/r1/incr", ""), (200, "{\"count\":4}".into()));
    }

    /// Deploys through the bucket, on three nodes that start with nothing:
    /// a deploy reaches every node, vars and new versions too, a bad deploy
    /// never moves the pointer, and an app taken down is gone everywhere.
    #[test]
    fn deploys_reach_every_node() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 3).unwrap();
        let bucket = s3::S3 { bucket: test_bucket(), ..base.clone() };
        let wports = free_ports(3);
        let peers: HashMap<own::Node, String> = (0..3).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        // Node 0 has the admin API, over TLS with the fleet's certificates.
        let (token, line) = admin::new_token(&["acme/*:*"]);
        let (weak, weak_line) = admin::new_token(&["acme/blog:deploy"]);
        let (ops, ops_line) = admin::new_token(&["*/*:read"]);
        let admin_ports = free_ports(2);
        let ports = free_ports(3);
        let public: std::collections::BTreeMap<u32, String> = (0..3).map(|n| (n, format!("http://127.0.0.1:{}", ports[n as usize]))).collect();
        let admin_port = admin_ports[0];
        let certs = dir.0.join("certs");
        let master = dir.0.join("master.key");
        secrets::new_master(&master).unwrap();
        let nodes: Vec<u16> = (0..3u32)
            .map(|n| {
                let fc = fleet::FleetCfg { node: n, lease_ms: 1000, replicas: 2, s3: bucket.clone(), peers: peers.clone(), wire_port: wports[n as usize], wire_certs: Some(certs.clone()), insecure: false, upload_ms: 0, upload_bytes: 1 << 20, read_lease: true, regions: HashMap::new(), place_ms: 0 };
                let c = http::Config { fleet: Some(fc), master_key: Some(master.clone()), public: public.clone(), ..cfg(&dir.0.join(format!("d{n}")), 2) };
                // Nodes 0 and 1 take deploys (to race them against each other).
                let admin = (n < 2).then(|| admin::AdminCfg { port: admin_ports[n as usize], public: true, tls: Some(wire::admin_tls(&certs, n).unwrap()), tokens: admin::parse_tokens(&format!("{line}\n{weak_line}\n{ops_line}")).unwrap() });
                let port = ports[n as usize];
                std::thread::spawn(move || http::serve_fleet_apps(&interp::Interp { max_pages: 16384 }, port, c, admin).unwrap());
                port
            })
            .collect();
        let url = format!("https://127.0.0.1:{admin_port}");
        let tls = wire::admin_client(&certs.join("ca.pem")).unwrap();
        assert!(eventually(10_000, || std::net::TcpStream::connect(("127.0.0.1", admin_port)).is_ok()));
        let api = |token: &str, method: &str, path: &str, body: &[u8]| admin::request(&url, Some((&tls, "node0.combd")), token, method, path, body).unwrap();
        for &p in &nodes {
            assert!(eventually(10_000, || std::net::TcpStream::connect(("127.0.0.1", p)).is_ok()));
            stat(p, "fences");
        }
        // The cluster's view: a `*/*` token sees every node; an app's doesn't.
        let (code, v) = api(&ops, "GET", "/v1/nodes", b"");
        assert_eq!(code, 200, "{v}");
        let seen: Vec<u64> = v["nodes"].as_array().unwrap().iter().map(|x| x["stats"]["node"].as_u64().unwrap_or(99)).collect();
        assert_eq!(seen, [0, 1, 2], "{v}");
        assert_eq!(api(&token, "GET", "/v1/nodes", b"").0, 403, "acme's token isn't the cluster's");
        let (code, page) = admin::request(&url, Some((&tls, "node0.combd")), "", "GET", "/", b"").unwrap();
        assert_eq!(code, 200, "the panel, with no token: {page}");
        // An app folder: version `n`, with `extra` routes on `api`.
        let src = store::tempdir::Dir::new();
        let app = |name: &str, domain: &str, routes: &str, vars: &str| {
            let d = src.0.join(name);
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join("modules")).unwrap();
            std::fs::copy(GUEST_EV, manifest::module_path(&d, "ev")).unwrap();
            std::fs::copy(GUEST, manifest::module_path(&d, "classic")).unwrap();
            std::fs::write(d.join("combd.toml"), format!("[app]\nname = \"{name}\"\ndomains = [\"{domain}\"]\n[vars]\n{vars}\n[http.api]\nmodule = \"ev\"\nroutes = {routes}\n[durable.room]\nmodule = \"classic\"\nroutes = [\"/room/:id/*\"]\n")).unwrap();
            deploy::bundle(&d).unwrap()
        };
        let deploy = |name: &str, b: &[u8]| -> Result<String, String> {
            match api(&token, "PUT", &format!("/v1/apps/acme/{name}"), b) {
                (200, v) => Ok(v["version"].as_str().unwrap().to_string()),
                (_, v) => Err(v["error"].as_str().unwrap_or("?").to_string()),
            }
        };
        let everywhere = |h: &str, path: &str, want: &str| {
            for &p in &nodes {
                assert!(eventually(10_000, || http_to(p, h, "GET", path, "").1 == want), "node {p} {h}{path}: {:?}, want {want}", http_to(p, h, "GET", path, ""));
            }
        };
        // Nothing yet.
        assert_eq!(http_to(nodes[0], "shop.test", "GET", "/env/V", "").0, 404);
        // v1 reaches every node: pushed, not polled.
        let v1 = deploy("shop", &app("shop", "shop.test", "[\"/env/*\"]", "V = \"one\"")).unwrap_or_else(|e| panic!("{e}"));
        let t = Instant::now();
        everywhere("shop.test", "/env/V", "{\"value\":\"one\"}");
        eprintln!("a deploy reached all three nodes {:?} after it was acknowledged", t.elapsed());
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        for (i, &p) in nodes.iter().cycle().take(9).enumerate() {
            assert_eq!(http_to(p, "shop.test", "POST", "/room/r1/incr", "").1, format!("{{\"count\":{}}}", i + 1));
        }
        // Only a token with the right scope, and no body read before that.
        let shop = deploy::bundle(&src.0.join("shop")).unwrap();
        assert_eq!(api("", "PUT", "/v1/apps/acme/shop", &shop).0, 401);
        assert_eq!(api(&weak, "PUT", "/v1/apps/acme/shop", &shop).0, 403, "scoped to acme/blog");
        assert_eq!(api(&token, "PUT", "/v1/apps/globex/shop", &shop).0, 403, "scoped to acme");
        assert_eq!(api("combd_guess", "GET", "/v1/apps/acme/shop", b"").0, 403);
        let t = Instant::now();
        let refused = {
            use std::io::{Read, Write};
            let s = std::net::TcpStream::connect(("127.0.0.1", admin_port)).unwrap();
            let conn = rustls::ClientConnection::new(tls.clone(), rustls::pki_types::ServerName::try_from("node0.combd").unwrap()).unwrap();
            let mut t = rustls::StreamOwned::new(conn, s);
            write!(t, "PUT /v1/apps/acme/shop HTTP/1.1\r\nauthorization: Bearer {weak}\r\ncontent-length: 60000000\r\n\r\n").unwrap();
            let mut out = vec![];
            let _ = t.read_to_end(&mut out);
            String::from_utf8_lossy(&out).into_owned()
        };
        assert!(refused.starts_with("HTTP/1.1 403") && t.elapsed() < Duration::from_secs(5), "refused before the 60 MB body: {refused:?} in {:?}", t.elapsed());
        assert_eq!(api(&token, "GET", "/v1/apps/acme/shop", b"").1["version"], v1.as_str());
        // A var set through the API reaches every node, checked against its type.
        assert_eq!(api(&token, "PUT", "/v1/apps/acme/shop/vars", br#"{"V": "changed"}"#).0, 200);
        everywhere("shop.test", "/env/V", "{\"value\":\"changed\"}");
        assert!(api(&token, "PUT", "/v1/apps/acme/shop/vars", br#"{"NOPE": "x"}"#).1["error"].as_str().unwrap().contains("not declared"));
        // A bad deploy (its manifest names another app, or a module is
        // missing) is refused, and the pointer stays.
        assert!(deploy("shop", &app("other", "shop.test", "[\"/env/*\"]", "V = \"x\"")).unwrap_err().contains("its manifest says it's \"other\""));
        assert_eq!(api(&token, "GET", "/v1/apps/acme/shop", b"").1["version"], v1.as_str());
        // v2: a new route; the object keeps its state.
        let v2 = deploy("shop", &app("shop", "shop.test", "[\"/env/*\", \"/hello\"]", "V = \"two\"")).unwrap();
        assert_ne!(v1, v2);
        everywhere("shop.test", "/hello", "{\"hits\":1}");
        everywhere("shop.test", "/room/r1/count", "{\"count\":9}");
        // Rollback to v1: /hello is gone again everywhere.
        assert_eq!(api(&token, "POST", "/v1/apps/acme/shop/rollback", serde_json::json!({ "version": v1 }).to_string().as_bytes()).0, 200);
        for &p in &nodes {
            assert!(eventually(10_000, || http_to(p, "shop.test", "GET", "/hello", "").0 == 404), "node {p}");
        }
        assert_eq!(api(&token, "POST", "/v1/apps/acme/shop/rollback", serde_json::json!({ "version": v2 }).to_string().as_bytes()).0, 200);
        // A second app, on its own domain.
        deploy("blog", &app("blog", "blog.test", "[\"/env/*\"]", "V = \"blog\"")).unwrap();
        everywhere("blog.test", "/env/V", "{\"value\":\"blog\"}");
        everywhere("shop.test", "/env/V", "{\"value\":\"changed\"}");
        // Secrets: set before the first deploy (nothing to check them against
        // yet), sealed in the catalog, and opened on every node for the app.
        let url1 = format!("https://127.0.0.1:{}", admin_ports[1]);
        let secret = |node: usize, t: &str, method: &str, name: &str, v: &str| match node {
            0 => api(t, method, &format!("/v1/apps/acme/vault/secrets/{name}"), v.as_bytes()),
            _ => admin::request(&url1, Some((&tls, "node1.combd")), t, method, &format!("/v1/apps/acme/vault/secrets/{name}"), v.as_bytes()).unwrap(),
        };
        assert_eq!(secret(0, &weak, "PUT", "api-key", "x").0, 403, "it takes the secrets scope");
        assert_eq!(secret(0, &token, "PUT", "api-key", "hunter2-hunter2").0, 200);
        deploy("vault", &app("vault", "vault.test", "[\"/env/*\"]", "V = \"v\"\n[secrets]\nK = \"api-key\"\n\"U?\" = { secret = \"hook\", type = \"url\" }")).unwrap_or_else(|e| panic!("{e}"));
        everywhere("vault.test", "/env/K", "{\"value\":\"hunter2-hunter2\"}");
        // Changed through another node: every node reloads with it.
        assert_eq!(secret(1, &token, "PUT", "api-key", "correct-horse").0, 200);
        everywhere("vault.test", "/env/K", "{\"value\":\"correct-horse\"}");
        // Checked against the live manifest, and errors never show a value.
        let e = secret(0, &token, "PUT", "hook", "not a url").1["error"].as_str().unwrap().to_string();
        assert!(e.contains("isn't a valid url") && !e.contains("not a url"), "{e}");
        assert!(secret(0, &token, "PUT", "nope", "x").1["error"].as_str().unwrap().contains("not declared"));
        assert!(secret(1, &token, "DELETE", "api-key", "").1["error"].as_str().unwrap().contains("K needs secret"));
        assert_eq!(secret(1, &token, "PUT", "hook", "https://hooks.test/x").0, 200);
        everywhere("vault.test", "/env/U", "{\"value\":\"https://hooks.test/x\"}");
        assert_eq!(secret(0, &token, "DELETE", "hook", "").0, 200, "an optional one can go");
        everywhere("vault.test", "/env/U", "{\"error\":\"not set\"}");
        // Never in the clear: not in the bucket, not in a node's copy.
        let has = |b: &[u8]| ["hunter2-hunter2", "correct-horse", "hooks.test"].iter().any(|s| b.windows(s.len()).any(|w| w == s.as_bytes()));
        for k in bucket.list("").unwrap() {
            assert!(!has(&bucket.get(&k).unwrap().unwrap().0), "{k}");
        }
        for n in 0..3 {
            assert!(!has(&std::fs::read(dir.0.join(format!("d{n}/catalog.snap"))).unwrap()), "node {n}'s copy");
        }
        // An app with a Postgres pool: deployed, its pool starts on every
        // node, and every node can query. Its URL is a secret.
        if let Ok(pg_url) = std::env::var("COMBD_PG_TEST") {
            let d = src.0.join("data");
            std::fs::create_dir_all(d.join("modules")).unwrap();
            std::fs::copy(GUEST_EV, manifest::module_path(&d, "ev")).unwrap();
            std::fs::write(d.join("combd.toml"), "[app]\nname = \"data\"\ndomains = [\"data.test\"]\n[secrets]\nPG = { secret = \"pg-url\", type = \"url\" }\n[http.api]\nmodule = \"ev\"\nroutes = [\"/pg/q\"]\n[pool.main]\nmodule = \"combd:postgres\"\nurl = \"$PG\"\ncallers = [\"api\"]\nmax = 2\n").unwrap();
            assert_eq!(api(&token, "PUT", "/v1/apps/acme/data/secrets/pg-url", pg_url.as_bytes()).0, 200);
            deploy("data", &deploy::bundle(&d).unwrap()).unwrap_or_else(|e| panic!("{e}"));
            for &p in &nodes {
                assert!(eventually(10_000, || http_to(p, "data.test", "POST", "/pg/q", "{\"q\":\"SELECT 41 + 1\"}").1.contains("[[42]]")), "node {p}: {:?}", http_to(p, "data.test", "POST", "/pg/q", "{\"q\":\"SELECT 41 + 1\"}"));
            }
        }
        // Two deploys at once, through two nodes, both claiming one domain:
        // the catalog's log gives them one order, and the second is checked
        // against the first. Exactly one wins, and every node agrees.
        let (bx, by) = (app("racex", "race.test", "[\"/env/*\"]", "V = \"x\""), app("racey", "race.test", "[\"/env/*\"]", "V = \"y\""));
        let (tls1, token1) = (tls.clone(), token.clone());
        let other = std::thread::spawn(move || admin::request(&url1, Some((&tls1, "node1.combd")), &token1, "PUT", "/v1/apps/acme/racey", &by).unwrap());
        let mine = api(&token, "PUT", "/v1/apps/acme/racex", &bx);
        let theirs = other.join().unwrap();
        assert_eq!([mine.0, theirs.0].iter().filter(|&&c| c == 200).count(), 1, "exactly one wins: {mine:?} {theirs:?}");
        let loser = if mine.0 == 200 { &theirs } else { &mine };
        assert!(loser.1["error"].as_str().unwrap().contains("domain race.test is already"), "{loser:?}");
        let want = if mine.0 == 200 { "{\"value\":\"x\"}" } else { "{\"value\":\"y\"}" };
        everywhere("race.test", "/env/V", want);
        // A second node 1 would fight the first over everything it owns:
        // its peers have been hearing the first one's heartbeats, so it must
        // see the id is taken (after waiting three leases), and not start.
        let twin = fleet::FleetCfg { node: 1, lease_ms: 1000, replicas: 2, s3: bucket.clone(), peers: peers.clone(), wire_port: 0, wire_certs: Some(certs.clone()), insecure: false, upload_ms: 0, upload_bytes: 1 << 20, read_lease: true, regions: HashMap::new(), place_ms: 0 };
        let t = Instant::now();
        assert!(http::node_id_free(&twin, 12345).unwrap_err().contains("node id 1 is already in use"));
        assert!(t.elapsed() >= Duration::from_secs(3), "it waits for a just-restarted node to go quiet first: {:?}", t.elapsed());
        // Taken down: gone everywhere; the other app stays.
        assert_eq!(api(&token, "DELETE", "/v1/apps/acme/shop", b"").0, 200);
        for &p in &nodes {
            assert!(eventually(10_000, || http_to(p, "shop.test", "GET", "/env/V", "").0 == 404), "node {p} still serves it");
        }
        everywhere("blog.test", "/env/V", "{\"value\":\"blog\"}");
    }

    const MILLION: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/million/app");

    /// Million's manifest, interpreted (it runs compiled; these tests are
    /// the interpreter's).
    fn million_toml() -> String {
        std::fs::read_to_string(format!("{MILLION}/combd.toml")).unwrap().replace("engine = \"compiled\"", "engine = \"interpreted\"")
    }

    /// The million checkboxes: a session (a `ws` process) per socket,
    /// asleep between events, its chunks' changes going straight to its
    /// socket without waking it, the count, and sessions that end. Asleep
    /// the app's way (it keeps its own state) and the host's (it keeps what
    /// the cell changed).
    #[test]
    fn million_one_session_per_socket() {
        million_sessions("app");
    }

    #[test]
    fn million_sessions_frozen_by_the_host() {
        million_sessions("auto");
    }

    fn million_sessions(sleep: &str) {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        // Asleep after every event, so every event wakes it.
        let m = million_toml().replace("hibernate_ms = 2000", "hibernate_ms = 0").replace("hibernate = \"app\"", &format!("hibernate = \"{sleep}\"")).replace("state_kb = 1", "state_kb = 16");
        std::fs::write(app.0.join("combd.toml"), m).unwrap();
        for f in ["million.wasm", "viewer.wasm", "web.wasm"] {
            std::fs::copy(format!("{MILLION}/modules/{f}"), app.0.join("modules").join(f)).expect("build it first: examples/million/build.sh");
        }
        let eng = interp::Interp { max_pages: 16384 };
        let (a, _) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(eng, cfg(&dir.0, 2), move |_| a);
        let host = "million.localhost";
        let json = |w: &mut Ws| serde_json::from_slice::<serde_json::Value>(&w.recv().1).unwrap();
        let bit = |hex: &str, i: usize| u8::from_str_radix(&hex[i / 8 * 2..i / 8 * 2 + 2], 16).unwrap() >> (i % 8) & 1;
        assert_eq!(http_to(port, host, "GET", "/ws", "").0, 426, "a ws route wants an upgrade");
        let (mut a, mut b) = (Ws::open_to(port, host, "/ws").unwrap(), Ws::open_to(port, host, "/ws").unwrap());
        for w in [&mut a, &mut b] {
            w.text(r#"{"t":"range","from":0,"to":500}"#);
            let all = json(w);
            assert_eq!((all["t"].as_str(), all["n"].as_u64(), all["bits"].as_str().map(str::len)), (Some("all"), Some(0), Some(3908)), "{all}");
            assert_eq!(bit(all["bits"].as_str().unwrap(), 7), 0);
        }
        assert!(eventually(2000, || stat(port, "sessions_live") == 2 && stat(port, "sessions_hibernated") == 2), "both asleep");
        // A set wakes its own viewer, not the other: the change goes straight to b's socket.
        let woken = stat(port, "sessions_woken");
        a.text(r#"{"t":"set","i":7,"v":true}"#);
        for w in [&mut a, &mut b] {
            assert_eq!(json(w), serde_json::json!({ "t": "set", "n": 0, "i": 7, "v": true }));
        }
        assert!(eventually(2000, || stat(port, "sessions_hibernated") == 2));
        assert_eq!(stat(port, "sessions_woken"), woken + 1, "only a woke");
        // A viewer that moves on hears nothing more from where it was.
        a.text(r#"{"t":"range","from":15625,"to":16000}"#);
        assert_eq!((json(&mut a)["n"].as_u64()), Some(1));
        b.text(r#"{"t":"set","i":9,"v":true}"#);
        assert_eq!(json(&mut b)["i"].as_u64(), Some(9));
        a.text(r#"{"t":"set","i":15626,"v":true}"#);
        assert_eq!(json(&mut a), serde_json::json!({ "t": "set", "n": 1, "i": 1, "v": true }), "not chunk 0's box 9");
        // Too many sets in a second: the rest are refused. 50 at once: even if
        // they straddle a second, at most 40 get in.
        for i in 100..150 {
            b.text(&format!(r#"{{"t":"set","i":{i},"v":true}}"#));
        }
        // Each comes back once: refused (`slow`), or as a change, alone or
        // in a batch with others (`sets`).
        let (mut slow, mut back) = (0, 0);
        while back < 50 {
            let m = json(&mut b);
            back += match m["t"].as_str() {
                Some("sets") => m["c"].as_array().unwrap().len(),
                Some("slow") => {
                    slow += 1;
                    1
                }
                _ => 1,
            };
        }
        assert!(slow >= 10, "{slow} refused");
        // The count: each chunk tells `boxes` at most once a second.
        let mut c = Ws::open_to(port, host, "/ws").unwrap();
        c.text(r#"{"t":"range","from":0,"to":31250}"#);
        let ones: usize = (0..2).map(|_| { let all = json(&mut c); (0..15625).map(|i| bit(all["bits"].as_str().unwrap(), i) as usize).sum::<usize>() }).sum();
        assert_eq!(ones, 2 + 50 - slow as usize + 1, "boxes 7 and 9, the sets that got in, and 15626");
        assert!(eventually(4000, || http_to(port, host, "GET", "/api/count", "").1 == format!("{{\"checked\":{ones}}}")), "{}", http_to(port, host, "GET", "/api/count", "").1);
        // A closed socket ends its session, unwoken (`on_close = "drop"`).
        let woken = stat(port, "sessions_woken");
        drop(a);
        assert!(eventually(3000, || stat(port, "sessions_live") == 2), "{}", stat(port, "sessions_live"));
        assert_eq!(stat(port, "sessions_woken"), woken, "nobody woke to hear it");
    }

    /// `max_awake`: no more cells at work than that. Twelve sleeping
    /// viewers asked at once, with room for one: each waits its turn,
    /// asleep, and every one is answered.
    #[test]
    fn sleeping_sessions_wake_in_turn_under_max_awake() {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        let m = million_toml().replace("hibernate_ms = 2000", "hibernate_ms = 0").replace("hibernate = \"app\"", "hibernate = \"auto\"").replace("state_kb = 1", "state_kb = 16");
        std::fs::write(app.0.join("combd.toml"), m).unwrap();
        for f in ["million.wasm", "viewer.wasm", "web.wasm"] {
            std::fs::copy(format!("{MILLION}/modules/{f}"), app.0.join("modules").join(f)).expect("build it first: examples/million/build.sh");
        }
        let eng = interp::Interp { max_pages: 16384 };
        let (a, _) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(eng, http::Config { max_awake: 1, ..cfg(&dir.0, 1) }, move |_| a);
        let host = "million.localhost";
        let mut socks: Vec<Ws> = (0..12).map(|_| Ws::open_to(port, host, "/ws").unwrap()).collect();
        for w in &mut socks {
            w.text(r#"{"t":"range","from":0,"to":500}"#);
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&w.recv().1).unwrap()["t"], "all");
        }
        assert!(eventually(3000, || stat(port, "sessions_hibernated") == 12), "all asleep");
        for w in &mut socks {
            w.text(r#"{"t":"range","from":15625,"to":16000}"#);
        }
        for w in &mut socks {
            let m = serde_json::from_slice::<serde_json::Value>(&w.recv().1).unwrap();
            assert_eq!((m["t"].as_str(), m["n"].as_u64()), (Some("all"), Some(1)), "{m}");
        }
        assert!(eventually(3000, || stat(port, "sessions_hibernated") == 12 && stat(port, "sessions_ready") == 0), "all asleep again, none waiting");
        assert!(stat(port, "sessions_waited") > 0, "and some had to wait their turn");
    }

    /// `memory_kb` under what a module starts with could never start a cell:
    /// the app is refused when it loads (or deploys), with why.
    #[test]
    fn a_memory_cap_under_the_module_is_refused() {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        let m = million_toml().replace("memory_kb = 512", "memory_kb = 64");
        std::fs::write(app.0.join("combd.toml"), m).unwrap();
        for f in ["million.wasm", "viewer.wasm", "web.wasm"] {
            std::fs::copy(format!("{MILLION}/modules/{f}"), app.0.join("modules").join(f)).expect("build it first: examples/million/build.sh");
        }
        let e = http::load_app(&interp::Interp { max_pages: 16384 }, &app.0).err().expect("refused");
        assert!(e.contains("million/viewer: memory_kb 64 is under this module's starting memory (128 KB)"), "{e}");
    }

    /// The million checkboxes on three nodes: viewers on different nodes
    /// watch one chunk; a set on one reaches the others' sockets (straight
    /// from the chunk's node), and the count adds up on any node.
    #[test]
    fn million_across_a_fleet() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::write(app.0.join("combd.toml"), million_toml()).unwrap();
        for f in ["million.wasm", "viewer.wasm", "web.wasm"] {
            std::fs::copy(format!("{MILLION}/modules/{f}"), app.0.join("modules").join(f)).expect("build it first: examples/million/build.sh");
        }
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 3).unwrap();
        let bucket = test_bucket();
        let wports = free_ports(3);
        let peers: HashMap<own::Node, String> = (0..3).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        let nodes: Vec<u16> = (0..3u32)
            .map(|n| {
                let fc = fleet::FleetCfg { node: n, lease_ms: 3000, replicas: 2, s3: s3::S3 { bucket: bucket.clone(), ..base.clone() }, peers: peers.clone(), wire_port: wports[n as usize], wire_certs: Some(dir.0.join("certs")), insecure: false, upload_ms: 0, upload_bytes: 1 << 20, read_lease: true, regions: HashMap::new(), place_ms: 0 };
                let eng = interp::Interp { max_pages: 16384 };
                let (a, pg) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
                let c = http::Config { pg, fleet: Some(fc), ..cfg(&dir.0.join(format!("d{n}")), 1) };
                server_app(eng, c, move |_| a).0
            })
            .collect();
        let host = "million.localhost";
        let json = |w: &mut Ws| serde_json::from_slice::<serde_json::Value>(&w.recv().1).unwrap();
        // One viewer per node, all on chunk 0. The first may wait for the
        // fleet to settle (a fresh object's first claim).
        let mut ws: Vec<Ws> = nodes.iter().map(|&p| Ws::open_to(p, host, "/ws").unwrap()).collect();
        for w in ws.iter_mut() {
            w.0.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            w.text(r#"{"t":"range","from":0,"to":500}"#);
            assert_eq!(json(w)["t"], "all");
        }
        for (k, i) in [(0usize, 3u64), (1, 4), (2, 5)] {
            ws[k].text(&format!(r#"{{"t":"set","i":{i},"v":true}}"#));
            for w in ws.iter_mut() {
                assert_eq!(json(w), serde_json::json!({ "t": "set", "n": 0, "i": i, "v": true }), "node {k}'s set, everywhere");
            }
        }
        for &p in &nodes {
            assert!(eventually(6000, || http_to(p, host, "GET", "/api/count", "").1 == "{\"checked\":3}"), "node {p}: {}", http_to(p, host, "GET", "/api/count", "").1);
        }
        assert!(eventually(6000, || nodes.iter().all(|&p| stat(p, "sessions_hibernated") == 1)), "each node's viewer asleep (hibernate_ms = 2000)");
    }

    const ARENA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/arena/app");

    /// The arena example, a whole match through combd: the page, matchmaking,
    /// a match ticking 20 times a second over WebSockets, sight blocked by
    /// walls, a pause and a resume, a drop and a reconnect, a drop for good
    /// (the other player wins), and both players' records.
    /// A deploy in the middle of a game doesn't end it: the match (open
    /// sockets, a beat) finishes on the version it started on, while new
    /// requests get the new one.
    #[test]
    fn a_deploy_never_ends_a_game_in_play() {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::copy(format!("{ARENA}/combd.toml"), app.0.join("combd.toml")).unwrap();
        std::fs::copy(format!("{ARENA}/modules/game.wasm"), app.0.join("modules/game.wasm")).expect("build the arena first: examples/arena/build.sh");
        let eng = interp::Interp { max_pages: 16384 };
        let (a, _) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let live = Arc::new(http::Live::new(a));
        let dir = store::tempdir::Dir::new();
        let port = free_ports(1)[0];
        let (l2, c) = (live.clone(), cfg(&dir.0, 2));
        std::thread::spawn(move || http::serve_live(&interp::Interp { max_pages: 16384 }, &l2, port, c));
        assert!(eventually(2000, || std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()));
        // A match against the match's own bot.
        let (code, b) = http(port, "POST", "/api/bot", &serde_json::json!({ "player": "alice", "rtt": { "0": 5 } }).to_string());
        assert_eq!(code, 200, "{b}");
        let m: serde_json::Value = serde_json::from_str(&b).unwrap();
        let mut ws = Ws::open(port, &format!("/match/{}/ws", m["match"].as_str().unwrap())).unwrap();
        ws.text(&serde_json::json!({ "t": "hello", "token": m["token"] }).to_string());
        // The last state seen within `ms`, and that it kept coming.
        let mut states = |ms: u64| -> serde_json::Value {
            let (t, mut last) = (Instant::now(), serde_json::Value::Null);
            while t.elapsed() < Duration::from_millis(ms) {
                if let Some((1, b)) = ws.recv_within(300) {
                    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
                    if v["t"] == "st" {
                        last = v;
                    }
                }
            }
            last
        };
        let before = states(4500);
        assert_eq!(before["phase"], "play", "{before}");
        // A new version, deployed mid-game (a var changed: new processes).
        std::fs::write(app.0.join("combd.dev.toml"), "[vars]\nMATCH_MS = 170000\n").unwrap();
        http::deploy_dir(&eng, &live, "local", &app.0).unwrap();
        // Well after the deploy is in (a second), it still goes on.
        let mid = states(1000);
        let after = states(1500);
        assert!(after["k"].as_u64() > mid["k"].as_u64() && mid["k"].as_u64() > before["k"].as_u64() && after["phase"] == "play", "the match went on, on its old version: {before} then {mid} then {after}");
        // New requests: the new version.
        assert_eq!(http(port, "GET", "/api/stats", "").0, 200);
    }

    /// A restore's segments come back in order, many at once, and a missing
    /// one fails it.
    #[test]
    fn segments_fetch_in_order() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let s3 = s3::S3 { bucket: test_bucket(), ..base };
        s3.make_bucket().unwrap();
        let keys: Vec<String> = (0..40).map(|i| format!("obj/test/s{i:04}")).collect();
        for (i, k) in keys.iter().enumerate() {
            let ents = vec![vec![i as u8], vec![i as u8, 0xff]];
            s3.put(k, &own::frame(&ents), &s3::Cond::Any).unwrap().unwrap();
        }
        let got = fleet::fetch(&s3, &keys).unwrap();
        let want: Vec<Vec<u8>> = (0..40u8).flat_map(|i| [vec![i], vec![i, 0xff]]).collect();
        assert_eq!(got, want);
        let mut gap = keys.clone();
        gap.insert(17, "obj/test/missing".into());
        assert!(fleet::fetch(&s3, &gap).unwrap_err().contains("missing"));
    }

    /// A match in play on a fleet (two peers per object) answers its
    /// messages under the read lease: the game writes nothing, so a ping
    /// mustn't wait for a round to the peers (an ocean away, in the arena).
    #[test]
    fn a_match_in_play_reads_under_the_lease() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test fleet mode against a real bucket");
            return;
        };
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::copy(format!("{ARENA}/combd.toml"), app.0.join("combd.toml")).unwrap();
        std::fs::copy(format!("{ARENA}/modules/game.wasm"), app.0.join("modules/game.wasm")).expect("build the arena first: examples/arena/build.sh");
        let dir = store::tempdir::Dir::new();
        wire::make_certs(&dir.0.join("certs"), 3).unwrap();
        let bucket = test_bucket();
        let wports = free_ports(3);
        let peers: HashMap<own::Node, String> = (0..3).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        let nodes: Vec<u16> = (0..3u32)
            .map(|n| {
                let fc = fleet::FleetCfg { node: n, lease_ms: 3000, replicas: 2, s3: s3::S3 { bucket: bucket.clone(), ..base.clone() }, peers: peers.clone(), wire_port: wports[n as usize], wire_certs: Some(dir.0.join("certs")), insecure: false, upload_ms: 0, upload_bytes: 1 << 20, read_lease: true, regions: HashMap::new(), place_ms: 0 };
                let eng = interp::Interp { max_pages: 16384 };
                let (a, pg) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
                let c = http::Config { pg, fleet: Some(fc), ..cfg(&dir.0.join(format!("d{n}")), 1) };
                server_app(eng, c, move |_| a).0
            })
            .collect();
        for &p in &nodes {
            stat(p, "fences");
        }
        let p = nodes[0];
        let b = (0..100).find_map(|_| match http(p, "POST", "/api/bot", &serde_json::json!({ "player": "alice", "rtt": { "0": 5 } }).to_string()) {
            (200, b) => Some(b),
            _ => {
                std::thread::sleep(Duration::from_millis(100));
                None
            }
        });
        let m: serde_json::Value = serde_json::from_str(&b.expect("a bot match")).unwrap();
        assert_eq!(m["node"], 0, "{m}");
        let mut ws = Ws::open(p, &format!("/match/{}/ws", m["match"].as_str().unwrap())).unwrap();
        ws.text(&serde_json::json!({ "t": "hello", "token": m["token"] }).to_string());
        // Into play (the countdown is 3 s), then count while it plays.
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(4000) {
            ws.recv_within(100);
        }
        let (acks0, leases0, msgs0) = (stat(p, "peer_acks"), stat(p, "lease_reads"), stat(p, "ws_messages"));
        let t = Instant::now();
        let mut i = 0;
        while t.elapsed() < Duration::from_millis(3000) {
            ws.text(&serde_json::json!({ "t": "in", "mx": 1.0, "my": 0.0, "aim": 0.0, "fire": i % 7 == 0 }).to_string());
            i += 1;
            ws.recv_within(20);
        }
        let (acks, leases, msgs) = (stat(p, "peer_acks") - acks0, stat(p, "lease_reads") - leases0, stat(p, "ws_messages") - msgs0);
        eprintln!("in 3 s of play: {msgs} messages, {leases} lease reads, {acks} peer acks");
        assert!(msgs > 50, "{msgs} messages");
        // A round renews the lease about twice a second: a few acks a second, not one per message.
        assert!(acks < msgs / 4, "{acks} peer acks for {msgs} messages ({leases} lease reads): the messages waited for rounds");
    }

    #[test]
    fn arena_plays_a_match() {
        let app = store::tempdir::Dir::new();
        std::fs::create_dir(app.0.join("modules")).unwrap();
        std::fs::copy(format!("{ARENA}/combd.toml"), app.0.join("combd.toml")).unwrap();
        std::fs::copy(format!("{ARENA}/modules/game.wasm"), app.0.join("modules/game.wasm")).expect("build the arena first: examples/arena/build.sh");
        // A short pause limit, so the test doesn't wait 30 s.
        std::fs::write(app.0.join("combd.dev.toml"), "[vars]\nPAUSE_MS = 800\n").unwrap();
        let eng = interp::Interp { max_pages: 16384 };
        let (a, pg) = http::load_app(&eng, &app.0).unwrap_or_else(|e| panic!("{e}"));
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = server_app(eng, http::Config { pg, ..cfg(&dir.0, 2) }, move |_| a);
        // The page is HTML; a ping says anyone may time it.
        let raw = |path: &str| {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        let page = raw("/");
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("content-type: text/html") && page.contains("<canvas"), "{}", &page[..page.len().min(300)]);
        assert!(raw("/_combd/ping").contains("access-control-allow-origin: *"));
        // Two players join and are paired.
        let post = |path: &str, body: serde_json::Value| -> serde_json::Value {
            let (code, b) = http(port, "POST", path, &body.to_string());
            assert_eq!(code, 200, "{path}: {b}");
            serde_json::from_str(&b).unwrap()
        };
        // A page warms a match on its nodes first (node ids past any real
        // fleet, and junk, are ignored); this match then starts on it.
        post("/api/warm", serde_json::json!({ "rtt": { "0": 5, "99": 1, "x": 3 } }));
        let ta = post("/api/join", serde_json::json!({ "player": "alice", "rtt": { "0": 5 } }))["ticket"].as_str().unwrap().to_string();
        let tb = post("/api/join", serde_json::json!({ "player": "bob", "rtt": { "0": 7 } }))["ticket"].as_str().unwrap().to_string();
        let (ma, mb) = (post("/api/poll", serde_json::json!({ "ticket": ta })), post("/api/poll", serde_json::json!({ "ticket": tb })));
        assert_eq!(ma["match"], mb["match"], "{ma} {mb}");
        assert_eq!((ma["side"].as_u64(), mb["side"].as_u64(), ma["node"].as_u64()), (Some(0), Some(1), Some(0)));
        let path = format!("/match/{}/ws", ma["match"].as_str().unwrap());
        let join = |token: &serde_json::Value| -> Ws {
            let mut ws = Ws::open(port, &path).unwrap();
            ws.text(&serde_json::json!({ "t": "hello", "token": token }).to_string());
            ws
        };
        // Read states until one passes `ok` (or give up).
        let until = |ws: &mut Ws, ms: u64, ok: &dyn Fn(&serde_json::Value) -> bool| -> serde_json::Value {
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(ms) {
                let Some((1, b)) = ws.recv_within(500) else { continue };
                let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
                if v["t"] == "st" && ok(&v) {
                    return v;
                }
            }
            panic!("no such state within {ms} ms");
        };
        let (mut a, mut b) = (join(&ma["token"]), join(&mb["token"]));
        // 3, 2, 1, then play.
        let cd = until(&mut a, 3000, &|v| v["phase"] == "countdown");
        assert!(cd["left"].as_u64().unwrap() <= 3000, "{cd}");
        let first = until(&mut a, 6000, &|v| v["phase"] == "play");
        assert!(first["them"].is_null(), "the spawns can't see each other through the middle wall: {first}");
        // 20 ticks a second.
        let t = Instant::now();
        let k0 = first["k"].as_u64().unwrap();
        let k1 = until(&mut a, 3000, &|v| v["k"].as_u64().unwrap() >= k0 + 10)["k"].as_u64().unwrap();
        assert!(t.elapsed() < Duration::from_millis(1500), "10 ticks took {:?}", t.elapsed());
        let _ = k1;
        // A pause and a resume, by alice.
        a.text(r#"{"t":"pause"}"#);
        let v = until(&mut b, 2000, &|v| v["phase"] == "paused");
        assert_eq!(v["paused_by_you"], false);
        a.text(r#"{"t":"resume"}"#);
        until(&mut b, 2000, &|v| v["phase"] == "countdown");
        until(&mut b, 5000, &|v| v["phase"] == "play");
        // Bob drops: paused. He's back: it goes on.
        drop(b);
        until(&mut a, 2000, &|v| v["phase"] == "paused");
        let mut b = join(&mb["token"]);
        until(&mut a, 5000, &|v| v["phase"] == "play");
        until(&mut b, 5000, &|v| v["phase"] == "play");
        // Bob drops for good: alice wins once the pause runs out.
        drop(b);
        let over = until(&mut a, 5000, &|v| v["phase"] == "over");
        assert_eq!(over["winner"], true, "{over}");
        assert!(eventually(3000, || http(port, "GET", "/api/player/alice", "").1 == "{\"draws\":0,\"losses\":0,\"wins\":1}"), "{:?}", http(port, "GET", "/api/player/alice", ""));
        assert!(eventually(3000, || http(port, "GET", "/api/player/bob", "").1 == "{\"draws\":0,\"losses\":1,\"wins\":0}"));
        let stats: serde_json::Value = serde_json::from_str(&http(port, "GET", "/api/stats", "").1).unwrap();
        assert_eq!(stats["matches"], 1, "{stats}");
    }

    /// A pool is one for the whole fleet: three nodes, `max = 2`, every node
    /// running slow queries at once, and Postgres never sees more than two
    /// connections. A node with no member of its own uses the nearest free
    /// one, over the wire. Then the manager's node stops mid-query: the
    /// query still finishes (`finish`), another node takes over, and every
    /// node still gets answers.
    /// Three nodes serving an app with a Postgres pool of `max`, on its own
    /// application name (to count its connections). `None`: no bucket or
    /// database to test against.
    struct PgFleet {
        nodes: Vec<u16>,
        stops: Vec<Arc<AtomicBool>>,
        pg_url: String,
        tag: String,
        _dirs: (store::tempdir::Dir, store::tempdir::Dir),
    }

    fn pg_fleet(max: usize) -> Option<PgFleet> {
        let (Some(base), Ok(pg_url)) = (s3::S3::from_env(), std::env::var("COMBD_PG_TEST")) else {
            eprintln!("skipping: set COMBD_S3 and COMBD_PG_TEST");
            return None;
        };
        let dir = store::tempdir::Dir::new();
        let certs = dir.0.join("certs");
        wire::make_certs(&certs, 3).unwrap();
        let bucket = s3::S3 { bucket: test_bucket(), ..base.clone() };
        let wports = free_ports(3);
        let peers: HashMap<own::Node, String> = (0..3).map(|n| (n, format!("127.0.0.1:{}", wports[n as usize]))).collect();
        let (token, line) = admin::new_token(&["acme/*:*"]);
        let admin_port = free_ports(1)[0];
        let stops: Vec<Arc<AtomicBool>> = (0..3).map(|_| Arc::default()).collect();
        let nodes: Vec<u16> = (0..3u32)
            .map(|n| {
                let fc = fleet::FleetCfg { node: n, lease_ms: 1000, replicas: 2, s3: bucket.clone(), peers: peers.clone(), wire_port: wports[n as usize], wire_certs: Some(certs.clone()), insecure: false, upload_ms: 0, upload_bytes: 1 << 20, read_lease: true, regions: HashMap::new(), place_ms: 0 };
                let c = http::Config { fleet: Some(fc), stop: stops[n as usize].clone(), ..cfg(&dir.0.join(format!("d{n}")), 2) };
                let admin = (n == 0).then(|| admin::AdminCfg { port: admin_port, public: false, tls: None, tokens: admin::parse_tokens(&line).unwrap() });
                let port = free_ports(1)[0];
                std::thread::spawn(move || http::serve_fleet_apps(&interp::Interp { max_pages: 16384 }, port, c, admin).unwrap());
                port
            })
            .collect();
        assert!(eventually(10_000, || std::net::TcpStream::connect(("127.0.0.1", admin_port)).is_ok()));
        for &p in &nodes {
            assert!(eventually(10_000, || std::net::TcpStream::connect(("127.0.0.1", p)).is_ok()));
            stat(p, "fences");
        }
        let tag = format!("combd-fleet-pool-{}-{}", std::process::id(), admin_port);
        let url = format!("{pg_url}{}application_name={tag}", if pg_url.contains('?') { "&" } else { "?" });
        let src = store::tempdir::Dir::new();
        std::fs::create_dir_all(src.0.join("modules")).unwrap();
        std::fs::copy(GUEST_EV, manifest::module_path(&src.0, "ev")).unwrap();
        std::fs::write(src.0.join("combd.toml"), format!("[app]\nname = \"data\"\ndomains = [\"data.test\"]\n[vars]\nPG = {{ type = \"url\" }}\n[http.api]\nmodule = \"ev\"\nroutes = [\"/pg/q\", \"/hello\"]\n[pool.main]\nmodule = \"combd:postgres\"\nurl = \"$PG\"\ncallers = [\"api\"]\nmax = {max}\nwait_ms = 20000\n")).unwrap();
        let api = |method: &str, path: &str, body: &[u8]| admin::request(&format!("http://127.0.0.1:{admin_port}"), None, &token, method, path, body).unwrap();
        assert_eq!(api("PUT", "/v1/apps/acme/data/vars", serde_json::json!({ "PG": url }).to_string().as_bytes()).0, 200);
        let (code, v) = api("PUT", "/v1/apps/acme/data", &deploy::bundle(&src.0).unwrap());
        assert_eq!(code, 200, "{v}");
        // Node 0 asks first, so it's the manager.
        for &p in &nodes {
            assert!(eventually(10_000, || pg_q(p, "SELECT 1", serde_json::json!([])).0 == 200), "node {p}: {:?}", pg_q(p, "SELECT 1", serde_json::json!([])));
        }
        Some(PgFleet { nodes, stops, pg_url, tag, _dirs: (dir, src) })
    }

    fn pg_q(port: u16, sql: &str, params: serde_json::Value) -> (u16, String) {
        http_to(port, "data.test", "POST", "/pg/q", &serde_json::json!({ "q": sql, "p": params }).to_string())
    }

    /// Load for `secs` from `threads` threads, each running `op` (true: it
    /// worked): (ops a second, p50 ms, p99 ms, failures).
    fn load(secs: f64, threads: usize, op: impl Fn(usize, &mut sim::Rng) -> bool + Sync) -> (f64, f64, f64, usize) {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        let (lat, bad) = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..threads)
                .map(|t| {
                    let op = &op;
                    sc.spawn(move || {
                        let (mut lat, mut bad, mut rng) = (vec![], 0, sim::Rng::new(t as u64 + 1));
                        while Instant::now() < end {
                            let s = Instant::now();
                            if op(t, &mut rng) {
                                lat.push(s.elapsed().as_secs_f64() * 1000.0);
                            } else {
                                bad += 1;
                            }
                        }
                        (lat, bad)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).fold((vec![], 0), |(mut l, b), (l2, b2)| {
                l.extend(l2);
                (l, b + b2)
            })
        });
        let mut lat = lat;
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| lat.get(((lat.len() as f64 * p) as usize).min(lat.len().saturating_sub(1))).copied().unwrap_or(0.0);
        (lat.len() as f64 / secs, pct(0.5), pct(0.99), bad)
    }

    /// How fast Postgres goes through combd, next to Postgres itself and to
    /// plain HTTP: reads, inserts, and updates by primary key, on the
    /// manager's node, on another node, and on all three. Not run by
    /// default: `cargo test --release -- --ignored pg_bench --nocapture`.
    #[test]
    #[ignore]
    fn pg_bench() {
        const MAX: usize = 8;
        let threads: usize = std::env::var("PG_BENCH_CLIENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
        const SECS: f64 = 4.0;
        let Some(f) = pg_fleet(MAX) else { return };
        let table = format!("bench_{}", std::process::id());
        let mut c = postgres::Client::connect(&f.pg_url, postgres::NoTls).unwrap();
        c.batch_execute(&format!("DROP TABLE IF EXISTS {table}; CREATE TABLE {table} (id serial PRIMARY KEY, n int NOT NULL, s text NOT NULL); INSERT INTO {table} (n, s) SELECT i, 'row ' || i FROM generate_series(1, 10000) i;")).unwrap();
        let get = format!("SELECT n, s FROM {table} WHERE id = $1");
        let ins = format!("INSERT INTO {table} (n, s) VALUES ($1, $2)");
        let upd = format!("UPDATE {table} SET n = n + 1 WHERE id = $1");
        let report = |name: &str, (ops, p50, p99, bad): (f64, f64, f64, usize)| eprintln!("{name:<44} {ops:>8.0} ops/s   p50 {p50:>6.2} ms   p99 {p99:>6.2} ms{}", if bad > 0 { format!("   {bad} failed") } else { String::new() });
        eprintln!("--- {threads} clients, a pool of {MAX}, {SECS} s each, 10,000 rows");
        // Plain HTTP to a cell, no database: what the web side costs.
        report("HTTP only (GET /hello), node 0", load(SECS, threads, |_, _| http_to(f.nodes[0], "data.test", "GET", "/hello", "").0 == 200));
        // Postgres itself: one connection per thread, no HTTP.
        for (name, sql) in [("get", &get), ("insert", &ins), ("update", &upd)] {
            let conns: Vec<_> = (0..MAX).map(|_| std::sync::Mutex::new(postgres::Client::connect(&f.pg_url, postgres::NoTls).unwrap())).collect();
            let r = load(SECS, MAX, |t, rng| {
                let mut c = conns[t].lock().unwrap();
                let id = 1 + rng.below(10_000) as i32;
                match name {
                    "insert" => c.execute(sql.as_str(), &[&id, &"new"]).is_ok(),
                    _ => if name == "get" { c.query(sql.as_str(), &[&id]).is_ok() } else { c.execute(sql.as_str(), &[&id]).is_ok() },
                }
            });
            report(&format!("Postgres direct, {MAX} connections: {name}"), r);
        }
        // One round trip, the floor under everything: one connection, then eight.
        for n in [1, MAX] {
            let conns: Vec<_> = (0..n).map(|_| std::sync::Mutex::new(postgres::Client::connect(&f.pg_url, postgres::NoTls).unwrap())).collect();
            report(&format!("Postgres round trip (SELECT 1), {n} connection(s)"), load(SECS, n, |t, _| conns[t].lock().unwrap().simple_query("SELECT 1").is_ok()));
        }
        // Through combd.
        let op = |name: &'static str, sql: String| {
            move |port: u16, rng: &mut sim::Rng| {
                let id = 1 + rng.below(10_000);
                let p = if name == "insert" { serde_json::json!([id, "new"]) } else { serde_json::json!([id]) };
                pg_q(port, &sql, p).0 == 200
            }
        };
        for (name, sql) in [("get", get.clone()), ("insert", ins.clone()), ("update", upd.clone())] {
            let o = op(name, sql);
            report(&format!("combd, manager's node: {name}"), load(SECS, threads, |_, rng| o(f.nodes[0], rng)));
            report(&format!("combd, another node: {name}"), load(SECS, threads, |_, rng| o(f.nodes[1], rng)));
            report(&format!("combd, all three nodes: {name}"), load(SECS, threads, |t, rng| o(f.nodes[t % 3], rng)));
        }
        let n: i64 = c.query_one(&format!("SELECT count(*) FROM {table}"), &[]).unwrap().get(0);
        eprintln!("--- {n} rows at the end");
        c.batch_execute(&format!("DROP TABLE {table}")).unwrap();
        for s in &f.stops {
            s.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn a_pool_is_one_for_the_whole_fleet() {
        let Some(PgFleet { nodes, stops, pg_url, tag, _dirs }) = pg_fleet(2) else { return };
        let q = |port: u16, sql: &str| pg_q(port, sql, serde_json::json!([]));
        // How many connections Postgres sees from this pool, sampled.
        let count = {
            let tag = tag.clone();
            let mut c = postgres::Client::connect(&pg_url, postgres::NoTls).unwrap();
            move || -> i64 { c.query_one("SELECT count(*) FROM pg_stat_activity WHERE application_name = $1", &[&tag]).unwrap().get(0) }
        };
        let most = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let sampling = Arc::new(AtomicBool::new(true));
        let sampler = {
            let (most, sampling) = (most.clone(), sampling.clone());
            let mut count = count;
            std::thread::spawn(move || {
                while sampling.load(std::sync::atomic::Ordering::Relaxed) {
                    most.fetch_max(count(), std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(20));
                }
                count
            })
        };
        // Four slow queries per node, all at once.
        let t = Instant::now();
        let runs: Vec<_> = nodes.iter().flat_map(|&p| (0..4).map(move |_| std::thread::spawn(move || q(p, "SELECT pg_sleep(0.3)")))).collect();
        for r in runs {
            let (code, body) = r.join().unwrap();
            assert_eq!(code, 200, "{body}");
        }
        eprintln!("12 queries of 0.3 s on 2 connections across 3 nodes: {:?}", t.elapsed());
        assert!(t.elapsed() >= Duration::from_millis(1700), "two at a time: {:?}", t.elapsed());
        assert!(most.load(std::sync::atomic::Ordering::Relaxed) <= 2, "Postgres saw {} connections", most.load(std::sync::atomic::Ordering::Relaxed));
        // The manager is the node that asked first: node 0. A long query on
        // node 1 runs while node 0 stops; it finishes, and the others carry on.
        let long = std::thread::spawn({
            let p = nodes[1];
            move || q(p, "SELECT pg_sleep(2), 7")
        });
        std::thread::sleep(Duration::from_millis(300));
        stops[0].store(true, std::sync::atomic::Ordering::Relaxed);
        let (code, body) = long.join().unwrap();
        assert!(code == 200 && body.contains("7"), "the query on an older manager's member finishes: {code} {body}");
        for &p in &nodes[1..] {
            assert!(eventually(10_000, || q(p, "SELECT 2").0 == 200), "node {p} after the takeover: {:?}", q(p, "SELECT 2"));
        }
        let runs: Vec<_> = nodes[1..].iter().flat_map(|&p| (0..3).map(move |_| std::thread::spawn(move || q(p, "SELECT pg_sleep(0.2)")))).collect();
        for r in runs {
            assert_eq!(r.join().unwrap().0, 200);
        }
        sampling.store(false, std::sync::atomic::Ordering::Relaxed);
        let count = std::sync::Mutex::new(sampler.join().unwrap());
        // Members of the old manager go; at most `max` stay.
        assert!(eventually(10_000, || count.lock().unwrap()() <= 2), "{} connections left", count.lock().unwrap()());
        for s in &stops {
            s.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The catalog on a real bucket: 250 changes snapshot and trim the log,
    /// and a node starting with no copy at all catches up to the same state
    /// (from the snapshot, then the slots after it).
    #[test]
    fn the_catalog_snapshots_trims_and_catches_up() {
        let Some(base) = s3::S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test the catalog against a real bucket");
            return;
        };
        let bucket = s3::S3 { bucket: test_bucket(), ..base };
        bucket.make_bucket().unwrap();
        let dir = store::tempdir::Dir::new();
        let store: Arc<dyn kv::Store> = Arc::new(bucket.clone());
        let a = catnode::CatNode::start(store.clone(), dir.0.join("a.snap"), vec![]);
        assert!(a.ready(Duration::from_secs(10)));
        for i in 0..250 {
            a.submit(&format!("acme/app{}", i % 7), format!("{{\"version\":\"v{i}\",\"domains\":[\"d{}\"]}}", i % 7).into_bytes()).unwrap();
        }
        // Two apps on one domain: refused.
        assert!(a.submit("acme/other", b"{\"version\":\"v\",\"domains\":[\"d3\"]}".to_vec()).unwrap_err().contains("domain d3 is already acme/app3's"));
        // An app's data key is written once, and never deleted.
        a.submit("acme/app1#key", b"sealed".to_vec()).unwrap();
        assert!(a.submit("acme/app1#key", b"another".to_vec()).unwrap_err().contains("written once"));
        assert!(a.submit("acme/app1#key", vec![]).unwrap_err().contains("written once"));
        assert_eq!(a.now().0.apps["acme/app1#key"], b"sealed", "submit returned, so the copy has it");
        let (sa, _) = a.now();
        assert_eq!(sa.at, 251);
        // The snapshot and the trim run in the background: wait for them.
        assert!(eventually(10_000, || bucket.list("catalog/log/").unwrap().len() < 200), "the log was never trimmed: {} slots", bucket.list("catalog/log/").unwrap().len());
        assert!(bucket.get(catalog::SNAP_KEY).unwrap().is_some());
        // A new node, nothing on its disk.
        let b = catnode::CatNode::start(store, dir.0.join("b.snap"), vec![]);
        assert!(b.ready(Duration::from_secs(10)));
        assert_eq!(b.now().0, sa, "the new node has exactly the same catalog");
        // And its copy is on its disk now, for a fast restart.
        assert!(dir.0.join("b.snap").exists());
    }

    #[test]
    fn apps_must_not_share_domains() {
        let root = store::tempdir::Dir::new();
        let mk = |org: &str, app: &str, domains: &str| {
            let d = root.0.join(org).join(app);
            std::fs::create_dir_all(d.join("modules")).unwrap();
            std::fs::copy(GUEST_EV, manifest::module_path(&d, "ev")).unwrap();
            std::fs::write(d.join("combd.toml"), format!("[app]\nname = \"{app}\"\n{domains}\n[http.api]\nmodule = \"ev\"\nroutes = [\"/*\"]\n")).unwrap();
            (org.to_string(), d)
        };
        let eng = interp::Interp { max_pages: 16384 };
        let two = [mk("a", "x", "domains = [\"same.test\"]"), mk("b", "y", "domains = [\"same.test\"]")];
        let Err(e) = http::load_apps(&eng, &two) else { panic!("two apps on one domain") };
        assert!(e.contains("domain same.test is already a/x's"), "{e}");
        let nodomain = [mk("c", "x", "domains = [\"c.test\"]"), mk("d", "y", "")];
        let Err(e) = http::load_apps(&eng, &nodomain) else { panic!("an app with no domains, among others") };
        assert!(e.contains("d/y: with more than one app, each one names its domains"), "{e}");
    }

    #[test]
    fn an_app_of_processes_on_the_interpreter() {
        an_app_of_processes(interp::Interp { max_pages: 16384 });
    }

    #[cfg(feature = "aot-guard")]
    #[test]
    fn an_app_of_processes_on_aot() {
        an_app_of_processes(aot::Aot);
    }

    #[test]
    fn event_cells_on_the_interpreter() {
        event_cells(interp::Interp { max_pages: 16384 });
    }

    #[cfg(feature = "aot-guard")]
    #[test]
    fn event_cells_on_aot() {
        event_cells(aot::Aot);
    }

    #[test]
    fn postgres_on_the_interpreter() {
        postgres_end_to_end(interp::Interp { max_pages: 16384 }, "interp");
    }

    #[cfg(feature = "aot-guard")]
    #[test]
    fn postgres_on_aot() {
        postgres_end_to_end(aot::Aot, "aot");
    }

    /// Objects by id: `/do/new` hands one out, `/do/id/<uuid>` reaches it.
    #[test]
    fn objects_by_uuid() {
        let dir = store::tempdir::Dir::new();
        let (port, _, _) = do_server(interp::Interp { max_pages: 16384 }, cfg(&dir.0, 2));
        let (code, body) = http(port, "GET", "/do/new", "");
        assert_eq!(code, 200);
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"].as_str().unwrap().to_string();
        assert_eq!(http(port, "POST", &format!("/do/id/{id}/incr"), ""), (200, "{\"count\":1}".into()));
        assert_eq!(http(port, "POST", &format!("/do/id/{}/incr", id.to_uppercase()), ""), (200, "{\"count\":2}".into()));
        // A plain object with the same text as a name is a different object.
        assert_eq!(http(port, "GET", &format!("/do/{id}/count"), ""), (200, "{\"count\":0}".into()));
        assert_eq!(http(port, "GET", "/do/id/not-a-uuid/count", "").0, 400);
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
        // A client can't fire an object's alarm handler.
        assert_eq!(http(port, "ALARM", "/do/room1/", "0").0, 400);
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
        // Far enough ahead that a busy machine can't let it fire before we look.
        assert_eq!(http(port, "POST", "/do/a1/alarm", "1500").0, 200);
        assert_eq!(fired(port, "a1"), r#"{"fired":0,"pending":true}"#);
        assert!(eventually(5000, || fired(port, "a1") == r#"{"fired":1,"pending":false}"#), "{}", fired(port, "a1"));
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
