//! Differential fuzzing: random valid Wasm modules (wasm-smith), run on our
//! interpreter and on Wasmtime. Every exported function is called with random
//! arguments, and the two must agree on everything: the results, whether it
//! traps, and every byte of memory after each call.
//!
//! The modules use only what our interpreter runs (no SIMD, GC, threads,
//! exceptions, tail calls, reference types, memory64, or more than one
//! memory). A module that still uses something we refuse (say, a table
//! instruction from bulk memory) is counted as refused, by reason. wasm-smith
//! makes every loop end (a fuel global that traps at zero) and NaN results
//! canonical, so both engines must match bit for bit.
//!
//! Not a mismatch: running out of stack. The engines have different depth
//! limits; such a call counts as inconclusive.
//!
//! `combd fuzz N [start]` runs seeds `start..start+N`, and writes each
//! module that disagrees to `fuzz-<seed>.wasm`.
//!
//! The AOT tier compiles modules ahead of time, so it's fuzzed in a batch
//! (`aot-fuzz.sh`): `fuzz-gen` writes each seed's module as generated Rust
//! into the AOT crate, the crate is built, and `fuzz-aot` runs each one on
//! the AOT tier and on our interpreter, compared the same way.

use crate::engine::{CellVm, Engine, Outcome};
use crate::interp::Interp;
use crate::sim::Rng;
use std::collections::BTreeMap;
use wasmtime::{Config, Instance, Module, Store, Val};

#[derive(Default, Debug)]
pub struct Stats {
    pub modules: u64,
    pub calls: u64,
    pub traps: u64,
    pub inconclusive: u64,
    /// Calls made on a cell that was frozen and thawed first.
    pub thawed: u64,
    pub refused: BTreeMap<String, u64>,
    /// Traps by kind, as Wasmtime names them.
    pub trap_kinds: BTreeMap<String, u64>,
    pub mismatches: Vec<(u64, String)>,
}

/// Enough for any call; past this, it's inconclusive.
const BUDGET: u64 = 200_000_000;
const ENGINE: Interp = Interp { max_pages: 1024 };
/// The loop fuel wasm-smith adds: one global per module, shared by every call.
const FUEL: u32 = 1_000_000;
/// Random bytes wasm-smith builds each module from: more makes bigger modules.
const ENTROPY: usize = 64 << 10;

/// Half the seeds avoid trapping code, so calls run deep; the other half
/// keep it, since traps must match too.
fn smith_config(seed: u64) -> wasm_smith::Config {
    let mut c = wasm_smith::Config::default();
    c.disallow_traps = seed % 2 == 0;
    for off in [
        &mut c.simd_enabled,
        &mut c.relaxed_simd_enabled,
        &mut c.threads_enabled,
        &mut c.shared_everything_threads_enabled,
        &mut c.gc_enabled,
        &mut c.exceptions_enabled,
        &mut c.tail_call_enabled,
        &mut c.memory64_enabled,
        &mut c.reference_types_enabled,
        &mut c.custom_page_sizes_enabled,
        &mut c.custom_descriptors_enabled,
        &mut c.wide_arithmetic_enabled,
        &mut c.extended_const_enabled,
        &mut c.allow_start_export,
    ] {
        *off = false;
    }
    for on in [&mut c.bulk_memory_enabled, &mut c.multi_value_enabled, &mut c.saturating_float_to_int_enabled, &mut c.sign_extension_ops_enabled, &mut c.canonicalize_nans, &mut c.export_everything] {
        *on = true;
    }
    c.min_funcs = 4;
    c.max_memories = 1;
    // Every memory declares a maximum, and Wasmtime gets our cap too, so
    // `memory.grow` has the same limit on both.
    c.memory_max_size_required = true;
    c.max_tables = 1;
    c.max_imports = 0;
    // Small memories, so comparing every byte after every call is cheap.
    c.max_memory32_bytes = 16 * 65536;
    c
}

/// Our interpreter's answer, or Wasmtime's.
#[derive(Debug)]
enum Got {
    Done(Vec<u64>),
    /// Why, for the report; any two traps match.
    Trap(String),
    /// Out of stack or budget: limits differ between engines.
    Limit,
}

impl PartialEq for Got {
    fn eq(&self, other: &Got) -> bool {
        match (self, other) {
            (Got::Done(a), Got::Done(b)) => a == b,
            (Got::Trap(_), Got::Trap(_)) | (Got::Limit, Got::Limit) => true,
            _ => false,
        }
    }
}

