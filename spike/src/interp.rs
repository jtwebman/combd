//! combd's own Wasm interpreter, phase 0 version.
//!
//! wasmparser decodes and validates. Each function is translated once into a
//! flat list of ops with branch targets already resolved. Values live on one
//! untyped `u64` stack (validation already proved the types). A running cell
//! is plain data, so a snapshot is just a clone, even mid-call.
//!
//! Reductions are spent at loop heads and calls, the same safe points a JIT
//! tier would have to honor later.
//!
//! ponytail: naive stack interpreter, one big match. Register bytecode and
//! fused ops are the known next step if compute speed matters.

use crate::engine::{CellVm, Engine, Import, Outcome};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use wasmparser::{
    BlockType, ConstExpr, DataKind, ElementItems, ElementKind, ExternalKind, FrameKind, FuncType,
    FuncValidator, FunctionBody, Operator, Parser, Payload, TypeRef, ValidPayload, Validator,
    ValidatorResources,
};

const PAGE: usize = 65536;
const MAX_FRAMES: usize = 10_000;
const MAX_STACK: usize = 1 << 20;
const MAX_TABLE: u64 = 1 << 20;
/// Spare stack slots past a function's max height, for host call results.
const SLACK: usize = 8;

/// Where a branch goes: the new pc, the operand height to cut back to, and how
/// many values on top to carry over.
#[derive(Clone, Copy, Debug)]
struct Target {
    pc: u32,
    height: u32,
    keep: u32,
}

macro_rules! ops {
    (plain: $($p:ident)*; mem: $($m:ident)*;) => {
        #[derive(Clone, Copy, Debug)]
        enum Op {
            Unreachable, Unsupported, Tick,
            Br(Target), BrIf(Target), BrIfNot(u32), Jump(u32), BrTable { start: u32, len: u32 }, Return,
            Call(u32), CallIndirect { ty: u32, table: u32 },
            Drop, Select,
            LocalGet(u32), LocalSet(u32), LocalTee(u32), GlobalGet(u32), GlobalSet(u32),
            I32Const(i32), I64Const(i64), F32Const(u32), F64Const(u64),
            MemorySize, MemoryGrow, MemoryCopy, MemoryFill, MemoryInit(u32), DataDrop(u32),
            // Fused pairs, made by `fuse` from the hottest sequences Rust emits.
            LocalGet2(u32, u32), LocalAddConst(u32, i32), LocalTeeGet(u32, u32), LocalSetGet(u32, u32),
            I32AddConst(i32), I32ShlConst(i32), I32AddLocal(u32), I32AddTee(u32), I32AddSet(u32),
            BrIfI32Eqz(Target), BrIfI32Eq(Target), BrIfI32Ne(Target), BrIfI32LtS(Target), BrIfI32LtU(Target),
            BrIfI32GtS(Target), BrIfI32GtU(Target), BrIfI32LeS(Target), BrIfI32LeU(Target), BrIfI32GeS(Target),
            BrIfI32GeU(Target),
            $($p,)* $($m(u32),)*
        }

        /// Ops that map one to one, with the same names wasmparser uses.
        fn simple(op: &Operator) -> Option<Op> {
            Some(match op {
                $(Operator::$p => Op::$p,)*
                $(Operator::$m { memarg } if memarg.memory == 0 => Op::$m(memarg.offset as u32),)*
                _ => return None,
            })
        }
    };
}

ops! {
    plain:
    I32Eqz I32Eq I32Ne I32LtS I32LtU I32GtS I32GtU I32LeS I32LeU I32GeS I32GeU
    I64Eqz I64Eq I64Ne I64LtS I64LtU I64GtS I64GtU I64LeS I64LeU I64GeS I64GeU
    F32Eq F32Ne F32Lt F32Gt F32Le F32Ge F64Eq F64Ne F64Lt F64Gt F64Le F64Ge
    I32Clz I32Ctz I32Popcnt I32Add I32Sub I32Mul I32DivS I32DivU I32RemS I32RemU
    I32And I32Or I32Xor I32Shl I32ShrS I32ShrU I32Rotl I32Rotr
    I64Clz I64Ctz I64Popcnt I64Add I64Sub I64Mul I64DivS I64DivU I64RemS I64RemU
    I64And I64Or I64Xor I64Shl I64ShrS I64ShrU I64Rotl I64Rotr
    F32Abs F32Neg F32Ceil F32Floor F32Trunc F32Nearest F32Sqrt
    F32Add F32Sub F32Mul F32Div F32Min F32Max F32Copysign
    F64Abs F64Neg F64Ceil F64Floor F64Trunc F64Nearest F64Sqrt
    F64Add F64Sub F64Mul F64Div F64Min F64Max F64Copysign
    I32WrapI64 I32TruncF32S I32TruncF32U I32TruncF64S I32TruncF64U
    I64ExtendI32S I64ExtendI32U I64TruncF32S I64TruncF32U I64TruncF64S I64TruncF64U
    F32ConvertI32S F32ConvertI32U F32ConvertI64S F32ConvertI64U F32DemoteF64
    F64ConvertI32S F64ConvertI32U F64ConvertI64S F64ConvertI64U F64PromoteF32
    I32ReinterpretF32 I64ReinterpretF64 F32ReinterpretI32 F64ReinterpretI64
    I32Extend8S I32Extend16S I64Extend8S I64Extend16S I64Extend32S
    I32TruncSatF32S I32TruncSatF32U I32TruncSatF64S I32TruncSatF64U
    I64TruncSatF32S I64TruncSatF32U I64TruncSatF64S I64TruncSatF64U;
    mem:
    I32Load I64Load F32Load F64Load I32Load8S I32Load8U I32Load16S I32Load16U
    I64Load8S I64Load8U I64Load16S I64Load16U I64Load32S I64Load32U
    I32Store I64Store F32Store F64Store I32Store8 I32Store16 I64Store8 I64Store16 I64Store32;
}

/// Merge two adjacent ops into one, if we have a fused form for the pair.
fn fuse(a: Op, b: Op) -> Option<Op> {
    Some(match (a, b) {
        (Op::LocalGet(x), Op::LocalGet(y)) => Op::LocalGet2(x, y),
        (Op::I32Const(c), Op::I32Add) => Op::I32AddConst(c),
        (Op::I32Const(c), Op::I32Shl) => Op::I32ShlConst(c),
        (Op::LocalGet(x), Op::I32AddConst(c)) => Op::LocalAddConst(x, c),
        (Op::LocalGet(x), Op::I32Add) => Op::I32AddLocal(x),
        (Op::I32Add, Op::LocalTee(x)) => Op::I32AddTee(x),
        (Op::I32Add, Op::LocalSet(x)) => Op::I32AddSet(x),
        (Op::LocalTee(x), Op::LocalGet(y)) => Op::LocalTeeGet(x, y),
        (Op::LocalSet(x), Op::LocalGet(y)) => Op::LocalSetGet(x, y),
        _ => return None,
    })
}

