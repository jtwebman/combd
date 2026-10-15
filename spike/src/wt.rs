//! Wasmtime behind the same engine interface, for the race.
//! Fuel yields stand in for reductions. Host calls run inline, so this engine
//! never returns `HostCall`, and it can't snapshot.

use crate::engine::{CellVm, Engine, Import, Outcome};
use crate::host::{self, HostState, Mem};
use std::future::Future;
use std::pin::{pin, Pin};
use std::task::{Context, Poll, Waker};
use wasmtime::{
    Caller, Config, ExternType, Instance, InstanceAllocationStrategy, InstancePre, Linker, Memory, Module,
    PoolingAllocationConfig, Store, Val, ValType,
};

pub struct Wt {
    engine: wasmtime::Engine,
    fuel_slice: u64,
}

impl Wt {
    /// `fuel_slice` is how much fuel (about one per wasm op) a cell runs before
    /// it yields.
    /// `pool` turns on Wasmtime's pooling allocator, sized for `slots` live
    /// cells. It is Wasmtime's fast path for many short-lived instances.
    pub fn new(fuel_slice: u64, pool: Option<u32>) -> Self {
        let mut c = Config::new();
        c.consume_fuel(true);
        if let Some(slots) = pool {
            let mut p = PoolingAllocationConfig::new();
            p.total_core_instances(slots).total_memories(slots).total_tables(slots).total_stacks(slots);
            c.allocation_strategy(InstanceAllocationStrategy::Pooling(p));
        }
        Wt { engine: wasmtime::Engine::new(&c).expect("wasmtime config"), fuel_slice }
    }
}

type Running = Pin<Box<dyn Future<Output = (Store<HostState>, wasmtime::Result<Vec<Val>>)> + Send>>;

pub struct WtVm {
    store: Option<Store<HostState>>,
    instance: Instance,
    memory: Memory,
    running: Option<Running>,
}

fn e(x: impl std::fmt::Display) -> String {
    x.to_string()
}

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// Guest memory seen through a Wasmtime caller.
struct SliceMem<'a>(Option<&'a mut [u8]>);

impl Mem for SliceMem<'_> {
    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        self.0.as_ref()?.get(ptr as usize..(ptr as usize).checked_add(len as usize)?).map(<[u8]>::to_vec)
    }
    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        let Some(m) = self.0.as_mut() else { return false };
        match m.get_mut(ptr as usize..ptr as usize + bytes.len()) {
            Some(to) => {
                to.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

fn to_u64(v: &Val) -> u64 {
    match v {
        Val::I32(x) => *x as u32 as u64,
        Val::I64(x) => *x as u64,
        Val::F32(x) => *x as u64,
        Val::F64(x) => *x,
        _ => 0,
    }
}

fn from_u64(t: &ValType, v: u64) -> Val {
    match t {
        ValType::I64 => Val::I64(v as i64),
        ValType::F32 => Val::F32(v as u32),
        ValType::F64 => Val::F64(v),
        _ => Val::I32(v as i32),
    }
}

impl Engine for Wt {
    type Program = InstancePre<HostState>;
    type Vm = WtVm;

    fn load(&self, wasm: &[u8]) -> Result<InstancePre<HostState>, String> {
        let module = Module::new(&self.engine, wasm).map_err(e)?;
        let mut linker = Linker::new(&self.engine);
        // Every import goes to the same host functions our own engine uses.
        for i in module.imports() {
            let ExternType::Func(ty) = i.ty() else { return Err("only function imports are supported".into()) };
            let results: Vec<ValType> = ty.results().collect();
            let imp = Import { module: i.module().into(), name: i.name().into(), results: results.len() as u32 };
            linker
                .func_new(i.module(), i.name(), ty.clone(), move |mut caller: Caller<'_, HostState>, params, out| {
                    let args: Vec<u64> = params.iter().map(to_u64).collect();
                    let memory = caller.get_export("memory").and_then(|x| x.into_memory());
                    let (data, st) = match memory {
                        Some(m) => {
                            let (d, s) = m.data_and_store_mut(&mut caller);
                            (Some(d), s)
                        }
                        None => (None, caller.data_mut()),
                    };
                    let r = host::call(&imp, &args, &mut SliceMem(data), st).map_err(wasmtime::Error::msg)?;
                    for ((slot, t), v) in out.iter_mut().zip(&results).zip(r) {
                        *slot = from_u64(t, v);
                    }
                    Ok(())
                })
                .map_err(e)?;
        }
        linker.instantiate_pre(&module).map_err(e)
    }

    fn spawn(&self, pre: &InstancePre<HostState>) -> Result<WtVm, String> {
        let mut store = Store::new(&self.engine, HostState::default());
        store.set_fuel(u64::MAX).map_err(e)?;
        store.fuel_async_yield_interval(Some(self.fuel_slice)).map_err(e)?;
        let instance = block_on(pre.instantiate_async(&mut store)).map_err(e)?;
        let memory = instance.get_memory(&mut store, "memory").ok_or("no memory export")?;
        Ok(WtVm { store: Some(store), instance, memory, running: None })
    }
}

impl CellVm for WtVm {
    fn begin(&mut self, export: &str, args: &[u64]) -> Result<(), String> {
        let store = self.store.as_mut().ok_or("cell is busy")?;
        let func = self.instance.get_func(&mut *store, export).ok_or("no such export")?;
        let ty = func.ty(&*store);
        let params: Vec<Val> = ty
            .params()
            .zip(args)
            .map(|(t, &a)| match t {
                ValType::I64 => Val::I64(a as i64),
                ValType::F32 => Val::F32(a as u32),
                ValType::F64 => Val::F64(a),
                _ => Val::I32(a as i32),
            })
            .collect();
        let n = ty.results().len();
        let mut store = self.store.take().unwrap();
        self.running = Some(Box::pin(async move {
            let mut out = vec![Val::I32(0); n];
            let r = func.call_async(&mut store, &params, &mut out).await;
            (store, r.map(|_| out))
        }));
        Ok(())
    }

    fn run(&mut self, _reductions: u32) -> Outcome {
        let Some(fut) = self.running.as_mut() else {
            return Outcome::Trap("cell is not running".into());
        };
        match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Pending => Outcome::Yield,
            Poll::Ready((store, r)) => {
                self.store = Some(store);
                self.running = None;
                match r {
                    Ok(vals) => Outcome::Done(vals.iter().map(to_u64).collect()),
                    Err(err) => Outcome::Trap(format!("{err:#}")),
                }
            }
        }
    }

    fn resume(&mut self, _results: &[u64]) {}

    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        let data = self.memory.data(self.store.as_ref()?);
        data.get(ptr as usize..(ptr as usize).checked_add(len as usize)?).map(<[u8]>::to_vec)
    }

    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        let Some(store) = self.store.as_mut() else { return false };
        match self.memory.data_mut(store).get_mut(ptr as usize..ptr as usize + bytes.len()) {
            Some(to) => {
                to.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    fn import(&self, _idx: u32) -> Option<&Import> {
        None // host calls run inline, never as `HostCall`
    }

    fn has_export(&mut self, name: &str) -> bool {
        let Some(store) = self.store.as_mut() else { return false };
        self.instance.get_func(&mut *store, name).is_some()
    }

    fn host_out(&self) -> Vec<u8> {
        self.store.as_ref().map_or_else(Vec::new, |s| s.data().out.clone())
    }

    fn snapshot(&self) -> Option<WtVm> {
        None
    }
}
