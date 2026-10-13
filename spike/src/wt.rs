//! Wasmtime behind the same engine interface, for the race.
//! Fuel yields stand in for reductions. Host calls run inline, so this engine
//! never returns `HostCall`, and it can't snapshot.

use crate::engine::{CellVm, Engine, Outcome};
use std::future::Future;
use std::pin::{pin, Pin};
use std::task::{Context, Poll, Waker};
use wasmtime::{Caller, Config, Instance, InstancePre, Linker, Memory, Module, Store, Val, ValType};

pub struct Wt {
    engine: wasmtime::Engine,
    fuel_slice: u64,
}

impl Wt {
    /// `fuel_slice` is how much fuel (about one per wasm op) a cell runs before
    /// it yields.
    pub fn new(fuel_slice: u64) -> Self {
        let mut c = Config::new();
        c.consume_fuel(true);
        Wt { engine: wasmtime::Engine::new(&c).expect("wasmtime config"), fuel_slice }
    }
}

pub struct Host {
    counter: i64,
}

type Running = Pin<Box<dyn Future<Output = (Store<Host>, wasmtime::Result<Vec<Val>>)> + Send>>;

pub struct WtVm {
    store: Option<Store<Host>>,
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

impl Engine for Wt {
    type Program = InstancePre<Host>;
    type Vm = WtVm;

    fn load(&self, wasm: &[u8]) -> Result<InstancePre<Host>, String> {
        let module = Module::new(&self.engine, wasm).map_err(e)?;
        let mut linker = Linker::new(&self.engine);
        linker
            .func_wrap("combd", "counter_incr", |mut c: Caller<'_, Host>| {
                c.data_mut().counter += 1;
                c.data().counter
            })
            .map_err(e)?;
        linker.instantiate_pre(&module).map_err(e)
    }

    fn spawn(&self, pre: &InstancePre<Host>) -> Result<WtVm, String> {
        let mut store = Store::new(&self.engine, Host { counter: 0 });
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
                    Ok(vals) => Outcome::Done(
                        vals.iter()
                            .map(|v| match v {
                                Val::I32(x) => *x as u32 as u64,
                                Val::I64(x) => *x as u64,
                                Val::F32(x) => *x as u64,
                                Val::F64(x) => *x,
                                _ => 0,
                            })
                            .collect(),
                    ),
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

    fn snapshot(&self) -> Option<WtVm> {
        None
    }
}