/// A compare followed by `br_if` becomes one branch op.
fn fuse_br_if(cmp: Op, t: Target) -> Option<Op> {
    Some(match cmp {
        Op::I32Eqz => Op::BrIfI32Eqz(t),
        Op::I32Eq => Op::BrIfI32Eq(t),
        Op::I32Ne => Op::BrIfI32Ne(t),
        Op::I32LtS => Op::BrIfI32LtS(t),
        Op::I32LtU => Op::BrIfI32LtU(t),
        Op::I32GtS => Op::BrIfI32GtS(t),
        Op::I32GtU => Op::BrIfI32GtU(t),
        Op::I32LeS => Op::BrIfI32LeS(t),
        Op::I32LeU => Op::BrIfI32LeU(t),
        Op::I32GeS => Op::BrIfI32GeS(t),
        Op::I32GeU => Op::BrIfI32GeU(t),
        _ => return None,
    })
}

/// Push an op, fusing it with the one before when allowed. `barrier` is the
/// newest branch target: an op that something jumps to must stay its own op.
fn emit(f: &mut Func, barrier: usize, op: Op) {
    f.ops.push(op);
    while f.ops.len() >= 2 && f.ops.len() - 1 > barrier {
        let n = f.ops.len();
        let Some(merged) = fuse(f.ops[n - 2], f.ops[n - 1]) else { break };
        f.ops[n - 2] = merged;
        f.ops.pop();
    }
}

struct Func {
    nparams: u32,
    nlocals: u32, // includes params
    nresults: u32,
    max_height: u32, // deepest operand stack the validator saw
    ops: Vec<Op>,
    table: Vec<Target>, // br_table entries
}

#[derive(Default)]
pub struct Module {
    types: Vec<FuncType>,
    func_types: Vec<u32>, // every function, imports first
    imports: u32,
    import_names: Vec<Import>,
    funcs: Vec<Func>, // defined functions only
    globals: Vec<u64>,
    mem: (u32, Option<u32>), // min and max pages
    tables: Vec<Vec<Option<u32>>>,
    data: Vec<Vec<u8>>,
    active: Vec<(u32, u32)>, // (segment, offset) copied in at spawn
    exports: HashMap<String, u32>,
}

fn e(x: impl std::fmt::Display) -> String {
    x.to_string()
}

fn const_val(expr: &ConstExpr, globals: &[u64]) -> Result<u64, String> {
    let mut r = expr.get_operators_reader();
    let v = match r.read().map_err(e)? {
        Operator::I32Const { value } => value as u32 as u64,
        Operator::I64Const { value } => value as u64,
        Operator::F32Const { value } => value.bits() as u64,
        Operator::F64Const { value } => value.bits(),
        Operator::GlobalGet { global_index } => *globals.get(global_index as usize).ok_or("bad global")?,
        Operator::RefFunc { function_index } => function_index as u64,
        Operator::RefNull { .. } => u64::MAX,
        op => return Err(format!("unsupported constant expression {op:?}")),
    };
    match r.read().map_err(e)? {
        Operator::End => Ok(v),
        _ => Err("unsupported constant expression".into()),
    }
}

pub fn load(wasm: &[u8]) -> Result<Module, String> {
    let mut v = Validator::new();
    let mut m = Module::default();
    let mut elems = vec![];
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(e)?;
        if let ValidPayload::Func(f, body) = v.payload(&payload).map_err(e)? {
            let func = translate(&m, f.into_validator(Default::default()), &body)?;
            m.funcs.push(func);
        }
        match payload {
            Payload::TypeSection(r) => {
                for t in r.into_iter_err_on_gc_types() {
                    m.types.push(t.map_err(e)?);
                }
            }
            Payload::ImportSection(r) => {
                for i in r.into_imports() {
                    let i = i.map_err(e)?;
                    match i.ty {
                        TypeRef::Func(t) => {
                            let results = m.types.get(t as usize).map_or(0, |ty| ty.results().len() as u32);
                            m.import_names.push(Import { module: i.module.to_string(), name: i.name.to_string(), results });
                            m.func_types.push(t);
                            m.imports += 1;
                        }
                        _ => return Err("only function imports are supported".into()),
                    }
                }
            }
            Payload::FunctionSection(r) => {
                for t in r {
                    m.func_types.push(t.map_err(e)?);
                }
            }
            Payload::TableSection(r) => {
                for t in r {
                    let t = t.map_err(e)?;
                    if t.ty.initial > MAX_TABLE {
                        return Err("table too large".into());
                    }
                    m.tables.push(vec![None; t.ty.initial as usize]);
                }
            }
            Payload::MemorySection(r) => {
                for mt in r {
                    let mt = mt.map_err(e)?;
                    if mt.memory64 || mt.shared {
                        return Err("only plain 32-bit memory is supported".into());
                    }
                    m.mem = (mt.initial as u32, mt.maximum.map(|x| x as u32));
                }
            }
            Payload::GlobalSection(r) => {
                for g in r {
                    let g = g.map_err(e)?;
                    let val = const_val(&g.init_expr, &m.globals)?;
                    m.globals.push(val);
                }
            }
            Payload::ExportSection(r) => {
                for x in r {
                    let x = x.map_err(e)?;
                    if x.kind == ExternalKind::Func {
                        m.exports.insert(x.name.to_string(), x.index);
                    }
                }
            }
            Payload::StartSection { .. } => return Err("start functions are not supported".into()),
            Payload::ElementSection(r) => {
                for el in r {
                    let el = el.map_err(e)?;
                    let ElementKind::Active { table_index, offset_expr } = el.kind else { continue };
                    let offset = const_val(&offset_expr, &m.globals)? as usize;
                    let funcs: Vec<u32> = match el.items {
                        ElementItems::Functions(fs) => fs.into_iter().collect::<Result<_, _>>().map_err(e)?,
                        ElementItems::Expressions(_, xs) => xs
                            .into_iter()
                            .map(|x| const_val(&x.map_err(e)?, &m.globals).map(|v| v as u32))
                            .collect::<Result<_, _>>()?,
                    };
                    elems.push((table_index.unwrap_or(0) as usize, offset, funcs));
                }
            }
            Payload::DataSection(r) => {
                for d in r {
                    let d = d.map_err(e)?;
                    let seg = m.data.len() as u32;
                    m.data.push(d.data.to_vec());
                    if let DataKind::Active { memory_index: 0, offset_expr } = d.kind {
                        m.active.push((seg, const_val(&offset_expr, &m.globals)? as u32));
                    }
                }
            }
            _ => {}
        }
    }
    for (t, offset, funcs) in elems {
        let table = m.tables.get_mut(t).ok_or("bad table")?;
        let slots = table.get_mut(offset..offset + funcs.len()).ok_or("element segment out of bounds")?;
        for (slot, f) in slots.iter_mut().zip(funcs) {
            *slot = (f != u32::MAX).then_some(f);
        }
    }
    Ok(m)
}

enum Patch {
    Op(usize),
    Table(usize),
}

struct Ctl {
    loop_start: Option<u32>,
    patches: Vec<Patch>,
    else_fix: Option<usize>,
}

