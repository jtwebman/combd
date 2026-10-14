//! The AOT tier behind the engine interface: modules compiled ahead of time
//! by `aot-gen` into `combd-aot`, found by the hash of their Wasm bytes.
//!
//! ponytail: a compiled cell can't pause mid-call yet. It runs each call to
//! the end, with a hard CPU limit that traps a runaway loop. Pausing needs a
//! stack per cell or deopt to the interpreter (see the plan's AOT tier).

use crate::engine::{CellVm, Engine, Import, Outcome};
use crate::host::{self, HostState, Mem};
use combd_aot::rt::{Ctx, Host, Mem as AotMem};
use combd_aot::Compiled;

/// Ticks (loop iterations) one call may run before it is killed: about 100 ms
/// of tight-loop work. Compiled cells can't pause yet, so this is what keeps a
/// stuck request from freezing its event loop (like Workers' CPU limit).
/// `COMBD_CPU_LIMIT_TICKS` overrides it.
fn cpu_limit() -> i64 {
    static LIMIT: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| std::env::var("COMBD_CPU_LIMIT_TICKS").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000_000))
}

pub struct Aot;

struct SliceMem<'a>(&'a mut [u8]);

impl Mem for SliceMem<'_> {
    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        self.0.get(ptr as usize..(ptr as usize).checked_add(len as usize)?).map(<[u8]>::to_vec)
    }
    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        match self.0.get_mut(ptr as usize..ptr as usize + bytes.len()) {
            Some(to) => {
                to.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

#[derive(Clone)]
struct AotHost {
    imports: Vec<Import>,
    st: HostState,
}

impl Host for AotHost {
    fn call(&mut self, import: u32, args: &[u64], mem: &mut [u8]) -> Result<u64, String> {
        let imp = self.imports.get(import as usize).ok_or("unknown import")?;
        let r = host::call(imp, args, &mut SliceMem(mem), &mut self.st)?;
        Ok(r.first().copied().unwrap_or(0))
    }
    fn boxed_clone(&self) -> Box<dyn Host> {
        Box::new(self.clone())
    }
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn out(&self) -> Vec<u8> {
        self.st.out.clone()
    }
}

pub struct AotVm {
    /// The cell between calls. `None` while a call is in flight on its fiber.
    state: Option<(Ctx, AotMem)>,
    m: &'static Compiled,
    pending: Option<(u32, Vec<u64>)>,
    #[cfg(feature = "aot-guard")]
    fiber: Option<combd_aot::fiber::Fiber<Ran>>,
    #[cfg(feature = "aot-guard")]
    spare: Option<corosensei::stack::DefaultStack>,
}

/// What a finished call hands back: the cell, and its result (`None` if a
/// guard page turned an out-of-bounds access into a trap).
#[cfg(feature = "aot-guard")]
type Ran = (Ctx, AotMem, Option<Result<Vec<u64>, combd_aot::rt::Trap>>);

/// Compiled-cell ticks per interpreter reduction. A run of 2,500 reductions is
/// about 50 us either way.
const TICKS_PER_REDUCTION: i64 = 20;

/// `COMBD_NO_YIELD=1` turns pausing off (run each call to the end), to compare.
fn no_yield() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("COMBD_NO_YIELD").is_some())
}

fn slice_for(reductions: u32) -> i64 {
    if reductions == u32::MAX || no_yield() { i64::MAX } else { reductions as i64 * TICKS_PER_REDUCTION }
}

impl Engine for Aot {
    type Program = &'static Compiled;
    type Vm = AotVm;

    fn load(&self, wasm: &[u8]) -> Result<&'static Compiled, String> {
        combd_aot::find(wasm).ok_or_else(|| "no AOT build for this module (run spike/aot.sh)".into())
    }

    fn spawn(&self, m: &&'static Compiled) -> Result<AotVm, String> {
        let imports = m.imports.iter().map(|(md, n, r)| Import { module: md.to_string(), name: n.to_string(), results: *r }).collect();
        let host = Box::new(AotHost { imports, st: HostState::default() });
        let (c, mem) = (m.new)(host).map_err(|t| t.to_string())?;
        Ok(AotVm {
            state: Some((c, mem)),
            m,
            pending: None,
            #[cfg(feature = "aot-guard")]
            fiber: None,
            #[cfg(feature = "aot-guard")]
            spare: None,
        })
    }
}

impl CellVm for AotVm {
    fn begin(&mut self, export: &str, args: &[u64]) -> Result<(), String> {
        let f = (self.m.export)(export).ok_or("no such export")?;
        self.pending = Some((f, args.to_vec()));
        Ok(())
    }