fn ours(vm: &mut <Interp as Engine>::Vm, name: &str, args: &[u64]) -> Result<Got, String> {
    vm.begin(name, args)?;
    let mut spent = 0;
    loop {
        match vm.run(1_000_000) {
            Outcome::Done(rs) => return Ok(Got::Done(rs)),
            Outcome::Trap(t) if t.contains("engine bug") => return Err(format!("ENGINE PANIC: {t}")),
            Outcome::Trap(t) if t.contains("exhausted") => return Ok(Got::Limit),
            Outcome::Trap(t) => return Ok(Got::Trap(t)),
            Outcome::Yield => {
                spent += 1_000_000;
                if spent > BUDGET {
                    return Ok(Got::Limit);
                }
            }
            Outcome::HostCall { .. } => return Err("a host call, with no imports".into()),
        }
    }
}

fn store(engine: &wasmtime::Engine) -> Store<wasmtime::StoreLimits> {
    let limits = wasmtime::StoreLimitsBuilder::new().memory_size(ENGINE.max_pages as usize * 65536).build();
    let mut s = Store::new(engine, limits);
    s.limiter(|l| l);
    s.set_fuel(BUDGET).unwrap();
    s
}

fn theirs(store: &mut Store<wasmtime::StoreLimits>, inst: &Instance, name: &str, args: &[Val], kinds: &mut BTreeMap<String, u64>) -> Got {
    let f = inst.get_func(&mut *store, name).unwrap();
    let mut out = vec![Val::I32(0); f.ty(&*store).results().len()];
    store.set_fuel(BUDGET).unwrap();
    match f.call(&mut *store, args, &mut out) {
        Ok(()) => Got::Done(out.iter().map(bits).collect()),
        Err(e) => {
            let kind = e.downcast_ref::<wasmtime::Trap>().map_or("other".to_string(), |t| format!("{t:?}"));
            *kinds.entry(kind).or_default() += 1;
            match e.downcast_ref::<wasmtime::Trap>() {
                Some(wasmtime::Trap::StackOverflow | wasmtime::Trap::OutOfFuel) => Got::Limit,
                _ => Got::Trap(format!("{e}")),
            }
        }
    }
}

fn bits(v: &Val) -> u64 {
    match v {
        Val::I32(x) => *x as u32 as u64,
        Val::I64(x) => *x as u64,
        Val::F32(x) => *x as u64,
        Val::F64(x) => *x,
        _ => u64::MAX,
    }
}

/// A random argument, biased toward edge values.
fn arg(rng: &mut Rng, t: &wasmtime::ValType) -> (u64, Val) {
    let edge = [0u64, 1, u64::MAX, 0x8000_0000, 0x7fff_ffff, 0x8000_0000_0000_0000, 0xffff_ffff];
    let x = if rng.chance(0.3) { edge[rng.below(edge.len() as u64) as usize] } else { rng.next() >> rng.below(64) };
    match t {
        wasmtime::ValType::I32 => (x as u32 as u64, Val::I32(x as i32)),
        wasmtime::ValType::I64 => (x, Val::I64(x as i64)),
        wasmtime::ValType::F32 => (x as u32 as u64, Val::F32(x as u32)),
        _ => (x, Val::F64(x)),
    }
}

fn memory(store: &mut Store<wasmtime::StoreLimits>, inst: &Instance) -> Option<Vec<u8>> {
    let exports: Vec<_> = inst.exports(&mut *store).filter_map(|e| e.into_memory()).collect();
    exports.first().map(|m| m.data(&*store).to_vec())
}

/// Seed `seed`'s module, and the random numbers left for its arguments.
pub fn module(seed: u64) -> Result<(Vec<u8>, Rng), String> {
    let mut rng = Rng::new(seed);
    let bytes: Vec<u8> = (0..ENTROPY).map(|_| rng.next() as u8).collect();
    let mut module = wasm_smith::Module::new(smith_config(seed), &mut arbitrary::Unstructured::new(&bytes)).map_err(|e| format!("wasm-smith: {e}"))?;
    module.ensure_termination(FUEL).map_err(|e| format!("wasm-smith: {e}"))?;
    Ok((module.to_bytes(), rng))
}

/// One seed: a module, every export called on both engines.
fn one(seed: u64, engine: &wasmtime::Engine, st: &mut Stats) -> Result<(), String> {
    let (bin, rng) = module(seed)?;
    compare(&bin, rng, engine, st)
}

/// A saved module (`fuzz-<seed>.wasm`) again, with the same arguments.
pub fn replay(path: &std::path::Path) -> Result<(), String> {
    let seed: u64 = path.file_stem().and_then(|s| s.to_str()?.strip_prefix("fuzz-")?.parse().ok()).ok_or("name it fuzz-<seed>.wasm")?;
    let bin = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut rng = Rng::new(seed);
    for _ in 0..ENTROPY {
        rng.next();
    }
    let mut c = Config::new();
    c.consume_fuel(true);
    compare(&bin, rng, &wasmtime::Engine::new(&c).unwrap(), &mut Stats::default())
}