fn patch(f: &mut Func, p: Patch, pc: u32) {
    match p {
        Patch::Op(i) => match &mut f.ops[i] {
            Op::Br(t)
            | Op::BrIf(t)
            | Op::BrIfI32Eqz(t)
            | Op::BrIfI32Eq(t)
            | Op::BrIfI32Ne(t)
            | Op::BrIfI32LtS(t)
            | Op::BrIfI32LtU(t)
            | Op::BrIfI32GtS(t)
            | Op::BrIfI32GtU(t)
            | Op::BrIfI32LeS(t)
            | Op::BrIfI32LeU(t)
            | Op::BrIfI32GeS(t)
            | Op::BrIfI32GeU(t) => t.pc = pc,
            Op::BrIfNot(to) | Op::Jump(to) => *to = pc,
            _ => {}
        },
        Patch::Table(i) => f.table[i].pc = pc,
    }
}

fn arity(m: &Module, bt: BlockType) -> (u32, u32) {
    match bt {
        BlockType::Empty => (0, 0),
        BlockType::Type(_) => (0, 1),
        BlockType::FuncType(i) => {
            let t = &m.types[i as usize];
            (t.params().len() as u32, t.results().len() as u32)
        }
    }
}

/// Resolve branch `depth`. Must run before the validator sees the op.
fn label(
    fv: &FuncValidator<ValidatorResources>,
    m: &Module,
    ctl: &mut [Ctl],
    depth: u32,
    at: Patch,
) -> Result<Target, String> {
    let fr = fv.get_control_frame(depth as usize).ok_or("bad branch depth")?;
    let (params, results) = arity(m, fr.block_type);
    let keep = if fr.kind == FrameKind::Loop { params } else { results };
    let n = ctl.len();
    let c = &mut ctl[n - 1 - depth as usize];
    let pc = match c.loop_start {
        Some(start) => start,
        None => {
            c.patches.push(at);
            0
        }
    };
    Ok(Target { pc, height: fr.height as u32, keep })
}

fn translate(m: &Module, mut fv: FuncValidator<ValidatorResources>, body: &FunctionBody) -> Result<Func, String> {
    let ty = &m.types[m.func_types[m.imports as usize + m.funcs.len()] as usize];
    let nparams = ty.params().len() as u32;
    let mut f = Func { nparams, nlocals: nparams, nresults: ty.results().len() as u32, max_height: 0, ops: vec![], table: vec![] };
    let mut locals = body.get_locals_reader().map_err(e)?;
    for _ in 0..locals.get_count() {
        let offset = locals.original_position() as u64;
        let (n, t) = locals.read().map_err(e)?;
        fv.define_locals(offset, n, t).map_err(e)?;
        f.nlocals = f.nlocals.checked_add(n).filter(|&n| n < 50_000).ok_or("too many locals")?;
    }
    let mut ctl = vec![Ctl { loop_start: None, patches: vec![], else_fix: None }];
    let mut barrier = 0;
    let mut r = body.get_operators_reader().map_err(e)?;
    while !r.eof() {
        let offset = r.original_position() as u64;
        let op = r.read().map_err(e)?;
        let dead = fv.get_control_frame(0).map_or(true, |fr| fr.unreachable);
        let here = f.ops.len();
        match &op {
            Operator::Block { .. } => ctl.push(Ctl { loop_start: None, patches: vec![], else_fix: None }),
            Operator::Loop { .. } => {
                ctl.push(Ctl { loop_start: Some(here as u32), patches: vec![], else_fix: None });
                f.ops.push(Op::Tick);
                barrier = here;
            }
            Operator::If { .. } => {
                ctl.push(Ctl { loop_start: None, patches: vec![], else_fix: Some(here) });
                f.ops.push(Op::BrIfNot(0));
            }
            Operator::Else => {
                let c = ctl.last_mut().ok_or("else without if")?;
                c.patches.push(Patch::Op(here));
                f.ops.push(Op::Jump(0));
                if let Some(i) = c.else_fix.take() {
                    patch(&mut f, Patch::Op(i), here as u32 + 1);
                }
                barrier = here + 1;
            }
            Operator::End => {
                let c = ctl.pop().ok_or("unbalanced end")?;
                if ctl.is_empty() {
                    f.ops.push(Op::Return);
                }
                for p in c.patches.into_iter().chain(c.else_fix.map(Patch::Op)) {
                    patch(&mut f, p, here as u32);
                }
                barrier = here;
            }
            _ if dead => {}
            Operator::Br { relative_depth } => {
                let t = label(&fv, m, &mut ctl, *relative_depth, Patch::Op(here))?;
                f.ops.push(Op::Br(t));
            }
            Operator::BrIf { relative_depth } => {
                let dummy = Target { pc: 0, height: 0, keep: 0 };
                if here > barrier && here > 0 && fuse_br_if(f.ops[here - 1], dummy).is_some() {
                    let t = label(&fv, m, &mut ctl, *relative_depth, Patch::Op(here - 1))?;
                    f.ops[here - 1] = fuse_br_if(f.ops[here - 1], t).unwrap();
                } else {
                    let t = label(&fv, m, &mut ctl, *relative_depth, Patch::Op(here))?;
                    f.ops.push(Op::BrIf(t));
                }
            }
            Operator::BrTable { targets } => {
                let start = f.table.len() as u32;
                let depths: Vec<u32> = targets.targets().chain([Ok(targets.default())]).collect::<Result<_, _>>().map_err(e)?;
                for d in &depths {
                    let t = label(&fv, m, &mut ctl, *d, Patch::Table(f.table.len()))?;
                    f.table.push(t);
                }
                f.ops.push(Op::BrTable { start, len: depths.len() as u32 });
            }
            Operator::Return => f.ops.push(Op::Return),
            Operator::Unreachable => emit(&mut f, barrier, Op::Unreachable),
            Operator::Nop => {}
            Operator::Call { function_index } => emit(&mut f, barrier, Op::Call(*function_index)),
            Operator::CallIndirect { type_index, table_index } => {
                emit(&mut f, barrier, Op::CallIndirect { ty: *type_index, table: *table_index })
            }
            Operator::Drop => emit(&mut f, barrier, Op::Drop),
            Operator::Select | Operator::TypedSelect { .. } => emit(&mut f, barrier, Op::Select),
            Operator::LocalGet { local_index } => emit(&mut f, barrier, Op::LocalGet(*local_index)),
            Operator::LocalSet { local_index } => emit(&mut f, barrier, Op::LocalSet(*local_index)),
            Operator::LocalTee { local_index } => emit(&mut f, barrier, Op::LocalTee(*local_index)),
            Operator::GlobalGet { global_index } => emit(&mut f, barrier, Op::GlobalGet(*global_index)),
            Operator::GlobalSet { global_index } => emit(&mut f, barrier, Op::GlobalSet(*global_index)),
            Operator::I32Const { value } => emit(&mut f, barrier, Op::I32Const(*value)),
            Operator::I64Const { value } => emit(&mut f, barrier, Op::I64Const(*value)),
            Operator::F32Const { value } => emit(&mut f, barrier, Op::F32Const(value.bits())),
            Operator::F64Const { value } => emit(&mut f, barrier, Op::F64Const(value.bits())),
            Operator::MemorySize { mem: 0 } => emit(&mut f, barrier, Op::MemorySize),
            Operator::MemoryGrow { mem: 0 } => emit(&mut f, barrier, Op::MemoryGrow),
            Operator::MemoryCopy { dst_mem: 0, src_mem: 0 } => emit(&mut f, barrier, Op::MemoryCopy),
            Operator::MemoryFill { mem: 0 } => emit(&mut f, barrier, Op::MemoryFill),
            Operator::MemoryInit { data_index, mem: 0 } => emit(&mut f, barrier, Op::MemoryInit(*data_index)),
            Operator::DataDrop { data_index } => emit(&mut f, barrier, Op::DataDrop(*data_index)),
            other => emit(&mut f, barrier, simple(other).unwrap_or(Op::Unsupported)),
        }
        fv.op(offset, &op).map_err(e)?;
        f.max_height = f.max_height.max(fv.operand_stack_height());
    }
    r.finish().map_err(e)?;
    Ok(f)
}