    fn run(&mut self, reductions: u32) -> Outcome {
        #[cfg(feature = "aot-guard")]
        {
            // A call that can't pause (no slice) needs no fiber: run it right here.
            if self.fiber.is_none() && slice_for(reductions) == i64::MAX {
                let Some((f, args)) = self.pending.take() else { return Outcome::Trap("cell is not running".into()) };
                let Some((c, mem)) = self.state.as_mut() else { return Outcome::Trap("cell is busy".into()) };
                c.slice = i64::MAX;
                c.budget = cpu_limit();
                c.fuel = c.budget;
                let range = mem.range();
                let call = self.m.call;
                return match combd_aot::guard::catch(range, || call(c, mem, f, &args)) {
                    None => Outcome::Trap("out of bounds memory access (guard page)".into()),
                    Some(Ok(v)) => Outcome::Done(v),
                    Some(Err(combd_aot::rt::Trap::Host)) => Outcome::Trap(c.host_err.take().unwrap_or_default()),
                    Some(Err(t)) => Outcome::Trap(t.to_string()),
                };
            }
            if self.fiber.is_none() {
                let Some((f, args)) = self.pending.take() else { return Outcome::Trap("cell is not running".into()) };
                let Some((mut c, mut mem)) = self.state.take() else { return Outcome::Trap("cell is busy".into()) };
                c.slice = slice_for(reductions);
                c.budget = cpu_limit();
                c.fuel = c.slice.min(c.budget);
                let call = self.m.call;
                self.fiber = Some(combd_aot::fiber::Fiber::start(self.spare.take(), move || {
                    let range = mem.range();
                    let r = combd_aot::guard::catch(range, || call(&mut c, &mut mem, f, &args));
                    (c, mem, r)
                }));
            }
            let (step, stack) = self.fiber.as_mut().unwrap().resume();
            if stack.is_some() {
                self.spare = stack;
            }
            match step {
                combd_aot::fiber::Step::Paused => Outcome::Yield,
                combd_aot::fiber::Step::Done((mut c, mem, r)) => {
                    self.fiber = None;
                    let out = match r {
                        None => Outcome::Trap("out of bounds memory access (guard page)".into()),
                        Some(Ok(v)) => Outcome::Done(v),
                        Some(Err(combd_aot::rt::Trap::Host)) => Outcome::Trap(c.host_err.take().unwrap_or_default()),
                        Some(Err(t)) => Outcome::Trap(t.to_string()),
                    };
                    self.state = Some((c, mem));
                    out
                }
            }
        }
        #[cfg(not(feature = "aot-guard"))]
        {
            let _ = reductions;
            let Some((f, args)) = self.pending.take() else { return Outcome::Trap("cell is not running".into()) };
            let Some((c, mem)) = self.state.as_mut() else { return Outcome::Trap("cell is busy".into()) };
            // No fibers in this build: no pausing, so the whole budget is one slice.
            c.slice = i64::MAX;
            c.budget = cpu_limit();
            c.fuel = c.budget;
            match (self.m.call)(c, mem, f, &args) {
                Ok(r) => Outcome::Done(r),
                Err(combd_aot::rt::Trap::Host) => Outcome::Trap(c.host_err.take().unwrap_or_default()),
                Err(t) => Outcome::Trap(t.to_string()),
            }
        }
    }

    fn resume(&mut self, _results: &[u64]) {}

    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        self.state.as_ref()?.1.get(ptr as usize, len as usize).map(<[u8]>::to_vec)
    }

    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        let Some((_, mem)) = self.state.as_mut() else { return false };
        match mem.get_mut(ptr as usize, bytes.len()) {
            Some(to) => {
                to.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    fn import(&self, _idx: u32) -> Option<&Import> {
        None // host calls run inline
    }

    fn host_out(&self) -> Vec<u8> {
        self.state.as_ref().map_or_else(Vec::new, |(c, _)| c.host.out())
    }

    fn host_state(&mut self) -> Option<&mut HostState> {
        let (c, _) = self.state.as_mut()?;
        c.host.as_any().downcast_mut::<AotHost>().map(|h| &mut h.st)
    }

    /// Between calls a compiled cell is plain data, so it can be copied.
    fn snapshot(&self) -> Option<AotVm> {
        let (c, mem) = self.state.as_ref()?;
        self.pending.is_none().then(|| AotVm {
            state: Some((c.clone(), mem.clone())),
            m: self.m,
            pending: None,
            #[cfg(feature = "aot-guard")]
            fiber: None,
            #[cfg(feature = "aot-guard")]
            spare: None,
        })
    }
}
