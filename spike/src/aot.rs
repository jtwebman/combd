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

/// Ticks (loop iterations) one call may run before it is killed.
const CPU_LIMIT: i64 = 2_000_000_000;

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
    st: HostStateBox,
}

/// HostState isn't Clone; snapshots copy the parts that matter.
#[derive(Default)]
struct HostStateBox(HostState);

impl Clone for HostStateBox {
    fn clone(&self) -> Self {
        HostStateBox(HostState { counter: self.0.counter, out: self.0.out.clone() })
    }
}

impl Host for AotHost {
    fn call(&mut self, import: u32, args: &[u64], mem: &mut [u8]) -> Result<u64, String> {
        let imp = self.imports.get(import as usize).ok_or("unknown import")?;
        let r = host::call(imp, args, &mut SliceMem(mem), &mut self.st.0)?;
        Ok(r.first().copied().unwrap_or(0))
    }
    fn boxed_clone(&self) -> Box<dyn Host> {
        Box::new(self.clone())
    }
    fn out(&self) -> Vec<u8> {
        self.st.0.out.clone()
    }
}

pub struct AotVm {
    c: Ctx,
    mem: AotMem,
    m: &'static Compiled,
    pending: Option<(u32, Vec<u64>)>,
}

impl Engine for Aot {
    type Program = &'static Compiled;
    type Vm = AotVm;

    fn load(&self, wasm: &[u8]) -> Result<&'static Compiled, String> {
        combd_aot::find(wasm).ok_or_else(|| "no AOT build for this module (run spike/aot.sh)".into())
    }

    fn spawn(&self, m: &&'static Compiled) -> Result<AotVm, String> {
        let imports = m.imports.iter().map(|(md, n, r)| Import { module: md.to_string(), name: n.to_string(), results: *r }).collect();
        let host = Box::new(AotHost { imports, st: HostStateBox::default() });
        let (c, mem) = (m.new)(host).map_err(|t| t.to_string())?;
        Ok(AotVm { c, mem, m, pending: None })
    }
}

impl CellVm for AotVm {
    fn begin(&mut self, export: &str, args: &[u64]) -> Result<(), String> {
        let f = (self.m.export)(export).ok_or("no such export")?;
        self.pending = Some((f, args.to_vec()));
        Ok(())
    }

    fn run(&mut self, _reductions: u32) -> Outcome {
        let Some((f, args)) = self.pending.take() else { return Outcome::Trap("cell is not running".into()) };
        self.c.fuel = CPU_LIMIT;
        #[cfg(feature = "aot-guard")]
        let result = {
            let range = self.mem.range();
            let (c, mem, call) = (&mut self.c, &mut self.mem, self.m.call);
            match combd_aot::guard::catch(range, || call(c, mem, f, &args)) {
                Some(r) => r,
                None => return Outcome::Trap("out of bounds memory access (guard page)".into()),
            }
        };
        #[cfg(not(feature = "aot-guard"))]
        let result = (self.m.call)(&mut self.c, &mut self.mem, f, &args);
        match result {
            Ok(r) => Outcome::Done(r),
            Err(combd_aot::rt::Trap::Host) => Outcome::Trap(self.c.host_err.take().unwrap_or_default()),
            Err(t) => Outcome::Trap(t.to_string()),
        }
    }

    fn resume(&mut self, _results: &[u64]) {}

    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        self.mem.get(ptr as usize, len as usize).map(<[u8]>::to_vec)
    }

    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        match self.mem.get_mut(ptr as usize, bytes.len()) {
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
        self.c.host.out()
    }

    /// Between calls a compiled cell is plain data, so it can be copied.
    fn snapshot(&self) -> Option<AotVm> {
        self.pending.is_none().then(|| AotVm { c: self.c.clone(), mem: self.mem.clone(), m: self.m, pending: None })
    }
}