/// Our engine. `max_pages` caps every cell's memory.
pub struct Interp {
    pub max_pages: u32,
}

impl Engine for Interp {
    type Program = Arc<Module>;
    type Vm = Vm;

    fn load(&self, wasm: &[u8]) -> Result<Arc<Module>, String> {
        load(wasm).map(Arc::new)
    }

    fn spawn(&self, m: &Arc<Module>) -> Result<Vm, String> {
        let (min, max) = m.mem;
        if min > self.max_pages {
            return Err("initial memory over the limit".into());
        }
        let max_pages = max.unwrap_or(u32::MAX).min(self.max_pages);
        let mut mem = LinMem::new(min as usize * PAGE);
        let mut dropped = vec![false; m.data.len()];
        for &(seg, offset) in &m.active {
            let data = &m.data[seg as usize];
            let to = mem.buf[..mem.len].get_mut(offset as usize..offset as usize + data.len()).ok_or("data segment out of bounds")?;
            to.copy_from_slice(data);
            dropped[seg as usize] = true;
        }
        Ok(Vm {
            m: m.clone(),
            stack: vec![],
            frames: vec![],
            func: 0,
            pc: 0,
            fp: 0,
            sp: 0,
            mem,
            max_pages,
            globals: m.globals.clone(),
            tables: m.tables.clone(),
            dropped,
            state: State::Idle,
        })
    }
}

#[derive(Clone, Copy)]
struct Frame {
    func: u32,
    pc: u32,
    fp: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Idle,
    Running,
    Host,
    Poisoned,
}

/// A cell's linear memory. New space always comes from a fresh zeroed
/// allocation (calloc), so the OS only hands out a page when the cell first
/// writes to it.
///
/// ponytail: growing past the buffer doubles it and copies. Real mmap
/// reservation (like Wasmtime) needs `unsafe`; keep it for the audited FFI crate.
pub struct LinMem {
    buf: Vec<u8>,
    len: usize,
}

/// Copy granularity: Apple Silicon's page size.
const CHUNK: usize = 16 * 1024;
/// macOS zeroes smaller calloc blocks by hand, which touches every page.
/// From 1 MiB up it hands out lazy zero pages, even when reusing memory.
const MIN_CAP: usize = 1 << 20;

impl LinMem {
    fn new(len: usize) -> Self {
        LinMem { buf: vec![0; len.max(MIN_CAP)], len }
    }

    /// A fresh zeroed buffer of `cap` bytes holding the live bytes. All-zero
    /// chunks are skipped, so pages the cell never wrote stay untouched.
    fn copy_into(&self, cap: usize) -> Vec<u8> {
        let mut nb = vec![0u8; cap];
        for (i, c) in self.buf[..self.len].chunks(CHUNK).enumerate() {
            if c.iter().any(|&b| b != 0) {
                nb[i * CHUNK..i * CHUNK + c.len()].copy_from_slice(c);
            }
        }
        nb
    }

    /// Caller has already checked `new_len <= max`.
    fn grow(&mut self, new_len: usize, max: usize) {
        if new_len > self.buf.len() {
            self.buf = self.copy_into(new_len.max(self.buf.len() * 2).min(max));
        }
        self.len = new_len;
    }
}

impl Clone for LinMem {
    fn clone(&self) -> Self {
        LinMem { buf: self.copy_into(self.buf.len()), len: self.len }
    }
}

/// One cell. Everything in it is plain data.
#[derive(Clone)]
pub struct Vm {
    m: Arc<Module>,
    stack: Vec<u64>,
    frames: Vec<Frame>,
    func: u32,
    pc: u32,
    fp: u32,
    sp: u32,
    mem: LinMem,
    max_pages: u32,
    globals: Vec<u64>,
    tables: Vec<Vec<Option<u32>>>,
    dropped: Vec<bool>,
    state: State,
}

impl CellVm for Vm {
    fn begin(&mut self, export: &str, args: &[u64]) -> Result<(), String> {
        if self.state != State::Idle {
            return Err(format!("cell is {:?}", self.state));
        }
        let idx = *self.m.exports.get(export).ok_or("no such export")?;
        let func = idx.checked_sub(self.m.imports).ok_or("export is an import")?;
        let f = &self.m.funcs[func as usize];
        if args.len() != f.nparams as usize {
            return Err("wrong number of arguments".into());
        }
        self.stack.clear();
        self.frames.clear();
        self.stack.resize(f.nlocals as usize + f.max_height as usize + SLACK, 0);
        self.stack[..args.len()].copy_from_slice(args);
        (self.func, self.pc, self.fp, self.sp) = (func, 0, 0, f.nlocals);
        self.state = State::Running;
        Ok(())
    }

    fn run(&mut self, reductions: u32) -> Outcome {
        if self.state != State::Running {
            return Outcome::Trap(format!("cell is {:?}", self.state));
        }
        // A panic here would be our bug, not the guest's. It kills this cell only.
        match catch_unwind(AssertUnwindSafe(|| self.exec(reductions))) {
            Ok(Ok(out)) => out,
            Ok(Err(trap)) => {
                self.state = State::Idle;
                self.stack.clear();
                self.frames.clear();
                Outcome::Trap(trap.into())
            }
            Err(_) => {
                self.state = State::Poisoned;
                Outcome::Trap("engine bug, cell poisoned".into())
            }
        }
    }