fn compare(bin: &[u8], mut rng: Rng, engine: &wasmtime::Engine, st: &mut Stats) -> Result<(), String> {
    let bin = bin.to_vec();
    st.modules += 1;

    let m = match ENGINE.load(&bin) {
        Ok(m) => Ok(m),
        Err(e) if e.contains("support") => {
            let short = e.split(|c: char| c == '{' || c == '(').next().unwrap_or("").trim().to_string();
            *st.refused.entry(short).or_default() += 1;
            return Ok(());
        }
        Err(e) => Err(e),
    };
    let wm = Module::new(engine, &bin).map_err(|e| format!("wasmtime rejected it: {e}"))?;

    let m = match m {
        Ok(m) => m,
        // A module we refuse at load (an element segment out of bounds, say)
        // must be one Wasmtime can't start either.
        Err(e) => {
            let mut store = store(engine);
            return match Instance::new(&mut store, &wm, &[]) {
                Err(_) => Ok(()),
                Ok(_) => Err(format!("we refused it ({e}), wasmtime started it")),
            };
        }
    };
    let funcs: Vec<(String, wasmtime::FuncType)> = wm.exports().filter_map(|e| Some((e.name().to_string(), e.ty().func()?.clone()))).collect();
    for (name, ty) in funcs {
        for _ in 0..3 {
            // A fresh instance on both for every call: the loop fuel is one
            // global per instance, and a call that burns it would make every
            // later call trap at once.
            let mut store = store(engine);
            let (mut vm, inst) = match (ENGINE.spawn(&m), Instance::new(&mut store, &wm, &[])) {
                (Ok(v), Ok(i)) => (v, i),
                (Err(_), Err(_)) => return Ok(()), // both refuse to start: agreed
                (Ok(_), Err(e)) => return Err(format!("wasmtime failed to start it ({e}), we started it")),
                (Err(e), Ok(_)) => return Err(format!("we failed to start it ({e}), wasmtime started it")),
            };
            let (raw, vals): (Vec<u64>, Vec<Val>) = ty.params().map(|t| arg(&mut rng, &t)).unzip();
            let a = ours(&mut vm, &name, &raw)?;
            let b = theirs(&mut store, &inst, &name, &vals, &mut st.trap_kinds);
            st.calls += 1;
            if a == Got::Limit || b == Got::Limit {
                // One engine hit a limit the other may not have.
                st.inconclusive += 1;
                continue;
            }
            if a != b {
                return Err(format!("{name}({raw:x?}): ours {a:x?}, wasmtime {b:x?}"));
            }
            if matches!(a, Got::Trap(_)) {
                st.traps += 1;
            }
            same_memory(&mut store, &inst, &vm, &format!("after {name}({raw:x?})"))?;
            // Frozen and thawed between calls (as an idle session is), it must
            // go on exactly as Wasmtime's, which never was. A trapped cell
            // doesn't freeze.
            let Some(saved) = vm.freeze() else { continue };
            let mut vm = ENGINE.thaw(&m, &saved).map_err(|e| format!("after {name}({raw:x?}): can't thaw what it froze: {e}"))?;
            same_memory(&mut store, &inst, &vm, &format!("thawed after {name}({raw:x?})"))?;
            let (raw2, vals2): (Vec<u64>, Vec<Val>) = ty.params().map(|t| arg(&mut rng, &t)).unzip();
            let (a2, b2) = (ours(&mut vm, &name, &raw2)?, theirs(&mut store, &inst, &name, &vals2, &mut st.trap_kinds));
            st.calls += 1;
            st.thawed += 1;
            if a2 == Got::Limit || b2 == Got::Limit {
                st.inconclusive += 1;
                continue;
            }
            if a2 != b2 {
                return Err(format!("thawed, then {name}({raw2:x?}): ours {a2:x?}, wasmtime {b2:x?}"));
            }
            same_memory(&mut store, &inst, &vm, &format!("thawed, then {name}({raw2:x?})"))?;
        }
    }
    Ok(())
}

