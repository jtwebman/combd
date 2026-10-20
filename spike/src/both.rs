//! Both engines on one node: each module runs the way its manifest says
//! (`[modules.<name>] engine`), interpreted or compiled, and only that way.
//! A program, and every cell made from it, is one or the other for good.

use crate::aot::{Aot, AotVm};
use crate::engine::{CellVm, Engine, Import, Kind, Outcome};
use crate::interp::{Interp, Module, Vm};
use std::sync::Arc;

pub struct Both {
    pub interp: Interp,
    pub aot: Aot,
}

pub enum Program {
    Interpreted(Arc<Module>),
    Compiled(&'static combd_aot::Compiled),
}

pub enum Cell {
    Interpreted(Vm),
    Compiled(AotVm),
}

impl Engine for Both {
    type Program = Program;
    type Vm = Cell;

    /// Without a word from the manifest: interpreted.
    fn load(&self, wasm: &[u8]) -> Result<Program, String> {
        self.load_as(wasm, Kind::Interpreted)
    }

    fn load_as(&self, wasm: &[u8], kind: Kind) -> Result<Program, String> {
        match kind {
            Kind::Interpreted => self.interp.load(wasm).map(Program::Interpreted),
            Kind::Compiled => self.aot.load(wasm).map(Program::Compiled),
        }
    }

    fn spawn(&self, p: &Program) -> Result<Cell, String> {
        match p {
            Program::Interpreted(m) => self.interp.spawn(m).map(Cell::Interpreted),
            Program::Compiled(c) => self.aot.spawn(c).map(Cell::Compiled),
        }
    }

    fn note(&self, p: &Program) -> Option<String> {
        match p {
            Program::Interpreted(m) => self.interp.note(m),
            Program::Compiled(c) => self.aot.note(c),
        }
    }

    fn thaw(&self, p: &Program, saved: &[u8]) -> Result<Cell, String> {
        match p {
            Program::Interpreted(m) => self.interp.thaw(m, saved).map(Cell::Interpreted),
            Program::Compiled(c) => self.aot.thaw(c, saved).map(Cell::Compiled),
        }
    }
}

/// Each call, to whichever engine the cell is.
macro_rules! each {
    ($self:expr, $v:ident => $e:expr) => {
        match $self {
            Cell::Interpreted($v) => $e,
            Cell::Compiled($v) => $e,
        }
    };
}

impl CellVm for Cell {
    fn begin(&mut self, export: &str, args: &[u64]) -> Result<(), String> {
        each!(self, v => v.begin(export, args))
    }
    fn run(&mut self, reductions: u32) -> Outcome {
        each!(self, v => v.run(reductions))
    }
    fn resume(&mut self, results: &[u64]) {
        each!(self, v => v.resume(results))
    }
    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        each!(self, v => v.read(ptr, len))
    }
    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        each!(self, v => v.write(ptr, bytes))
    }
    fn import(&self, idx: u32) -> Option<&Import> {
        each!(self, v => v.import(idx))
    }
    fn host_out(&self) -> Vec<u8> {
        each!(self, v => v.host_out())
    }
    fn host_state(&mut self) -> Option<&mut crate::host::HostState> {
        each!(self, v => v.host_state())
    }
    fn has_export(&mut self, name: &str) -> bool {
        each!(self, v => v.has_export(name))
    }
    fn freeze(&self) -> Option<Vec<u8>> {
        each!(self, v => v.freeze())
    }
    fn cap_memory(&mut self, pages: u32) -> Result<(), String> {
        each!(self, v => v.cap_memory(pages))
    }
    fn db_pending(&mut self) -> Option<crate::host::DbOp> {
        each!(self, v => v.db_pending())
    }
    fn db_done(&mut self, value: u64, out: Vec<u8>) {
        each!(self, v => v.db_done(value, out))
    }
    fn snapshot(&self) -> Option<Cell> {
        match self {
            Cell::Interpreted(v) => v.snapshot().map(Cell::Interpreted),
            Cell::Compiled(v) => v.snapshot().map(Cell::Compiled),
        }
    }
}