    fn resume(&mut self, results: &[u64]) {
        if self.state == State::Host {
            let sp = self.sp as usize;
            self.stack[sp..sp + results.len()].copy_from_slice(results);
            self.sp += results.len() as u32;
            self.state = State::Running;
        }
    }

    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        self.mem.buf[..self.mem.len].get(ptr as usize..(ptr as usize).checked_add(len as usize)?).map(<[u8]>::to_vec)
    }

    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        match self.mem.buf[..self.mem.len].get_mut(ptr as usize..ptr as usize + bytes.len()) {
            Some(to) => {
                to.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    fn import(&self, idx: u32) -> Option<&Import> {
        self.m.import_names.get(idx as usize)
    }

    fn snapshot(&self) -> Option<Vm> {
        Some(self.clone())
    }
}

fn i32_(v: u64) -> i32 {
    v as u32 as i32
}
fn u32_(v: u64) -> u32 {
    v as u32
}
fn i64_(v: u64) -> i64 {
    v as i64
}
fn u64_(v: u64) -> u64 {
    v
}
fn f32_(v: u64) -> f32 {
    f32::from_bits(v as u32)
}
fn f64_(v: u64) -> f64 {
    f64::from_bits(v)
}
fn ri32(x: i32) -> u64 {
    x as u32 as u64
}
fn ru32(x: u32) -> u64 {
    x as u64
}
fn ri64(x: i64) -> u64 {
    x as u64
}
fn ru64(x: u64) -> u64 {
    x
}
fn rf32(x: f32) -> u64 {
    x.to_bits() as u64
}
fn rf64(x: f64) -> u64 {
    x.to_bits()
}
fn rb(x: bool) -> u64 {
    x as u64
}

macro_rules! fminmax {
    ($min:ident, $max:ident, $t:ty) => {
        fn $min(a: $t, b: $t) -> $t {
            if a.is_nan() || b.is_nan() {
                <$t>::NAN
            } else if a == b {
                if a.is_sign_negative() { a } else { b }
            } else {
                a.min(b)
            }
        }
        fn $max(a: $t, b: $t) -> $t {
            if a.is_nan() || b.is_nan() {
                <$t>::NAN
            } else if a == b {
                if a.is_sign_positive() { a } else { b }
            } else {
                a.max(b)
            }
        }
    };
}
fminmax!(fmin32, fmax32, f32);
fminmax!(fmin64, fmax64, f64);

/// Wasm's trapping float-to-int rule: trap on NaN or when the truncated value
/// falls outside [lo, hi).
fn trunc(x: f64, lo: f64, hi: f64) -> Result<f64, &'static str> {
    if x.is_nan() {
        return Err("invalid conversion to integer");
    }
    let t = x.trunc();
    if t < lo || t >= hi {
        return Err("integer overflow");
    }
    Ok(t)
}

const I32R: (f64, f64) = (-2147483648.0, 2147483648.0);
const U32R: (f64, f64) = (0.0, 4294967296.0);
const I64R: (f64, f64) = (-9223372036854775808.0, 9223372036854775808.0);
const U64R: (f64, f64) = (0.0, 18446744073709551616.0);

/// Move the carried values down to the label's height. Returns the new sp.
fn branch(s: &mut [u64], base: usize, sp: usize, t: Target) -> usize {
    let dst = base + t.height as usize;
    let keep = t.keep as usize;
    let src = sp - keep;
    if dst != src {
        s.copy_within(src..sp, dst);
    }
    dst + keep
}

impl Vm {
    fn exec(&mut self, mut fuel: u32) -> Result<Outcome, &'static str> {
        let m = self.m.clone();
        let Vm { stack: sv, frames, func, pc: spc, fp: sfp, sp: ssp, mem, max_pages, globals, tables, dropped, state, .. } = self;
        let mut fi = *func as usize;
        let mut f = &m.funcs[fi];
        let mut pc = *spc as usize;
        let mut fp = *sfp as usize;
        let mut sp = *ssp as usize;
        let mut base = fp + f.nlocals as usize;
        let lm = mem;
        let mut mem: &mut [u8] = &mut lm.buf[..lm.len];
        // Plain slices keep pointer and length in registers. Re-slice after any resize.
        let mut s: &mut [u64] = &mut sv[..];
        let mut ops: &[Op] = &f.ops;

        macro_rules! pop {
            () => {{
                sp -= 1;
                s[sp]
            }};
        }
        macro_rules! push {
            ($v:expr) => {{
                let v = $v;
                s[sp] = v;
                sp += 1;
            }};
        }
        macro_rules! save {
            ($pc:expr) => {{
                *func = fi as u32;
                *spc = $pc as u32;
                *sfp = fp as u32;
                *ssp = sp as u32;
            }};
        }
        macro_rules! un {
            ($from:ident, $to:ident, |$x:ident| $e:expr) => {{
                let v = &mut s[sp - 1];
                let $x = $from(*v);
                *v = $to($e);
            }};
        }
        macro_rules! bin {
            ($from:ident, $to:ident, |$a:ident, $b:ident| $e:expr) => {{
                let $b = $from(pop!());
                let v = &mut s[sp - 1];
                let $a = $from(*v);
                *v = $to($e);
            }};
        }
        macro_rules! load {
            ($off:expr, $n:literal, |$b:ident| $e:expr) => {{
                let a = pop!() as u32 as usize + $off as usize;
                let $b: [u8; $n] = mem.get(a..a + $n).ok_or("out of bounds memory access")?.try_into().unwrap();
                push!($e);
            }};
        }
        macro_rules! store {
            ($off:expr, $n:literal, |$v:ident| $e:expr) => {{
                let $v = pop!();
                let a = pop!() as u32 as usize + $off as usize;
                mem.get_mut(a..a + $n).ok_or("out of bounds memory access")?.copy_from_slice(&$e[..$n]);
            }};
        }
        macro_rules! brc {
            ($t:expr, |$a:ident| $cond:expr) => {{
                let $a = pop!();
                if $cond {
                    sp = branch(s, base, sp, $t);
                    pc = $t.pc as usize;
                }
            }};
            ($t:expr, $from:ident, |$a:ident, $b:ident| $cond:expr) => {{
                let $b = $from(pop!());
                let $a = $from(pop!());
                if $cond {
                    sp = branch(s, base, sp, $t);
                    pc = $t.pc as usize;
                }
            }};
        }
        macro_rules! call {
            ($idx:expr, $redo:expr) => {{
                let idx = $idx as usize;
                if idx < m.imports as usize {
                    let n = m.types[m.func_types[idx] as usize].params().len();
                    let args = s[sp - n..sp].to_vec();
                    sp -= n;
                    save!(pc);
                    *state = State::Host;
                    return Ok(Outcome::HostCall { import: idx as u32, args });
                }
                if fuel == 0 {
                    $redo;
                    save!(pc - 1);
                    return Ok(Outcome::Yield);
                }
                fuel -= 1;
                if frames.len() >= MAX_FRAMES {
                    return Err("call stack exhausted");
                }
                frames.push(Frame { func: fi as u32, pc: pc as u32, fp: fp as u32 });
                fi = idx - m.imports as usize;
                f = &m.funcs[fi];
                ops = &f.ops;
                fp = sp - f.nparams as usize;
                base = fp + f.nlocals as usize;
                let need = base + f.max_height as usize + SLACK;
                if need > MAX_STACK {
                    return Err("value stack exhausted");
                }
                if s.len() < need {
                    let grown = need.max(s.len() * 2).min(MAX_STACK);
                    sv.resize(grown, 0);
                    s = &mut sv[..];
                }
                s[fp + f.nparams as usize..base].fill(0);
                sp = base;
                pc = 0;
            }};
        }