/// Our memory against Wasmtime's, every byte, and the same size.
fn same_memory(store: &mut Store<wasmtime::StoreLimits>, inst: &Instance, vm: &<Interp as Engine>::Vm, when: &str) -> Result<(), String> {
    let Some(mem) = memory(store, inst) else { return Ok(()) };
    let ours_mem = vm.read(0, mem.len() as u32);
    if ours_mem.as_deref() != Some(&mem[..]) || vm.read(0, mem.len() as u32 + 1).is_some() {
        let first = ours_mem.map(|o| o.iter().zip(&mem).position(|(x, y)| x != y));
        return Err(format!("{when}: memory differs (wasmtime has {} bytes; first difference at {first:?})", mem.len()));
    }
    Ok(())
}

pub fn run(seeds: std::ops::Range<u64>, save: bool) -> Stats {
    let mut c = Config::new();
    c.consume_fuel(true);
    let engine = wasmtime::Engine::new(&c).unwrap();
    let mut st = Stats::default();
    for seed in seeds {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| one(seed, &engine, &mut st)));
        let r = r.unwrap_or_else(|_| Err("a panic outside the cell (the harness or wasm-smith)".into()));
        if let Err(e) = r {
            if e.starts_with("wasm-smith") {
                continue; // it couldn't make a module from these bytes
            }
            if save {
                if let Ok((bin, _)) = module(seed) {
                    let _ = std::fs::write(format!("fuzz-{seed}.wasm"), bin);
                }
            }
            st.mismatches.push((seed, e));
        }
    }
    st
}

/// Any engine's answer, through the engine interface.
fn cell<V: CellVm>(vm: &mut V, name: &str, args: &[u64]) -> Result<Got, String> {
    vm.begin(name, args)?;
    let mut spent = 0;
    loop {
        match vm.run(1_000_000) {
            Outcome::Done(rs) => return Ok(Got::Done(rs)),
            Outcome::Trap(t) if t.contains("engine bug") => return Err(format!("ENGINE PANIC: {t}")),
            Outcome::Trap(t) if t.contains("xhausted") || t.contains("OutOfFuel") => return Ok(Got::Limit),
            Outcome::Trap(t) => return Ok(Got::Trap(t)),
            Outcome::Yield => {
                spent += 1_000_000;
                if spent > BUDGET {
                    return Ok(Got::Limit);
                }
            }
            Outcome::HostCall { .. } => return Err("a host call, with no imports".into()),
        }
    }
}

/// All of a cell's memory, a page at a time.
fn all_memory<V: CellVm>(vm: &V) -> Vec<u8> {
    let mut out = vec![];
    while let Some(p) = vm.read(out.len() as u32, 65536) {
        out.extend(p);
    }
    out
}

/// Whether memory must match after a trap too. Not for the guard-page AOT
/// tier: LLVM may drop or merge a store around a load it doesn't know can
/// fault (`aot-cases/precise_traps.wat`, `dead_store`). That's invisible in
/// combd, because a trapped cell is never used again (`http::finish_task`);
/// results, traps, and memory after every call that succeeds must still match.
const MEMORY_AT_TRAP: bool = !cfg!(feature = "aot-guard");

/// One seed on two engines, `a` (ours) and `b`, compared call by call.
fn pair<A: Engine, B: Engine>(seed: u64, a: &A, b: &B, types: &wasmtime::Engine, st: &mut Stats) -> Result<(), String> {
    let (bin, rng) = module(seed)?;
    pair_bin(&bin, rng, a, b, types, st)
}

fn pair_bin<A: Engine, B: Engine>(bin: &[u8], mut rng: Rng, a: &A, b: &B, types: &wasmtime::Engine, st: &mut Stats) -> Result<(), String> {
    let (Ok(ma), Ok(mb)) = (a.load(bin), b.load(bin)) else { return Ok(()) };
    st.modules += 1;
    let wm = Module::new(types, bin).map_err(|e| e.to_string())?;
    let funcs: Vec<(String, wasmtime::FuncType)> = wm.exports().filter_map(|e| Some((e.name().to_string(), e.ty().func()?.clone()))).collect();
    for (name, ty) in funcs {
        for _ in 0..3 {
            let (mut va, mut vb) = match (a.spawn(&ma), b.spawn(&mb)) {
                (Ok(x), Ok(y)) => (x, y),
                (Err(_), Err(_)) => return Ok(()),
                (x, y) => return Err(format!("only one started it: {:?} vs {:?}", x.err(), y.err())),
            };
            let raw: Vec<u64> = ty.params().map(|t| arg(&mut rng, &t).0).collect();
            let (x, y) = (cell(&mut va, &name, &raw)?, cell(&mut vb, &name, &raw)?);
            st.calls += 1;
            if x == Got::Limit || y == Got::Limit {
                st.inconclusive += 1;
                continue;
            }
            if x != y {
                return Err(format!("{name}({raw:x?}): interpreter {x:x?}, other {y:x?}"));
            }
            if matches!(x, Got::Trap(_)) {
                st.traps += 1;
                if !MEMORY_AT_TRAP {
                    continue;
                }
            }
            let (ma, mb) = (all_memory(&va), all_memory(&vb));
            if ma != mb {
                let first = ma.iter().zip(&mb).position(|(p, q)| p != q);
                return Err(format!("after {name}({raw:x?}): memory differs ({} vs {} bytes; first difference at {first:?})", ma.len(), mb.len()));
            }
        }
    }
    Ok(())
}

