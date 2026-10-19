//! The engine interface. The rest of combd only talks to engines through this.
//! An engine never does I/O. It hands back `HostCall` and waits for `resume`.

pub enum Outcome {
    /// The call finished with these raw results (each value as its bits).
    Done(Vec<u64>),
    /// The reduction budget ran out. Call `run` again to continue.
    Yield,
    /// The guest called host import `import`. Handle it, then call `resume`.
    HostCall { import: u32, args: Vec<u64> },
    /// The cell crashed. Only this cell is affected.
    Trap(String),
}

/// A host function a module imports.
#[derive(Clone, Debug)]
pub struct Import {
    pub module: String,
    pub name: String,
    /// How many values it returns (0 or 1 for everything we support).
    pub results: u32,
}

pub trait Engine {
    type Program;
    type Vm: CellVm;
    /// Validate and prepare a module, once per deploy.
    fn load(&self, wasm: &[u8]) -> Result<Self::Program, String>;
    /// Make a fresh cell from a loaded program.
    fn spawn(&self, program: &Self::Program) -> Result<Self::Vm, String>;
    /// A cell again, from what `CellVm::freeze` kept.
    fn thaw(&self, _program: &Self::Program, _saved: &[u8]) -> Result<Self::Vm, String> {
        Err("this engine can't thaw a cell".into())
    }
}

pub trait CellVm {
    /// Start calling an exported function. Nothing runs until `run`.
    fn begin(&mut self, export: &str, args: &[u64]) -> Result<(), String>;
    /// Run until done, out of budget, a host call, or a trap.
    fn run(&mut self, reductions: u32) -> Outcome;
    /// Hand back the results of the last `HostCall`.
    fn resume(&mut self, results: &[u64]);
    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>>;
    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool;
    /// Which import a `HostCall` refers to.
    fn import(&self, idx: u32) -> Option<&Import>;
    /// Output the engine captured itself (only engines that run host calls inline).
    fn host_out(&self) -> Vec<u8> {
        Vec::new()
    }
    /// The host state of an engine that runs host calls inline. Others use
    /// the `HostState` the caller passes in.
    fn host_state(&mut self) -> Option<&mut crate::host::HostState> {
        None
    }
    /// Whether the module exports `name` (event cells export `on_request`).
    fn has_export(&mut self, _name: &str) -> bool {
        false
    }
    /// An idle cell, as little as it takes to make it again (`Engine::thaw`):
    /// what differs from a fresh one. `None`: it's busy, or the engine can't.
    fn freeze(&self) -> Option<Vec<u8>> {
        None
    }
    /// Lower the most this cell's memory may grow to (`memory_kb`), in
    /// pages. `Err`: it already starts bigger. Engines that fix the limit
    /// when they compile (the AOT tier) ignore it.
    fn cap_memory(&mut self, _pages: u32) -> Result<(), String> {
        Ok(())
    }
    /// A database call this cell is waiting on, for engines that pause
    /// inside a host call (the compiled tier's fibers). The interpreter
    /// leaves it in its `HostState` instead.
    fn db_pending(&mut self) -> Option<crate::host::DbOp> {
        None
    }
    /// The answer to that call: the import's result, and what `pg_read` gets.
    fn db_done(&mut self, _value: u64, _out: Vec<u8>) {}
    /// A full copy of the cell, even mid-call. `None` if the engine can't.
    fn snapshot(&self) -> Option<Self>
    where
        Self: Sized;
}