        loop {
            let op = ops[pc];
            pc += 1;
            match op {
                Op::Tick => {
                    if fuel == 0 {
                        save!(pc - 1);
                        return Ok(Outcome::Yield);
                    }
                    fuel -= 1;
                }
                Op::Unreachable => return Err("unreachable"),
                Op::Unsupported => return Err("unsupported instruction"),
                Op::Br(t) => {
                    sp = branch(s, base, sp, t);
                    pc = t.pc as usize;
                }
                Op::BrIf(t) => {
                    if pop!() as u32 != 0 {
                        sp = branch(s, base, sp, t);
                        pc = t.pc as usize;
                    }
                }
                Op::BrIfNot(to) => {
                    if pop!() as u32 == 0 {
                        pc = to as usize;
                    }
                }
                Op::Jump(to) => pc = to as usize,
                Op::BrTable { start, len } => {
                    let i = (pop!() as u32).min(len - 1);
                    let t = f.table[(start + i) as usize];
                    sp = branch(s, base, sp, t);
                    pc = t.pc as usize;
                }
                Op::Return => {
                    let n = f.nresults as usize;
                    s.copy_within(sp - n..sp, fp);
                    sp = fp + n;
                    match frames.pop() {
                        None => {
                            *state = State::Idle;
                            return Ok(Outcome::Done(s[..n].to_vec()));
                        }
                        Some(fr) => {
                            fi = fr.func as usize;
                            f = &m.funcs[fi];
                            ops = &f.ops;
                            pc = fr.pc as usize;
                            fp = fr.fp as usize;
                            base = fp + f.nlocals as usize;
                        }
                    }
                }
                Op::Call(idx) => call!(idx, ()),
                Op::CallIndirect { ty, table } => {
                    let i = pop!() as u32 as usize;
                    let callee = match tables[table as usize].get(i) {
                        None => return Err("undefined table element"),
                        Some(None) => return Err("uninitialized table element"),
                        Some(Some(c)) => *c,
                    };
                    if m.types[m.func_types[callee as usize] as usize] != m.types[ty as usize] {
                        return Err("indirect call type mismatch");
                    }
                    call!(callee, push!(i as u64));
                }
                Op::Drop => sp -= 1,
                Op::Select => {
                    let c = pop!();
                    let b = pop!();
                    if c as u32 == 0 {
                        s[sp - 1] = b;
                    }
                }
                Op::LocalGet(i) => push!(s[fp + i as usize]),
                Op::LocalSet(i) => {
                    let v = pop!();
                    s[fp + i as usize] = v;
                }
                Op::LocalTee(i) => s[fp + i as usize] = s[sp - 1],
                Op::GlobalGet(i) => push!(globals[i as usize]),
                Op::GlobalSet(i) => globals[i as usize] = pop!(),
                Op::I32Const(v) => push!(v as u32 as u64),
                Op::I64Const(v) => push!(v as u64),
                Op::F32Const(v) => push!(v as u64),
                Op::F64Const(v) => push!(v),

                Op::MemorySize => push!((mem.len() / PAGE) as u64),
                Op::MemoryGrow => {
                    let old = mem.len() / PAGE;
                    let new = old as u64 + pop!() as u32 as u64;
                    if new > *max_pages as u64 {
                        push!(u32::MAX as u64);
                    } else {
                        lm.grow(new as usize * PAGE, *max_pages as usize * PAGE);
                        mem = &mut lm.buf[..lm.len];
                        push!(old as u64);
                    }
                }
                Op::MemoryCopy => {
                    let n = pop!() as u32 as usize;
                    let src = pop!() as u32 as usize;
                    let dst = pop!() as u32 as usize;
                    if src + n > mem.len() || dst + n > mem.len() {
                        return Err("out of bounds memory access");
                    }
                    mem.copy_within(src..src + n, dst);
                }
                Op::MemoryFill => {
                    let n = pop!() as u32 as usize;
                    let val = pop!() as u8;
                    let dst = pop!() as u32 as usize;
                    mem.get_mut(dst..dst + n).ok_or("out of bounds memory access")?.fill(val);
                }
                Op::MemoryInit(seg) => {
                    let n = pop!() as u32 as usize;
                    let src = pop!() as u32 as usize;
                    let dst = pop!() as u32 as usize;
                    let data: &[u8] = if dropped[seg as usize] { &[] } else { &m.data[seg as usize] };
                    let from = data.get(src..src + n).ok_or("out of bounds memory access")?;
                    mem.get_mut(dst..dst + n).ok_or("out of bounds memory access")?.copy_from_slice(from);
                }
                Op::DataDrop(seg) => dropped[seg as usize] = true,

                Op::I32Load(o) | Op::F32Load(o) | Op::I64Load32U(o) => load!(o, 4, |b| u32::from_le_bytes(b) as u64),
                Op::I64Load(o) | Op::F64Load(o) => load!(o, 8, |b| u64::from_le_bytes(b)),
                Op::I32Load8U(o) | Op::I64Load8U(o) => load!(o, 1, |b| b[0] as u64),
                Op::I32Load16U(o) | Op::I64Load16U(o) => load!(o, 2, |b| u16::from_le_bytes(b) as u64),
                Op::I32Load8S(o) => load!(o, 1, |b| ri32(b[0] as i8 as i32)),
                Op::I32Load16S(o) => load!(o, 2, |b| ri32(i16::from_le_bytes(b) as i32)),
                Op::I64Load8S(o) => load!(o, 1, |b| b[0] as i8 as i64 as u64),
                Op::I64Load16S(o) => load!(o, 2, |b| i16::from_le_bytes(b) as i64 as u64),
                Op::I64Load32S(o) => load!(o, 4, |b| i32::from_le_bytes(b) as i64 as u64),
                Op::I32Store(o) | Op::F32Store(o) | Op::I64Store32(o) => store!(o, 4, |v| v.to_le_bytes()),
                Op::I64Store(o) | Op::F64Store(o) => store!(o, 8, |v| v.to_le_bytes()),
                Op::I32Store8(o) | Op::I64Store8(o) => store!(o, 1, |v| v.to_le_bytes()),
                Op::I32Store16(o) | Op::I64Store16(o) => store!(o, 2, |v| v.to_le_bytes()),

                Op::LocalGet2(x, y) => {
                    push!(s[fp + x as usize]);
                    push!(s[fp + y as usize]);
                }
                Op::LocalAddConst(x, c) => push!((s[fp + x as usize] as u32).wrapping_add(c as u32) as u64),
                Op::LocalTeeGet(x, y) => {
                    s[fp + x as usize] = s[sp - 1];
                    push!(s[fp + y as usize]);
                }
                Op::LocalSetGet(x, y) => {
                    let v = pop!();
                    s[fp + x as usize] = v;
                    push!(s[fp + y as usize]);
                }
                Op::I32AddConst(c) => un!(u32_, ru32, |x| x.wrapping_add(c as u32)),
                Op::I32ShlConst(c) => un!(u32_, ru32, |x| x.wrapping_shl(c as u32)),
                Op::I32AddLocal(y) => {
                    let b = s[fp + y as usize] as u32;
                    un!(u32_, ru32, |x| x.wrapping_add(b))
                }
                Op::I32AddTee(y) => {
                    bin!(u32_, ru32, |a, b| a.wrapping_add(b));
                    s[fp + y as usize] = s[sp - 1];
                }
                Op::I32AddSet(y) => {
                    bin!(u32_, ru32, |a, b| a.wrapping_add(b));
                    let v = pop!();
                    s[fp + y as usize] = v;
                }
                Op::BrIfI32Eqz(t) => brc!(t, |a| u32_(a) == 0),
                Op::BrIfI32Eq(t) => brc!(t, u32_, |a, b| a == b),
                Op::BrIfI32Ne(t) => brc!(t, u32_, |a, b| a != b),
                Op::BrIfI32LtS(t) => brc!(t, i32_, |a, b| a < b),
                Op::BrIfI32LtU(t) => brc!(t, u32_, |a, b| a < b),
                Op::BrIfI32GtS(t) => brc!(t, i32_, |a, b| a > b),
                Op::BrIfI32GtU(t) => brc!(t, u32_, |a, b| a > b),
                Op::BrIfI32LeS(t) => brc!(t, i32_, |a, b| a <= b),
                Op::BrIfI32LeU(t) => brc!(t, u32_, |a, b| a <= b),
                Op::BrIfI32GeS(t) => brc!(t, i32_, |a, b| a >= b),
                Op::BrIfI32GeU(t) => brc!(t, u32_, |a, b| a >= b),

                Op::I32Eqz => un!(u32_, rb, |x| x == 0),
                Op::I32Eq => bin!(u32_, rb, |a, b| a == b),
                Op::I32Ne => bin!(u32_, rb, |a, b| a != b),
                Op::I32LtS => bin!(i32_, rb, |a, b| a < b),
                Op::I32LtU => bin!(u32_, rb, |a, b| a < b),
                Op::I32GtS => bin!(i32_, rb, |a, b| a > b),
                Op::I32GtU => bin!(u32_, rb, |a, b| a > b),
                Op::I32LeS => bin!(i32_, rb, |a, b| a <= b),
                Op::I32LeU => bin!(u32_, rb, |a, b| a <= b),
                Op::I32GeS => bin!(i32_, rb, |a, b| a >= b),
                Op::I32GeU => bin!(u32_, rb, |a, b| a >= b),
                Op::I64Eqz => un!(u64_, rb, |x| x == 0),
                Op::I64Eq => bin!(u64_, rb, |a, b| a == b),
                Op::I64Ne => bin!(u64_, rb, |a, b| a != b),
                Op::I64LtS => bin!(i64_, rb, |a, b| a < b),
                Op::I64LtU => bin!(u64_, rb, |a, b| a < b),
                Op::I64GtS => bin!(i64_, rb, |a, b| a > b),
                Op::I64GtU => bin!(u64_, rb, |a, b| a > b),
                Op::I64LeS => bin!(i64_, rb, |a, b| a <= b),
                Op::I64LeU => bin!(u64_, rb, |a, b| a <= b),
                Op::I64GeS => bin!(i64_, rb, |a, b| a >= b),
                Op::I64GeU => bin!(u64_, rb, |a, b| a >= b),
                Op::F32Eq => bin!(f32_, rb, |a, b| a == b),
                Op::F32Ne => bin!(f32_, rb, |a, b| a != b),
                Op::F32Lt => bin!(f32_, rb, |a, b| a < b),
                Op::F32Gt => bin!(f32_, rb, |a, b| a > b),
                Op::F32Le => bin!(f32_, rb, |a, b| a <= b),
                Op::F32Ge => bin!(f32_, rb, |a, b| a >= b),
                Op::F64Eq => bin!(f64_, rb, |a, b| a == b),
                Op::F64Ne => bin!(f64_, rb, |a, b| a != b),
                Op::F64Lt => bin!(f64_, rb, |a, b| a < b),
                Op::F64Gt => bin!(f64_, rb, |a, b| a > b),
                Op::F64Le => bin!(f64_, rb, |a, b| a <= b),
                Op::F64Ge => bin!(f64_, rb, |a, b| a >= b),

                Op::I32Clz => un!(u32_, ru32, |x| x.leading_zeros()),
                Op::I32Ctz => un!(u32_, ru32, |x| x.trailing_zeros()),
                Op::I32Popcnt => un!(u32_, ru32, |x| x.count_ones()),
                Op::I32Add => bin!(u32_, ru32, |a, b| a.wrapping_add(b)),
                Op::I32Sub => bin!(u32_, ru32, |a, b| a.wrapping_sub(b)),
                Op::I32Mul => bin!(u32_, ru32, |a, b| a.wrapping_mul(b)),
                Op::I32DivS => bin!(i32_, ri32, |a, b| match (a, b) {
                    (_, 0) => return Err("integer divide by zero"),
                    (i32::MIN, -1) => return Err("integer overflow"),
                    _ => a / b,
                }),
                Op::I32DivU => bin!(u32_, ru32, |a, b| a.checked_div(b).ok_or("integer divide by zero")?),
                Op::I32RemS => bin!(i32_, ri32, |a, b| if b == 0 { return Err("integer divide by zero") } else { a.wrapping_rem(b) }),
                Op::I32RemU => bin!(u32_, ru32, |a, b| a.checked_rem(b).ok_or("integer divide by zero")?),
                Op::I32And => bin!(u32_, ru32, |a, b| a & b),
                Op::I32Or => bin!(u32_, ru32, |a, b| a | b),
                Op::I32Xor => bin!(u32_, ru32, |a, b| a ^ b),
                Op::I32Shl => bin!(u32_, ru32, |a, b| a.wrapping_shl(b)),
                Op::I32ShrS => bin!(u32_, ru32, |a, b| (a as i32).wrapping_shr(b) as u32),
                Op::I32ShrU => bin!(u32_, ru32, |a, b| a.wrapping_shr(b)),
                Op::I32Rotl => bin!(u32_, ru32, |a, b| a.rotate_left(b % 32)),
                Op::I32Rotr => bin!(u32_, ru32, |a, b| a.rotate_right(b % 32)),
                Op::I64Clz => un!(u64_, ru64, |x| x.leading_zeros() as u64),
                Op::I64Ctz => un!(u64_, ru64, |x| x.trailing_zeros() as u64),
                Op::I64Popcnt => un!(u64_, ru64, |x| x.count_ones() as u64),
                Op::I64Add => bin!(u64_, ru64, |a, b| a.wrapping_add(b)),
                Op::I64Sub => bin!(u64_, ru64, |a, b| a.wrapping_sub(b)),
                Op::I64Mul => bin!(u64_, ru64, |a, b| a.wrapping_mul(b)),
                Op::I64DivS => bin!(i64_, ri64, |a, b| match (a, b) {
                    (_, 0) => return Err("integer divide by zero"),
                    (i64::MIN, -1) => return Err("integer overflow"),
                    _ => a / b,
                }),
                Op::I64DivU => bin!(u64_, ru64, |a, b| a.checked_div(b).ok_or("integer divide by zero")?),
                Op::I64RemS => bin!(i64_, ri64, |a, b| if b == 0 { return Err("integer divide by zero") } else { a.wrapping_rem(b) }),
                Op::I64RemU => bin!(u64_, ru64, |a, b| a.checked_rem(b).ok_or("integer divide by zero")?),
                Op::I64And => bin!(u64_, ru64, |a, b| a & b),
                Op::I64Or => bin!(u64_, ru64, |a, b| a | b),
                Op::I64Xor => bin!(u64_, ru64, |a, b| a ^ b),
                Op::I64Shl => bin!(u64_, ru64, |a, b| a.wrapping_shl(b as u32)),
                Op::I64ShrS => bin!(u64_, ru64, |a, b| (a as i64).wrapping_shr(b as u32) as u64),
                Op::I64ShrU => bin!(u64_, ru64, |a, b| a.wrapping_shr(b as u32)),
                Op::I64Rotl => bin!(u64_, ru64, |a, b| a.rotate_left((b % 64) as u32)),
                Op::I64Rotr => bin!(u64_, ru64, |a, b| a.rotate_right((b % 64) as u32)),

                Op::F32Abs => un!(f32_, rf32, |x| x.abs()),
                Op::F32Neg => un!(f32_, rf32, |x| -x),
                Op::F32Ceil => un!(f32_, rf32, |x| x.ceil()),
                Op::F32Floor => un!(f32_, rf32, |x| x.floor()),
                Op::F32Trunc => un!(f32_, rf32, |x| x.trunc()),
                Op::F32Nearest => un!(f32_, rf32, |x| x.round_ties_even()),
                Op::F32Sqrt => un!(f32_, rf32, |x| x.sqrt()),
                Op::F32Add => bin!(f32_, rf32, |a, b| a + b),
                Op::F32Sub => bin!(f32_, rf32, |a, b| a - b),
                Op::F32Mul => bin!(f32_, rf32, |a, b| a * b),
                Op::F32Div => bin!(f32_, rf32, |a, b| a / b),
                Op::F32Min => bin!(f32_, rf32, |a, b| fmin32(a, b)),
                Op::F32Max => bin!(f32_, rf32, |a, b| fmax32(a, b)),
                Op::F32Copysign => bin!(f32_, rf32, |a, b| a.copysign(b)),
                Op::F64Abs => un!(f64_, rf64, |x| x.abs()),
                Op::F64Neg => un!(f64_, rf64, |x| -x),
                Op::F64Ceil => un!(f64_, rf64, |x| x.ceil()),
                Op::F64Floor => un!(f64_, rf64, |x| x.floor()),
                Op::F64Trunc => un!(f64_, rf64, |x| x.trunc()),
                Op::F64Nearest => un!(f64_, rf64, |x| x.round_ties_even()),
                Op::F64Sqrt => un!(f64_, rf64, |x| x.sqrt()),
                Op::F64Add => bin!(f64_, rf64, |a, b| a + b),
                Op::F64Sub => bin!(f64_, rf64, |a, b| a - b),
                Op::F64Mul => bin!(f64_, rf64, |a, b| a * b),
                Op::F64Div => bin!(f64_, rf64, |a, b| a / b),
                Op::F64Min => bin!(f64_, rf64, |a, b| fmin64(a, b)),
                Op::F64Max => bin!(f64_, rf64, |a, b| fmax64(a, b)),
                Op::F64Copysign => bin!(f64_, rf64, |a, b| a.copysign(b)),

                Op::I32WrapI64 => un!(u64_, ru32, |x| x as u32),
                Op::I32TruncF32S => un!(f32_, ri32, |x| trunc(x as f64, I32R.0, I32R.1)? as i32),
                Op::I32TruncF32U => un!(f32_, ru32, |x| trunc(x as f64, U32R.0, U32R.1)? as u32),
                Op::I32TruncF64S => un!(f64_, ri32, |x| trunc(x, I32R.0, I32R.1)? as i32),
                Op::I32TruncF64U => un!(f64_, ru32, |x| trunc(x, U32R.0, U32R.1)? as u32),
                Op::I64ExtendI32S => un!(i32_, ri64, |x| x as i64),
                Op::I64ExtendI32U => un!(u32_, ru64, |x| x as u64),
                Op::I64TruncF32S => un!(f32_, ri64, |x| trunc(x as f64, I64R.0, I64R.1)? as i64),
                Op::I64TruncF32U => un!(f32_, ru64, |x| trunc(x as f64, U64R.0, U64R.1)? as u64),
                Op::I64TruncF64S => un!(f64_, ri64, |x| trunc(x, I64R.0, I64R.1)? as i64),
                Op::I64TruncF64U => un!(f64_, ru64, |x| trunc(x, U64R.0, U64R.1)? as u64),
                Op::F32ConvertI32S => un!(i32_, rf32, |x| x as f32),
                Op::F32ConvertI32U => un!(u32_, rf32, |x| x as f32),
                Op::F32ConvertI64S => un!(i64_, rf32, |x| x as f32),
                Op::F32ConvertI64U => un!(u64_, rf32, |x| x as f32),
                Op::F32DemoteF64 => un!(f64_, rf32, |x| x as f32),
                Op::F64ConvertI32S => un!(i32_, rf64, |x| x as f64),
                Op::F64ConvertI32U => un!(u32_, rf64, |x| x as f64),
                Op::F64ConvertI64S => un!(i64_, rf64, |x| x as f64),
                Op::F64ConvertI64U => un!(u64_, rf64, |x| x as f64),
                Op::F64PromoteF32 => un!(f32_, rf64, |x| x as f64),
                // Values are stored as raw bits, so reinterpreting is free.
                Op::I32ReinterpretF32 | Op::I64ReinterpretF64 | Op::F32ReinterpretI32 | Op::F64ReinterpretI64 => {}
                Op::I32Extend8S => un!(u32_, ri32, |x| x as i8 as i32),
                Op::I32Extend16S => un!(u32_, ri32, |x| x as i16 as i32),
                Op::I64Extend8S => un!(u64_, ri64, |x| x as i8 as i64),
                Op::I64Extend16S => un!(u64_, ri64, |x| x as i16 as i64),
                Op::I64Extend32S => un!(u64_, ri64, |x| x as i32 as i64),
                // Rust's `as` already saturates and maps NaN to 0, exactly like wasm.
                Op::I32TruncSatF32S => un!(f32_, ri32, |x| x as i32),
                Op::I32TruncSatF32U => un!(f32_, ru32, |x| x as u32),
                Op::I32TruncSatF64S => un!(f64_, ri32, |x| x as i32),
                Op::I32TruncSatF64U => un!(f64_, ru32, |x| x as u32),
                Op::I64TruncSatF32S => un!(f32_, ri64, |x| x as i64),
                Op::I64TruncSatF32U => un!(f32_, ru64, |x| x as u64),
                Op::I64TruncSatF64S => un!(f64_, ri64, |x| x as i64),
                Op::I64TruncSatF64U => un!(f64_, ru64, |x| x as u64),
            }
        }
    }
}