/// Write each seed's module as generated Rust into the AOT crate
/// (`aot/src/fuzz/`, listed in `aot/src/gen_fuzz.rs`; neither is in git).
/// `fuzz-gen 0` clears them, so normal builds don't carry them.
pub fn gen_aot(seeds: std::ops::Range<u64>, crate_src: &std::path::Path) -> Stats {
    let dir = crate_src.join("fuzz");
    let _ = std::fs::remove_dir_all(&dir);
    let mut st = Stats::default();
    let mut list = String::from("//! Fuzz modules from `combd fuzz-gen`. Not in git.\n\n");
    let mut names = vec![];
    // The hand-written cases first, as `c0`, `c1`, ...
    for (i, bin) in cases().into_iter().enumerate() {
        let src = crate::aot_gen::generate(&bin, &format!("aot case {i}")).expect("an AOT case must compile");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("c{i}.rs")), src).unwrap();
        list.push_str(&format!("#[path = \"fuzz/c{i}.rs\"]\nmod c{i};\n"));
        names.push(format!("c{i}::MODULE"));
    }
    if seeds.is_empty() && names.len() == cases().len() {
        // `fuzz-gen 0`: clear everything out.
        names.clear();
        let _ = std::fs::remove_dir_all(&dir);
    }
    for seed in seeds {
        let Ok((bin, _)) = module(seed) else { continue };
        if ENGINE.load(&bin).is_err() {
            continue; // the interpreter refuses it: nothing to compare against
        }
        match crate::aot_gen::generate(&bin, &format!("fuzz seed {seed}")) {
            Ok(src) => {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(format!("s{seed}.rs")), src).unwrap();
                list.push_str(&format!("#[path = \"fuzz/s{seed}.rs\"]\nmod s{seed};\n"));
                names.push(format!("s{seed}::MODULE"));
                st.modules += 1;
            }
            Err(e) => *st.refused.entry(e.split(|c: char| c == '{' || c == '(').next().unwrap_or("").trim().to_string()).or_default() += 1,
        }
    }
    let path = crate_src.join("gen_fuzz.rs");
    if names.is_empty() {
        let _ = std::fs::remove_file(path);
    } else {
        list.push_str(&format!("\npub static ALL: &[crate::Compiled] = &[{}];\n", names.join(", ")));
        std::fs::write(path, list).unwrap();
    }
    st
}

/// The hand-written AOT cases, `aot-cases/*.wat`, in name order.
fn cases() -> Vec<Vec<u8>> {
    let mut paths: Vec<_> = std::fs::read_dir("aot-cases").map(|r| r.filter_map(|e| Some(e.ok()?.path())).collect()).unwrap_or_default();
    paths.sort();
    paths.into_iter().filter(|p| p.extension().is_some_and(|x| x == "wat")).map(|p| wat::parse_file(&p).expect("a valid case")).collect()
}

/// The AOT tier against our interpreter, for the seeds `fuzz-gen` built in.
#[cfg(feature = "aot-lite")]
pub fn run_aot(seeds: std::ops::Range<u64>) -> Stats {
    let types = wasmtime::Engine::default();
    let mut st = Stats::default();
    for (i, bin) in cases().into_iter().enumerate() {
        if let Err(e) = pair_bin(&bin, Rng::new(i as u64), &ENGINE, &crate::aot::Aot, &types, &mut st) {
            st.mismatches.push((u64::MAX - i as u64, format!("aot case {i}: {e}")));
        }
    }
    for seed in seeds {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pair(seed, &ENGINE, &crate::aot::Aot, &types, &mut st)));
        match r.unwrap_or_else(|_| Err("a panic outside the cell".into())) {
            Ok(()) => {}
            Err(e) if e.starts_with("wasm-smith") => {}
            Err(e) => st.mismatches.push((seed, e)),
        }
    }
    st
}

#[cfg(test)]
mod tests {
    #[test]
    fn our_interpreter_agrees_with_wasmtime() {
        let st = super::run(0..300, false);
        assert!(st.mismatches.is_empty(), "{:#?}", st.mismatches);
        assert!(st.modules > 250 && st.calls > 500, "{st:?}");
    }
}
