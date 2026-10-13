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

pub trait Engine {
    type Program;
    type Vm: CellVm;
    /// Validate and prepare a module, once per deploy.
    fn load(&self, wasm: &[u8]) -> Result<Self::Program, String>;
    /// Make a fresh cell from a loaded program.
    fn spawn(&self, program: &Self::Program) -> Result<Self::Vm, String>;
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
    /// A full copy of the cell, even mid-call. `None` if the engine can't.
    fn snapshot(&self) -> Option<Self>
    where
        Self: Sized;
}
