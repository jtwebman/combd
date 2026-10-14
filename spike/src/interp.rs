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
            Br(Target), Jump(u32), BrTable { start: u32, len: u32 },
            MemorySize, MemoryGrow, MemoryCopy, MemoryFill, MemoryInit(u32), DataDrop(u32),
            // Register forms: operands are frame slots (locals and stack slots alike).
            Copy { d: u32, src: u32 }, ConstTo { d: u32, v: u64 }, SetSp(u32),
            // Calls and returns carry their stack height, so no `SetSp` is needed first.
            CallAt(u32, u32), CallIndirectAt { ty: u32, table: u32, h: u32 }, ReturnAt(u32),
            BrIfR { c: u32, t: u32 }, IfR { c: u32, to: u32 },
            GlobalGetR { d: u32, g: u32 }, GlobalSetR { g: u32, a: u32 },
            SelectR { d: u32, a: u32, b: u32, c: u16 },
            Store32RI { a: u32, imm: i32, off: u32 }, Store64RI { a: u32, imm: i32, off: u32 },
            Store8RI { a: u32, imm: i32, off: u32 }, Store16RI { a: u32, imm: i32, off: u32 },
            // A compare fused with the `br_if` or `if` that consumes it.
            BrIfI32EqRR { a: u32, b: u32, t: u32 }, BrIfI32EqRI { a: u32, imm: i32, t: u32 }, IfI32EqRR { a: u32, b: u32, to: u32 }, IfI32EqRI { a: u32, imm: i32, to: u32 },
            BrIfI32NeRR { a: u32, b: u32, t: u32 }, BrIfI32NeRI { a: u32, imm: i32, t: u32 }, IfI32NeRR { a: u32, b: u32, to: u32 }, IfI32NeRI { a: u32, imm: i32, to: u32 },
            BrIfI32LtSRR { a: u32, b: u32, t: u32 }, BrIfI32LtSRI { a: u32, imm: i32, t: u32 }, IfI32LtSRR { a: u32, b: u32, to: u32 }, IfI32LtSRI { a: u32, imm: i32, to: u32 },
            BrIfI32LtURR { a: u32, b: u32, t: u32 }, BrIfI32LtURI { a: u32, imm: i32, t: u32 }, IfI32LtURR { a: u32, b: u32, to: u32 }, IfI32LtURI { a: u32, imm: i32, to: u32 },
            BrIfI32GtSRR { a: u32, b: u32, t: u32 }, BrIfI32GtSRI { a: u32, imm: i32, t: u32 }, IfI32GtSRR { a: u32, b: u32, to: u32 }, IfI32GtSRI { a: u32, imm: i32, to: u32 },
            BrIfI32GtURR { a: u32, b: u32, t: u32 }, BrIfI32GtURI { a: u32, imm: i32, t: u32 }, IfI32GtURR { a: u32, b: u32, to: u32 }, IfI32GtURI { a: u32, imm: i32, to: u32 },
            BrIfI32LeSRR { a: u32, b: u32, t: u32 }, BrIfI32LeSRI { a: u32, imm: i32, t: u32 }, IfI32LeSRR { a: u32, b: u32, to: u32 }, IfI32LeSRI { a: u32, imm: i32, to: u32 },
            BrIfI32LeURR { a: u32, b: u32, t: u32 }, BrIfI32LeURI { a: u32, imm: i32, t: u32 }, IfI32LeURR { a: u32, b: u32, to: u32 }, IfI32LeURI { a: u32, imm: i32, to: u32 },
            BrIfI32GeSRR { a: u32, b: u32, t: u32 }, BrIfI32GeSRI { a: u32, imm: i32, t: u32 }, IfI32GeSRR { a: u32, b: u32, to: u32 }, IfI32GeSRI { a: u32, imm: i32, to: u32 },
            BrIfI32GeURR { a: u32, b: u32, t: u32 }, BrIfI32GeURI { a: u32, imm: i32, t: u32 }, IfI32GeURR { a: u32, b: u32, to: u32 }, IfI32GeURI { a: u32, imm: i32, to: u32 },
            BrIfI64EqRR { a: u32, b: u32, t: u32 }, BrIfI64EqRI { a: u32, imm: i32, t: u32 }, IfI64EqRR { a: u32, b: u32, to: u32 }, IfI64EqRI { a: u32, imm: i32, to: u32 },
            BrIfI64NeRR { a: u32, b: u32, t: u32 }, BrIfI64NeRI { a: u32, imm: i32, t: u32 }, IfI64NeRR { a: u32, b: u32, to: u32 }, IfI64NeRI { a: u32, imm: i32, to: u32 },
            BrIfI64LtSRR { a: u32, b: u32, t: u32 }, BrIfI64LtSRI { a: u32, imm: i32, t: u32 }, IfI64LtSRR { a: u32, b: u32, to: u32 }, IfI64LtSRI { a: u32, imm: i32, to: u32 },
            BrIfI64LtURR { a: u32, b: u32, t: u32 }, BrIfI64LtURI { a: u32, imm: i32, t: u32 }, IfI64LtURR { a: u32, b: u32, to: u32 }, IfI64LtURI { a: u32, imm: i32, to: u32 },
            BrIfI64GtSRR { a: u32, b: u32, t: u32 }, BrIfI64GtSRI { a: u32, imm: i32, t: u32 }, IfI64GtSRR { a: u32, b: u32, to: u32 }, IfI64GtSRI { a: u32, imm: i32, to: u32 },
            BrIfI64GtURR { a: u32, b: u32, t: u32 }, BrIfI64GtURI { a: u32, imm: i32, t: u32 }, IfI64GtURR { a: u32, b: u32, to: u32 }, IfI64GtURI { a: u32, imm: i32, to: u32 },
            BrIfI64LeSRR { a: u32, b: u32, t: u32 }, BrIfI64LeSRI { a: u32, imm: i32, t: u32 }, IfI64LeSRR { a: u32, b: u32, to: u32 }, IfI64LeSRI { a: u32, imm: i32, to: u32 },
            BrIfI64LeURR { a: u32, b: u32, t: u32 }, BrIfI64LeURI { a: u32, imm: i32, t: u32 }, IfI64LeURR { a: u32, b: u32, to: u32 }, IfI64LeURI { a: u32, imm: i32, to: u32 },
            BrIfI64GeSRR { a: u32, b: u32, t: u32 }, BrIfI64GeSRI { a: u32, imm: i32, t: u32 }, IfI64GeSRR { a: u32, b: u32, to: u32 }, IfI64GeSRI { a: u32, imm: i32, to: u32 },
            BrIfI64GeURR { a: u32, b: u32, t: u32 }, BrIfI64GeURI { a: u32, imm: i32, t: u32 }, IfI64GeURR { a: u32, b: u32, to: u32 }, IfI64GeURI { a: u32, imm: i32, to: u32 },
            BrIfI32EqzR { a: u32, t: u32 }, IfI32EqzR { a: u32, to: u32 },
            BrIfI64EqzR { a: u32, t: u32 }, IfI64EqzR { a: u32, to: u32 },
            I32AddRR { d: u32, a: u32, b: u32 }, I32AddRI { d: u32, a: u32, imm: i32 }, I32SubRR { d: u32, a: u32, b: u32 }, I32SubRI { d: u32, a: u32, imm: i32 }, I32MulRR { d: u32, a: u32, b: u32 }, I32MulRI { d: u32, a: u32, imm: i32 }, I32AndRR { d: u32, a: u32, b: u32 }, I32AndRI { d: u32, a: u32, imm: i32 }, I32OrRR { d: u32, a: u32, b: u32 }, I32OrRI { d: u32, a: u32, imm: i32 }, I32XorRR { d: u32, a: u32, b: u32 }, I32XorRI { d: u32, a: u32, imm: i32 }, I32ShlRR { d: u32, a: u32, b: u32 }, I32ShlRI { d: u32, a: u32, imm: i32 }, I32ShrSRR { d: u32, a: u32, b: u32 }, I32ShrSRI { d: u32, a: u32, imm: i32 }, I32ShrURR { d: u32, a: u32, b: u32 }, I32ShrURI { d: u32, a: u32, imm: i32 }, I32EqRR { d: u32, a: u32, b: u32 }, I32EqRI { d: u32, a: u32, imm: i32 }, I32NeRR { d: u32, a: u32, b: u32 }, I32NeRI { d: u32, a: u32, imm: i32 }, I32LtSRR { d: u32, a: u32, b: u32 }, I32LtSRI { d: u32, a: u32, imm: i32 }, I32LtURR { d: u32, a: u32, b: u32 }, I32LtURI { d: u32, a: u32, imm: i32 }, I32GtSRR { d: u32, a: u32, b: u32 }, I32GtSRI { d: u32, a: u32, imm: i32 }, I32GtURR { d: u32, a: u32, b: u32 }, I32GtURI { d: u32, a: u32, imm: i32 }, I32LeSRR { d: u32, a: u32, b: u32 }, I32LeSRI { d: u32, a: u32, imm: i32 }, I32LeURR { d: u32, a: u32, b: u32 }, I32LeURI { d: u32, a: u32, imm: i32 }, I32GeSRR { d: u32, a: u32, b: u32 }, I32GeSRI { d: u32, a: u32, imm: i32 }, I32GeURR { d: u32, a: u32, b: u32 }, I32GeURI { d: u32, a: u32, imm: i32 }, I64AddRR { d: u32, a: u32, b: u32 }, I64AddRI { d: u32, a: u32, imm: i32 }, I64SubRR { d: u32, a: u32, b: u32 }, I64SubRI { d: u32, a: u32, imm: i32 }, I64MulRR { d: u32, a: u32, b: u32 }, I64MulRI { d: u32, a: u32, imm: i32 }, I64AndRR { d: u32, a: u32, b: u32 }, I64AndRI { d: u32, a: u32, imm: i32 }, I64OrRR { d: u32, a: u32, b: u32 }, I64OrRI { d: u32, a: u32, imm: i32 }, I64XorRR { d: u32, a: u32, b: u32 }, I64XorRI { d: u32, a: u32, imm: i32 }, I64ShlRR { d: u32, a: u32, b: u32 }, I64ShlRI { d: u32, a: u32, imm: i32 }, I64ShrSRR { d: u32, a: u32, b: u32 }, I64ShrSRI { d: u32, a: u32, imm: i32 }, I64ShrURR { d: u32, a: u32, b: u32 }, I64ShrURI { d: u32, a: u32, imm: i32 }, I64EqRR { d: u32, a: u32, b: u32 }, I64EqRI { d: u32, a: u32, imm: i32 }, I64NeRR { d: u32, a: u32, b: u32 }, I64NeRI { d: u32, a: u32, imm: i32 }, I64LtSRR { d: u32, a: u32, b: u32 }, I64LtSRI { d: u32, a: u32, imm: i32 }, I64LtURR { d: u32, a: u32, b: u32 }, I64LtURI { d: u32, a: u32, imm: i32 }, I64GtSRR { d: u32, a: u32, b: u32 }, I64GtSRI { d: u32, a: u32, imm: i32 }, I64GtURR { d: u32, a: u32, b: u32 }, I64GtURI { d: u32, a: u32, imm: i32 }, I64LeSRR { d: u32, a: u32, b: u32 }, I64LeSRI { d: u32, a: u32, imm: i32 }, I64LeURR { d: u32, a: u32, b: u32 }, I64LeURI { d: u32, a: u32, imm: i32 }, I64GeSRR { d: u32, a: u32, b: u32 }, I64GeSRI { d: u32, a: u32, imm: i32 }, I64GeURR { d: u32, a: u32, b: u32 }, I64GeURI { d: u32, a: u32, imm: i32 },
            I32EqzR { d: u32, a: u32 }, I64EqzR { d: u32, a: u32 }, I32WrapI64R { d: u32, a: u32 }, I64ExtendI32UR { d: u32, a: u32 }, I64ExtendI32SR { d: u32, a: u32 },
            I32LoadR { d: u32, a: u32, off: u32 }, I64LoadR { d: u32, a: u32, off: u32 }, Load8UR { d: u32, a: u32, off: u32 }, Load16UR { d: u32, a: u32, off: u32 }, I32Load8SR { d: u32, a: u32, off: u32 }, I32Load16SR { d: u32, a: u32, off: u32 },
            Store32R { a: u32, v: u32, off: u32 }, Store64R { a: u32, v: u32, off: u32 }, Store8R { a: u32, v: u32, off: u32 }, Store16R { a: u32, v: u32, off: u32 },
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
            Op::Br(t) => t.pc = pc,
            Op::Jump(to) | Op::IfR { to, .. } => *to = pc,
            Op::IfI32EqRR { to, .. } | Op::IfI32EqRI { to, .. }
            | Op::IfI32NeRR { to, .. } | Op::IfI32NeRI { to, .. }
            | Op::IfI32LtSRR { to, .. } | Op::IfI32LtSRI { to, .. }
            | Op::IfI32LtURR { to, .. } | Op::IfI32LtURI { to, .. }
            | Op::IfI32GtSRR { to, .. } | Op::IfI32GtSRI { to, .. }
            | Op::IfI32GtURR { to, .. } | Op::IfI32GtURI { to, .. }
            | Op::IfI32LeSRR { to, .. } | Op::IfI32LeSRI { to, .. }
            | Op::IfI32LeURR { to, .. } | Op::IfI32LeURI { to, .. }
            | Op::IfI32GeSRR { to, .. } | Op::IfI32GeSRI { to, .. }
            | Op::IfI32GeURR { to, .. } | Op::IfI32GeURI { to, .. }
            | Op::IfI64EqRR { to, .. } | Op::IfI64EqRI { to, .. }
            | Op::IfI64NeRR { to, .. } | Op::IfI64NeRI { to, .. }
            | Op::IfI64LtSRR { to, .. } | Op::IfI64LtSRI { to, .. }
            | Op::IfI64LtURR { to, .. } | Op::IfI64LtURI { to, .. }
            | Op::IfI64GtSRR { to, .. } | Op::IfI64GtSRI { to, .. }
            | Op::IfI64GtURR { to, .. } | Op::IfI64GtURI { to, .. }
            | Op::IfI64LeSRR { to, .. } | Op::IfI64LeSRI { to, .. }
            | Op::IfI64LeURR { to, .. } | Op::IfI64LeURI { to, .. }
            | Op::IfI64GeSRR { to, .. } | Op::IfI64GeSRI { to, .. }
            | Op::IfI64GeURR { to, .. } | Op::IfI64GeURI { to, .. }
            | Op::IfI32EqzR { to, .. }
            | Op::IfI64EqzR { to, .. } => *to = pc,
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

/// One entry of the translator's virtual operand stack: the value lives in a
/// frame slot (a local, or its own stack slot), or is a constant not yet
/// written anywhere.
#[derive(Clone, Copy, PartialEq)]
enum V {
    Slot(u32),
    Const(u64),
}

/// Register translator state for one function. Frame slot `i` is local `i`;
/// the stack value at height `h` lives in slot `nl + h`.
struct Regs {
    nl: u32,
    vs: Vec<V>,
    /// Whether the runtime `sp` matches `vs`. Register ops don't move it.
    sp_ok: bool,
    /// The last op, if it was a register op: (index, the slot it wrote).
    last_def: Option<(usize, u32)>,
}

impl Regs {
    fn own(&self, h: usize) -> u32 {
        self.nl + h as u32
    }

    /// Write every virtual entry into its own stack slot.
    fn materialize(&mut self, f: &mut Func) {
        for h in 0..self.vs.len() {
            let own = self.own(h);
            match self.vs[h] {
                V::Slot(r) if r == own => {}
                V::Slot(r) => f.ops.push(Op::Copy { d: own, src: r }),
                V::Const(v) => f.ops.push(Op::ConstTo { d: own, v }),
            }
            self.vs[h] = V::Slot(own);
        }
    }

    fn ensure_sp(&mut self, f: &mut Func) {
        if !self.sp_ok {
            f.ops.push(Op::SetSp(self.vs.len() as u32));
            self.sp_ok = true;
        }
    }

    /// The slot holding `v`, which sat at height `h`. A constant is written to
    /// that height's own slot first.
    fn slot(&self, f: &mut Func, v: V, h: usize) -> u32 {
        match v {
            V::Slot(r) => r,
            V::Const(c) => {
                let own = self.own(h);
                f.ops.push(Op::ConstTo { d: own, v: c });
                own
            }
        }
    }

    /// After a stack op: every entry is physical and `sp` is right.
    fn sync(&mut self, h: usize) {
        self.vs.truncate(h);
        while self.vs.len() < h {
            let own = self.own(self.vs.len());
            self.vs.push(V::Slot(own));
        }
        self.sp_ok = true;
        self.last_def = None;
    }
}

/// A compare whose only use is the next `br_if` becomes one branch op.
fn fuse_cmp_br(cmp: Op, t: u32) -> Option<Op> {
    Some(match cmp {
        Op::I32EqRR { a, b, .. } => Op::BrIfI32EqRR { a, b, t },
        Op::I32EqRI { a, imm, .. } => Op::BrIfI32EqRI { a, imm, t },
        Op::I32NeRR { a, b, .. } => Op::BrIfI32NeRR { a, b, t },
        Op::I32NeRI { a, imm, .. } => Op::BrIfI32NeRI { a, imm, t },
        Op::I32LtSRR { a, b, .. } => Op::BrIfI32LtSRR { a, b, t },
        Op::I32LtSRI { a, imm, .. } => Op::BrIfI32LtSRI { a, imm, t },
        Op::I32LtURR { a, b, .. } => Op::BrIfI32LtURR { a, b, t },
        Op::I32LtURI { a, imm, .. } => Op::BrIfI32LtURI { a, imm, t },
        Op::I32GtSRR { a, b, .. } => Op::BrIfI32GtSRR { a, b, t },
        Op::I32GtSRI { a, imm, .. } => Op::BrIfI32GtSRI { a, imm, t },
        Op::I32GtURR { a, b, .. } => Op::BrIfI32GtURR { a, b, t },
        Op::I32GtURI { a, imm, .. } => Op::BrIfI32GtURI { a, imm, t },
        Op::I32LeSRR { a, b, .. } => Op::BrIfI32LeSRR { a, b, t },
        Op::I32LeSRI { a, imm, .. } => Op::BrIfI32LeSRI { a, imm, t },
        Op::I32LeURR { a, b, .. } => Op::BrIfI32LeURR { a, b, t },
        Op::I32LeURI { a, imm, .. } => Op::BrIfI32LeURI { a, imm, t },
        Op::I32GeSRR { a, b, .. } => Op::BrIfI32GeSRR { a, b, t },
        Op::I32GeSRI { a, imm, .. } => Op::BrIfI32GeSRI { a, imm, t },
        Op::I32GeURR { a, b, .. } => Op::BrIfI32GeURR { a, b, t },
        Op::I32GeURI { a, imm, .. } => Op::BrIfI32GeURI { a, imm, t },
        Op::I64EqRR { a, b, .. } => Op::BrIfI64EqRR { a, b, t },
        Op::I64EqRI { a, imm, .. } => Op::BrIfI64EqRI { a, imm, t },
        Op::I64NeRR { a, b, .. } => Op::BrIfI64NeRR { a, b, t },
        Op::I64NeRI { a, imm, .. } => Op::BrIfI64NeRI { a, imm, t },
        Op::I64LtSRR { a, b, .. } => Op::BrIfI64LtSRR { a, b, t },
        Op::I64LtSRI { a, imm, .. } => Op::BrIfI64LtSRI { a, imm, t },
        Op::I64LtURR { a, b, .. } => Op::BrIfI64LtURR { a, b, t },
        Op::I64LtURI { a, imm, .. } => Op::BrIfI64LtURI { a, imm, t },
        Op::I64GtSRR { a, b, .. } => Op::BrIfI64GtSRR { a, b, t },
        Op::I64GtSRI { a, imm, .. } => Op::BrIfI64GtSRI { a, imm, t },
        Op::I64GtURR { a, b, .. } => Op::BrIfI64GtURR { a, b, t },
        Op::I64GtURI { a, imm, .. } => Op::BrIfI64GtURI { a, imm, t },
        Op::I64LeSRR { a, b, .. } => Op::BrIfI64LeSRR { a, b, t },
        Op::I64LeSRI { a, imm, .. } => Op::BrIfI64LeSRI { a, imm, t },
        Op::I64LeURR { a, b, .. } => Op::BrIfI64LeURR { a, b, t },
        Op::I64LeURI { a, imm, .. } => Op::BrIfI64LeURI { a, imm, t },
        Op::I64GeSRR { a, b, .. } => Op::BrIfI64GeSRR { a, b, t },
        Op::I64GeSRI { a, imm, .. } => Op::BrIfI64GeSRI { a, imm, t },
        Op::I64GeURR { a, b, .. } => Op::BrIfI64GeURR { a, b, t },
        Op::I64GeURI { a, imm, .. } => Op::BrIfI64GeURI { a, imm, t },
        Op::I32EqzR { a, .. } => Op::BrIfI32EqzR { a, t },
        Op::I64EqzR { a, .. } => Op::BrIfI64EqzR { a, t },
        _ => return None,
    })
}

/// A compare whose only use is the next `if` becomes one op.
fn fuse_cmp_if(cmp: Op, to: u32) -> Option<Op> {
    Some(match cmp {
        Op::I32EqRR { a, b, .. } => Op::IfI32EqRR { a, b, to },
        Op::I32EqRI { a, imm, .. } => Op::IfI32EqRI { a, imm, to },
        Op::I32NeRR { a, b, .. } => Op::IfI32NeRR { a, b, to },
        Op::I32NeRI { a, imm, .. } => Op::IfI32NeRI { a, imm, to },
        Op::I32LtSRR { a, b, .. } => Op::IfI32LtSRR { a, b, to },
        Op::I32LtSRI { a, imm, .. } => Op::IfI32LtSRI { a, imm, to },
        Op::I32LtURR { a, b, .. } => Op::IfI32LtURR { a, b, to },
        Op::I32LtURI { a, imm, .. } => Op::IfI32LtURI { a, imm, to },
        Op::I32GtSRR { a, b, .. } => Op::IfI32GtSRR { a, b, to },
        Op::I32GtSRI { a, imm, .. } => Op::IfI32GtSRI { a, imm, to },
        Op::I32GtURR { a, b, .. } => Op::IfI32GtURR { a, b, to },
        Op::I32GtURI { a, imm, .. } => Op::IfI32GtURI { a, imm, to },
        Op::I32LeSRR { a, b, .. } => Op::IfI32LeSRR { a, b, to },
        Op::I32LeSRI { a, imm, .. } => Op::IfI32LeSRI { a, imm, to },
        Op::I32LeURR { a, b, .. } => Op::IfI32LeURR { a, b, to },
        Op::I32LeURI { a, imm, .. } => Op::IfI32LeURI { a, imm, to },
        Op::I32GeSRR { a, b, .. } => Op::IfI32GeSRR { a, b, to },
        Op::I32GeSRI { a, imm, .. } => Op::IfI32GeSRI { a, imm, to },
        Op::I32GeURR { a, b, .. } => Op::IfI32GeURR { a, b, to },
        Op::I32GeURI { a, imm, .. } => Op::IfI32GeURI { a, imm, to },
        Op::I64EqRR { a, b, .. } => Op::IfI64EqRR { a, b, to },
        Op::I64EqRI { a, imm, .. } => Op::IfI64EqRI { a, imm, to },
        Op::I64NeRR { a, b, .. } => Op::IfI64NeRR { a, b, to },
        Op::I64NeRI { a, imm, .. } => Op::IfI64NeRI { a, imm, to },
        Op::I64LtSRR { a, b, .. } => Op::IfI64LtSRR { a, b, to },
        Op::I64LtSRI { a, imm, .. } => Op::IfI64LtSRI { a, imm, to },
        Op::I64LtURR { a, b, .. } => Op::IfI64LtURR { a, b, to },
        Op::I64LtURI { a, imm, .. } => Op::IfI64LtURI { a, imm, to },
        Op::I64GtSRR { a, b, .. } => Op::IfI64GtSRR { a, b, to },
        Op::I64GtSRI { a, imm, .. } => Op::IfI64GtSRI { a, imm, to },
        Op::I64GtURR { a, b, .. } => Op::IfI64GtURR { a, b, to },
        Op::I64GtURI { a, imm, .. } => Op::IfI64GtURI { a, imm, to },
        Op::I64LeSRR { a, b, .. } => Op::IfI64LeSRR { a, b, to },
        Op::I64LeSRI { a, imm, .. } => Op::IfI64LeSRI { a, imm, to },
        Op::I64LeURR { a, b, .. } => Op::IfI64LeURR { a, b, to },
        Op::I64LeURI { a, imm, .. } => Op::IfI64LeURI { a, imm, to },
        Op::I64GeSRR { a, b, .. } => Op::IfI64GeSRR { a, b, to },
        Op::I64GeSRI { a, imm, .. } => Op::IfI64GeSRI { a, imm, to },
        Op::I64GeURR { a, b, .. } => Op::IfI64GeURR { a, b, to },
        Op::I64GeURI { a, imm, .. } => Op::IfI64GeURI { a, imm, to },
        Op::I32EqzR { a, .. } => Op::IfI32EqzR { a, to },
        Op::I64EqzR { a, .. } => Op::IfI64EqzR { a, to },
        _ => return None,
    })
}

/// Point a register op's result at slot `x` instead.
fn retarget(op: &mut Op, x: u32) {
    match op {
        Op::GlobalGetR { d, .. } | Op::SelectR { d, .. } => *d = x,
        Op::I32AddRI { d, .. } | Op::I32AddRR { d, .. } | Op::I32AndRI { d, .. } | Op::I32AndRR { d, .. } | Op::I32EqRI { d, .. } | Op::I32EqRR { d, .. } | Op::I32EqzR { d, .. } | Op::I32GeSRI { d, .. } | Op::I32GeSRR { d, .. } | Op::I32GeURI { d, .. } | Op::I32GeURR { d, .. } | Op::I32GtSRI { d, .. } | Op::I32GtSRR { d, .. } | Op::I32GtURI { d, .. } | Op::I32GtURR { d, .. } | Op::I32LeSRI { d, .. } | Op::I32LeSRR { d, .. } | Op::I32LeURI { d, .. } | Op::I32LeURR { d, .. } | Op::I32Load16SR { d, .. } | Op::I32Load8SR { d, .. } | Op::I32LoadR { d, .. } | Op::I32LtSRI { d, .. } | Op::I32LtSRR { d, .. } | Op::I32LtURI { d, .. } | Op::I32LtURR { d, .. } | Op::I32MulRI { d, .. } | Op::I32MulRR { d, .. } | Op::I32NeRI { d, .. } | Op::I32NeRR { d, .. } | Op::I32OrRI { d, .. } | Op::I32OrRR { d, .. } | Op::I32ShlRI { d, .. } | Op::I32ShlRR { d, .. } | Op::I32ShrSRI { d, .. } | Op::I32ShrSRR { d, .. } | Op::I32ShrURI { d, .. } | Op::I32ShrURR { d, .. } | Op::I32SubRI { d, .. } | Op::I32SubRR { d, .. } | Op::I32WrapI64R { d, .. } | Op::I32XorRI { d, .. } | Op::I32XorRR { d, .. } | Op::I64AddRI { d, .. } | Op::I64AddRR { d, .. } | Op::I64AndRI { d, .. } | Op::I64AndRR { d, .. } | Op::I64EqRI { d, .. } | Op::I64EqRR { d, .. } | Op::I64EqzR { d, .. } | Op::I64ExtendI32SR { d, .. } | Op::I64ExtendI32UR { d, .. } | Op::I64GeSRI { d, .. } | Op::I64GeSRR { d, .. } | Op::I64GeURI { d, .. } | Op::I64GeURR { d, .. } | Op::I64GtSRI { d, .. } | Op::I64GtSRR { d, .. } | Op::I64GtURI { d, .. } | Op::I64GtURR { d, .. } | Op::I64LeSRI { d, .. } | Op::I64LeSRR { d, .. } | Op::I64LeURI { d, .. } | Op::I64LeURR { d, .. } | Op::I64LoadR { d, .. } | Op::I64LtSRI { d, .. } | Op::I64LtSRR { d, .. } | Op::I64LtURI { d, .. } | Op::I64LtURR { d, .. } | Op::I64MulRI { d, .. } | Op::I64MulRR { d, .. } | Op::I64NeRI { d, .. } | Op::I64NeRR { d, .. } | Op::I64OrRI { d, .. } | Op::I64OrRR { d, .. } | Op::I64ShlRI { d, .. } | Op::I64ShlRR { d, .. } | Op::I64ShrSRI { d, .. } | Op::I64ShrSRR { d, .. } | Op::I64ShrURI { d, .. } | Op::I64ShrURR { d, .. } | Op::I64SubRI { d, .. } | Op::I64SubRR { d, .. } | Op::I64XorRI { d, .. } | Op::I64XorRR { d, .. } | Op::Load16UR { d, .. } | Op::Load8UR { d, .. } => *d = x,
        _ => {}
    }
}

/// How the register translator handles one operator.
enum Kind {
    /// Not converted yet: materialize the virtual stack, then emit a stack op.
    Stack,
    Get(u32),
    Const(u64),
    Set(u32, bool),
    Drop,
    Bin { rr: fn(u32, u32, u32) -> Op, ri: fn(u32, u32, i32) -> Op, comm: bool, wide: bool },
    Un(fn(u32, u32) -> Op),
    Load(fn(u32, u32, u32) -> Op, u32),
    Store(fn(u32, u32, u32) -> Op, fn(u32, i32, u32) -> Op, u32, bool),
    GlobalGet(u32),
    GlobalSet(u32),
    Select,
    BrIf(u32),
    If,
}

fn classify(op: &Operator) -> Kind {
    match op {
        Operator::LocalGet { local_index } => Kind::Get(*local_index),
        Operator::LocalSet { local_index } => Kind::Set(*local_index, false),
        Operator::LocalTee { local_index } => Kind::Set(*local_index, true),
        Operator::I32Const { value } => Kind::Const(*value as u32 as u64),
        Operator::I64Const { value } => Kind::Const(*value as u64),
        Operator::F32Const { value } => Kind::Const(value.bits() as u64),
        Operator::F64Const { value } => Kind::Const(value.bits()),
        Operator::Drop => Kind::Drop,
        Operator::BrIf { relative_depth } => Kind::BrIf(*relative_depth),
        Operator::If { .. } => Kind::If,
        Operator::GlobalGet { global_index } => Kind::GlobalGet(*global_index),
        Operator::GlobalSet { global_index } => Kind::GlobalSet(*global_index),
        Operator::Select | Operator::TypedSelect { .. } => Kind::Select,
        Operator::I32Add => Kind::Bin { rr: |d, a, b| Op::I32AddRR { d, a, b }, ri: |d, a, imm| Op::I32AddRI { d, a, imm }, comm: true, wide: false },
        Operator::I32Sub => Kind::Bin { rr: |d, a, b| Op::I32SubRR { d, a, b }, ri: |d, a, imm| Op::I32SubRI { d, a, imm }, comm: false, wide: false },
        Operator::I32Mul => Kind::Bin { rr: |d, a, b| Op::I32MulRR { d, a, b }, ri: |d, a, imm| Op::I32MulRI { d, a, imm }, comm: true, wide: false },
        Operator::I32And => Kind::Bin { rr: |d, a, b| Op::I32AndRR { d, a, b }, ri: |d, a, imm| Op::I32AndRI { d, a, imm }, comm: true, wide: false },
        Operator::I32Or => Kind::Bin { rr: |d, a, b| Op::I32OrRR { d, a, b }, ri: |d, a, imm| Op::I32OrRI { d, a, imm }, comm: true, wide: false },
        Operator::I32Xor => Kind::Bin { rr: |d, a, b| Op::I32XorRR { d, a, b }, ri: |d, a, imm| Op::I32XorRI { d, a, imm }, comm: true, wide: false },
        Operator::I32Shl => Kind::Bin { rr: |d, a, b| Op::I32ShlRR { d, a, b }, ri: |d, a, imm| Op::I32ShlRI { d, a, imm }, comm: false, wide: false },
        Operator::I32ShrS => Kind::Bin { rr: |d, a, b| Op::I32ShrSRR { d, a, b }, ri: |d, a, imm| Op::I32ShrSRI { d, a, imm }, comm: false, wide: false },
        Operator::I32ShrU => Kind::Bin { rr: |d, a, b| Op::I32ShrURR { d, a, b }, ri: |d, a, imm| Op::I32ShrURI { d, a, imm }, comm: false, wide: false },
        Operator::I32Eq => Kind::Bin { rr: |d, a, b| Op::I32EqRR { d, a, b }, ri: |d, a, imm| Op::I32EqRI { d, a, imm }, comm: true, wide: false },
        Operator::I32Ne => Kind::Bin { rr: |d, a, b| Op::I32NeRR { d, a, b }, ri: |d, a, imm| Op::I32NeRI { d, a, imm }, comm: true, wide: false },
        Operator::I32LtS => Kind::Bin { rr: |d, a, b| Op::I32LtSRR { d, a, b }, ri: |d, a, imm| Op::I32LtSRI { d, a, imm }, comm: false, wide: false },
        Operator::I32LtU => Kind::Bin { rr: |d, a, b| Op::I32LtURR { d, a, b }, ri: |d, a, imm| Op::I32LtURI { d, a, imm }, comm: false, wide: false },
        Operator::I32GtS => Kind::Bin { rr: |d, a, b| Op::I32GtSRR { d, a, b }, ri: |d, a, imm| Op::I32GtSRI { d, a, imm }, comm: false, wide: false },
        Operator::I32GtU => Kind::Bin { rr: |d, a, b| Op::I32GtURR { d, a, b }, ri: |d, a, imm| Op::I32GtURI { d, a, imm }, comm: false, wide: false },
        Operator::I32LeS => Kind::Bin { rr: |d, a, b| Op::I32LeSRR { d, a, b }, ri: |d, a, imm| Op::I32LeSRI { d, a, imm }, comm: false, wide: false },
        Operator::I32LeU => Kind::Bin { rr: |d, a, b| Op::I32LeURR { d, a, b }, ri: |d, a, imm| Op::I32LeURI { d, a, imm }, comm: false, wide: false },
        Operator::I32GeS => Kind::Bin { rr: |d, a, b| Op::I32GeSRR { d, a, b }, ri: |d, a, imm| Op::I32GeSRI { d, a, imm }, comm: false, wide: false },
        Operator::I32GeU => Kind::Bin { rr: |d, a, b| Op::I32GeURR { d, a, b }, ri: |d, a, imm| Op::I32GeURI { d, a, imm }, comm: false, wide: false },
        Operator::I64Add => Kind::Bin { rr: |d, a, b| Op::I64AddRR { d, a, b }, ri: |d, a, imm| Op::I64AddRI { d, a, imm }, comm: true, wide: true },
        Operator::I64Sub => Kind::Bin { rr: |d, a, b| Op::I64SubRR { d, a, b }, ri: |d, a, imm| Op::I64SubRI { d, a, imm }, comm: false, wide: true },
        Operator::I64Mul => Kind::Bin { rr: |d, a, b| Op::I64MulRR { d, a, b }, ri: |d, a, imm| Op::I64MulRI { d, a, imm }, comm: true, wide: true },
        Operator::I64And => Kind::Bin { rr: |d, a, b| Op::I64AndRR { d, a, b }, ri: |d, a, imm| Op::I64AndRI { d, a, imm }, comm: true, wide: true },
        Operator::I64Or => Kind::Bin { rr: |d, a, b| Op::I64OrRR { d, a, b }, ri: |d, a, imm| Op::I64OrRI { d, a, imm }, comm: true, wide: true },
        Operator::I64Xor => Kind::Bin { rr: |d, a, b| Op::I64XorRR { d, a, b }, ri: |d, a, imm| Op::I64XorRI { d, a, imm }, comm: true, wide: true },
        Operator::I64Shl => Kind::Bin { rr: |d, a, b| Op::I64ShlRR { d, a, b }, ri: |d, a, imm| Op::I64ShlRI { d, a, imm }, comm: false, wide: true },
        Operator::I64ShrS => Kind::Bin { rr: |d, a, b| Op::I64ShrSRR { d, a, b }, ri: |d, a, imm| Op::I64ShrSRI { d, a, imm }, comm: false, wide: true },
        Operator::I64ShrU => Kind::Bin { rr: |d, a, b| Op::I64ShrURR { d, a, b }, ri: |d, a, imm| Op::I64ShrURI { d, a, imm }, comm: false, wide: true },
        Operator::I64Eq => Kind::Bin { rr: |d, a, b| Op::I64EqRR { d, a, b }, ri: |d, a, imm| Op::I64EqRI { d, a, imm }, comm: true, wide: true },
        Operator::I64Ne => Kind::Bin { rr: |d, a, b| Op::I64NeRR { d, a, b }, ri: |d, a, imm| Op::I64NeRI { d, a, imm }, comm: true, wide: true },
        Operator::I64LtS => Kind::Bin { rr: |d, a, b| Op::I64LtSRR { d, a, b }, ri: |d, a, imm| Op::I64LtSRI { d, a, imm }, comm: false, wide: true },
        Operator::I64LtU => Kind::Bin { rr: |d, a, b| Op::I64LtURR { d, a, b }, ri: |d, a, imm| Op::I64LtURI { d, a, imm }, comm: false, wide: true },
        Operator::I64GtS => Kind::Bin { rr: |d, a, b| Op::I64GtSRR { d, a, b }, ri: |d, a, imm| Op::I64GtSRI { d, a, imm }, comm: false, wide: true },
        Operator::I64GtU => Kind::Bin { rr: |d, a, b| Op::I64GtURR { d, a, b }, ri: |d, a, imm| Op::I64GtURI { d, a, imm }, comm: false, wide: true },
        Operator::I64LeS => Kind::Bin { rr: |d, a, b| Op::I64LeSRR { d, a, b }, ri: |d, a, imm| Op::I64LeSRI { d, a, imm }, comm: false, wide: true },
        Operator::I64LeU => Kind::Bin { rr: |d, a, b| Op::I64LeURR { d, a, b }, ri: |d, a, imm| Op::I64LeURI { d, a, imm }, comm: false, wide: true },
        Operator::I64GeS => Kind::Bin { rr: |d, a, b| Op::I64GeSRR { d, a, b }, ri: |d, a, imm| Op::I64GeSRI { d, a, imm }, comm: false, wide: true },
        Operator::I64GeU => Kind::Bin { rr: |d, a, b| Op::I64GeURR { d, a, b }, ri: |d, a, imm| Op::I64GeURI { d, a, imm }, comm: false, wide: true },
        Operator::I32Eqz => Kind::Un(|d, a| Op::I32EqzR { d, a }),
        Operator::I64Eqz => Kind::Un(|d, a| Op::I64EqzR { d, a }),
        Operator::I32WrapI64 => Kind::Un(|d, a| Op::I32WrapI64R { d, a }),
        Operator::I64ExtendI32U => Kind::Un(|d, a| Op::I64ExtendI32UR { d, a }),
        Operator::I64ExtendI32S => Kind::Un(|d, a| Op::I64ExtendI32SR { d, a }),
        Operator::I32Load { memarg } | Operator::F32Load { memarg } | Operator::I64Load32U { memarg } if memarg.memory == 0 => Kind::Load(|d, a, off| Op::I32LoadR { d, a, off }, memarg.offset as u32),
        Operator::I64Load { memarg } | Operator::F64Load { memarg } if memarg.memory == 0 => Kind::Load(|d, a, off| Op::I64LoadR { d, a, off }, memarg.offset as u32),
        Operator::I32Load8U { memarg } | Operator::I64Load8U { memarg } if memarg.memory == 0 => Kind::Load(|d, a, off| Op::Load8UR { d, a, off }, memarg.offset as u32),
        Operator::I32Load16U { memarg } | Operator::I64Load16U { memarg } if memarg.memory == 0 => Kind::Load(|d, a, off| Op::Load16UR { d, a, off }, memarg.offset as u32),
        Operator::I32Load8S { memarg } if memarg.memory == 0 => Kind::Load(|d, a, off| Op::I32Load8SR { d, a, off }, memarg.offset as u32),
        Operator::I32Load16S { memarg } if memarg.memory == 0 => Kind::Load(|d, a, off| Op::I32Load16SR { d, a, off }, memarg.offset as u32),
        Operator::I32Store { memarg } | Operator::F32Store { memarg } | Operator::I64Store32 { memarg } if memarg.memory == 0 => Kind::Store(|a, v, off| Op::Store32R { a, v, off }, |a, imm, off| Op::Store32RI { a, imm, off }, memarg.offset as u32, false),
        Operator::I64Store { memarg } | Operator::F64Store { memarg } if memarg.memory == 0 => Kind::Store(|a, v, off| Op::Store64R { a, v, off }, |a, imm, off| Op::Store64RI { a, imm, off }, memarg.offset as u32, true),
        Operator::I32Store8 { memarg } | Operator::I64Store8 { memarg } if memarg.memory == 0 => Kind::Store(|a, v, off| Op::Store8R { a, v, off }, |a, imm, off| Op::Store8RI { a, imm, off }, memarg.offset as u32, false),
        Operator::I32Store16 { memarg } | Operator::I64Store16 { memarg } if memarg.memory == 0 => Kind::Store(|a, v, off| Op::Store16R { a, v, off }, |a, imm, off| Op::Store16RI { a, imm, off }, memarg.offset as u32, false),
        _ => Kind::Stack,
    }
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
    let mut rg = Regs { nl: f.nlocals, vs: vec![], sp_ok: true, last_def: None };
    let mut r = body.get_operators_reader().map_err(e)?;
    while !r.eof() {
        let offset = r.original_position() as u64;
        let op = r.read().map_err(e)?;
        let dead = fv.get_control_frame(0).map_or(true, |fr| fr.unreachable);
        let kind = if dead { Kind::Stack } else { classify(&op) };
        let stack = matches!(kind, Kind::Stack);
        let resync = matches!(kind, Kind::Stack | Kind::If);
        // Structural ops, calls, and returns don't read `sp` (calls and returns
        // carry their height). Everything else on the stack path does.
        let needs_sp = !matches!(
            op,
            Operator::Block { .. }
                | Operator::Loop { .. }
                | Operator::Else
                | Operator::End
                | Operator::Call { .. }
                | Operator::CallIndirect { .. }
                | Operator::Return
        );
        if stack && !dead {
            rg.materialize(&mut f);
            if needs_sp {
                rg.ensure_sp(&mut f);
            }
        }
        let here = f.ops.len();
        match kind {
            Kind::Stack => {}
            Kind::Get(x) => {
                rg.vs.push(V::Slot(x));
                rg.sp_ok = false;
            }
            Kind::Const(c) => {
                rg.vs.push(V::Const(c));
                rg.sp_ok = false;
            }
            Kind::Drop => {
                rg.vs.pop();
                rg.sp_ok = false;
            }
            Kind::Set(x, tee) => {
                let h = rg.vs.len() - 1;
                let v = rg.vs.pop().ok_or("stack underflow")?;
                // Lazy reads of `x` still on the stack must see the old value.
                let mut copied = false;
                for k in 0..rg.vs.len() {
                    if rg.vs[k] == V::Slot(x) {
                        let own = rg.own(k);
                        f.ops.push(Op::Copy { d: own, src: x });
                        rg.vs[k] = V::Slot(own);
                        copied = true;
                    }
                }
                let n = f.ops.len();
                match v {
                    V::Slot(r) if r == x => {}
                    V::Slot(r) if !copied && r == rg.own(h) && n > barrier && rg.last_def == Some((n - 1, r)) => {
                        retarget(&mut f.ops[n - 1], x)
                    }
                    V::Slot(r) => f.ops.push(Op::Copy { d: x, src: r }),
                    V::Const(c) => f.ops.push(Op::ConstTo { d: x, v: c }),
                }
                if tee {
                    rg.vs.push(V::Slot(x));
                }
                rg.sp_ok = false;
                rg.last_def = None;
            }
            Kind::Bin { rr, ri, comm, wide } => {
                let mut b = rg.vs.pop().ok_or("stack underflow")?;
                let mut a = rg.vs.pop().ok_or("stack underflow")?;
                let ha = rg.vs.len();
                if comm && matches!(a, V::Const(_)) && matches!(b, V::Slot(_)) {
                    std::mem::swap(&mut a, &mut b);
                }
                let ra = rg.slot(&mut f, a, ha);
                let d = rg.own(ha);
                let imm = match b {
                    V::Const(c) if !wide => Some(c as u32 as i32),
                    V::Const(c) if c as i64 == c as i64 as i32 as i64 => Some(c as i64 as i32),
                    _ => None,
                };
                let op = match imm {
                    Some(imm) => ri(d, ra, imm),
                    None => {
                        let rb = rg.slot(&mut f, b, ha + 1);
                        rr(d, ra, rb)
                    }
                };
                f.ops.push(op);
                rg.last_def = Some((f.ops.len() - 1, d));
                rg.vs.push(V::Slot(d));
                rg.sp_ok = false;
            }
            Kind::Un(make) => {
                let a = rg.vs.pop().ok_or("stack underflow")?;
                let h = rg.vs.len();
                let ra = rg.slot(&mut f, a, h);
                let d = rg.own(h);
                f.ops.push(make(d, ra));
                rg.last_def = Some((f.ops.len() - 1, d));
                rg.vs.push(V::Slot(d));
                rg.sp_ok = false;
            }
            Kind::Load(make, off) => {
                let a = rg.vs.pop().ok_or("stack underflow")?;
                let h = rg.vs.len();
                let ra = rg.slot(&mut f, a, h);
                let d = rg.own(h);
                f.ops.push(make(d, ra, off));
                rg.last_def = Some((f.ops.len() - 1, d));
                rg.vs.push(V::Slot(d));
                rg.sp_ok = false;
            }
            Kind::Store(make, make_imm, off, wide) => {
                let v = rg.vs.pop().ok_or("stack underflow")?;
                let a = rg.vs.pop().ok_or("stack underflow")?;
                let h = rg.vs.len();
                let ra = rg.slot(&mut f, a, h);
                let imm = match v {
                    V::Const(c) if !wide => Some(c as u32 as i32),
                    V::Const(c) if c as i64 == c as i64 as i32 as i64 => Some(c as i64 as i32),
                    _ => None,
                };
                match imm {
                    Some(imm) => f.ops.push(make_imm(ra, imm, off)),
                    None => {
                        let rv = rg.slot(&mut f, v, h + 1);
                        f.ops.push(make(ra, rv, off));
                    }
                }
                rg.last_def = None;
                rg.sp_ok = false;
            }
            Kind::GlobalGet(g) => {
                let d = rg.own(rg.vs.len());
                f.ops.push(Op::GlobalGetR { d, g });
                rg.last_def = Some((f.ops.len() - 1, d));
                rg.vs.push(V::Slot(d));
                rg.sp_ok = false;
            }
            Kind::GlobalSet(g) => {
                let v = rg.vs.pop().ok_or("stack underflow")?;
                let h = rg.vs.len();
                let a = rg.slot(&mut f, v, h);
                f.ops.push(Op::GlobalSetR { g, a });
                rg.last_def = None;
                rg.sp_ok = false;
            }
            Kind::Select => {
                let c = rg.vs.pop().ok_or("stack underflow")?;
                let b = rg.vs.pop().ok_or("stack underflow")?;
                let a = rg.vs.pop().ok_or("stack underflow")?;
                let h = rg.vs.len();
                let (ra, rb, rc) = (rg.slot(&mut f, a, h), rg.slot(&mut f, b, h + 1), rg.slot(&mut f, c, h + 2));
                let d = rg.own(h);
                // The condition slot is packed in 16 bits to keep ops at 16 bytes.
                let rc = u16::try_from(rc).map_err(|_| "function frame too large")?;
                f.ops.push(Op::SelectR { d, a: ra, b: rb, c: rc });
                rg.last_def = Some((f.ops.len() - 1, d));
                rg.vs.push(V::Slot(d));
                rg.sp_ok = false;
            }
            Kind::BrIf(depth) => {
                let c = rg.vs.pop().ok_or("stack underflow")?;
                rg.sp_ok = false; // the condition left the stack, so `sp` must move too
                let h = rg.vs.len();
                let rc = rg.slot(&mut f, c, h);
                rg.materialize(&mut f);
                let t = label(&fv, m, &mut ctl, depth, Patch::Table(f.table.len()))?;
                if t.keep > 0 {
                    rg.ensure_sp(&mut f); // carried values are found from `sp`
                }
                f.table.push(t);
                let ti = (f.table.len() - 1) as u32;
                let n = f.ops.len();
                let fused = (n > barrier && rc == rg.own(h) && rg.last_def == Some((n.wrapping_sub(1), rc)))
                    .then(|| fuse_cmp_br(f.ops[n - 1], ti))
                    .flatten();
                match fused {
                    Some(op) => f.ops[n - 1] = op,
                    None => f.ops.push(Op::BrIfR { c: rc, t: ti }),
                }
                rg.last_def = None;
            }
            Kind::If => {
                let c = rg.vs.pop().ok_or("stack underflow")?;
                rg.sp_ok = false; // the condition left the stack, so `sp` must move too
                let h = rg.vs.len();
                let rc = rg.slot(&mut f, c, h);
                rg.materialize(&mut f);
                let n = f.ops.len();
                let fused = (n > barrier && rc == rg.own(h) && rg.last_def == Some((n.wrapping_sub(1), rc)))
                    .then(|| fuse_cmp_if(f.ops[n - 1], 0))
                    .flatten();
                let at = match fused {
                    Some(op) => {
                        f.ops[n - 1] = op;
                        n - 1
                    }
                    None => {
                        f.ops.push(Op::IfR { c: rc, to: 0 });
                        n
                    }
                };
                ctl.push(Ctl { loop_start: None, patches: vec![], else_fix: Some(at) });
            }
        }
        if stack {
        match &op {
            Operator::Block { .. } => ctl.push(Ctl { loop_start: None, patches: vec![], else_fix: None }),
            Operator::Loop { .. } => {
                ctl.push(Ctl { loop_start: Some(here as u32), patches: vec![], else_fix: None });
                f.ops.push(Op::Tick);
                barrier = here;
            }
            // Only reached in dead code: reachable `if` takes the register path.
            Operator::If { .. } => {
                ctl.push(Ctl { loop_start: None, patches: vec![], else_fix: Some(here) });
                f.ops.push(Op::IfR { c: 0, to: 0 });
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
                    f.ops.push(Op::ReturnAt(f.nresults));
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
            Operator::BrTable { targets } => {
                let start = f.table.len() as u32;
                let depths: Vec<u32> = targets.targets().chain([Ok(targets.default())]).collect::<Result<_, _>>().map_err(e)?;
                for d in &depths {
                    let t = label(&fv, m, &mut ctl, *d, Patch::Table(f.table.len()))?;
                    f.table.push(t);
                }
                f.ops.push(Op::BrTable { start, len: depths.len() as u32 });
            }
            Operator::Return => f.ops.push(Op::ReturnAt(rg.vs.len() as u32)),
            Operator::Unreachable => f.ops.push(Op::Unreachable),
            Operator::Nop => {}
            Operator::Call { function_index } => f.ops.push(Op::CallAt(*function_index, rg.vs.len() as u32)),
            Operator::CallIndirect { type_index, table_index } => {
                f.ops.push(Op::CallIndirectAt { ty: *type_index, table: *table_index, h: rg.vs.len() as u32 })
            }
            Operator::MemorySize { mem: 0 } => f.ops.push(Op::MemorySize),
            Operator::MemoryGrow { mem: 0 } => f.ops.push(Op::MemoryGrow),
            Operator::MemoryCopy { dst_mem: 0, src_mem: 0 } => f.ops.push(Op::MemoryCopy),
            Operator::MemoryFill { mem: 0 } => f.ops.push(Op::MemoryFill),
            Operator::MemoryInit { data_index, mem: 0 } => f.ops.push(Op::MemoryInit(*data_index)),
            Operator::DataDrop { data_index } => f.ops.push(Op::DataDrop(*data_index)),
            other => f.ops.push(simple(other).unwrap_or(Op::Unsupported)),
        }
        }
        fv.op(offset, &op).map_err(e)?;
        let height = fv.operand_stack_height();
        if resync {
            rg.sync(height as usize);
            // A label can be reached by an `if` jump that left `sp` stale.
            if matches!(op, Operator::Block { .. } | Operator::Loop { .. } | Operator::Else | Operator::End | Operator::If { .. }) {
                rg.sp_ok = false;
            }
        } else if rg.vs.len() != height as usize {
            return Err("translator stack out of step with the validator".into());
        }
        f.max_height = f.max_height.max(height);
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
        macro_rules! reg {
            ($d:expr, $from:ident, $to:ident, $a:expr, $b:expr, |$x:ident, $y:ident| $e:expr) => {{
                let $x = $from($a);
                let $y = $from($b);
                s[fp + $d as usize] = $to($e);
            }};
        }
        macro_rules! take {
            ($t:expr) => {{
                let t = f.table[$t as usize];
                sp = branch(s, base, sp, t);
                pc = t.pc as usize;
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
                Op::Jump(to) => pc = to as usize,
                Op::BrTable { start, len } => {
                    let i = (pop!() as u32).min(len - 1);
                    let t = f.table[(start + i) as usize];
                    sp = branch(s, base, sp, t);
                    pc = t.pc as usize;
                }
                Op::ReturnAt(h) => {
                    sp = base + h as usize;
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
                Op::CallAt(idx, h) => {
                    sp = base + h as usize;
                    call!(idx, ())
                }
                Op::CallIndirectAt { ty, table, h } => {
                    sp = base + h as usize;
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

                Op::Copy { d, src } => s[fp + d as usize] = s[fp + src as usize],
                Op::ConstTo { d, v } => s[fp + d as usize] = v,
                Op::SetSp(h) => sp = base + h as usize,
                Op::BrIfR { c, t } => {
                    if s[fp + c as usize] as u32 != 0 {
                        let t = f.table[t as usize];
                        sp = branch(s, base, sp, t);
                        pc = t.pc as usize;
                    }
                }
                Op::BrIfI32EqRR { a, b, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if x == y { take!(t) } }
                Op::BrIfI32EqRI { a, imm, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if x == y { take!(t) } }
                Op::IfI32EqRR { a, b, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if !(x == y) { pc = to as usize; } }
                Op::IfI32EqRI { a, imm, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if !(x == y) { pc = to as usize; } }
                Op::BrIfI32NeRR { a, b, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if x != y { take!(t) } }
                Op::BrIfI32NeRI { a, imm, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if x != y { take!(t) } }
                Op::IfI32NeRR { a, b, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if !(x != y) { pc = to as usize; } }
                Op::IfI32NeRI { a, imm, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if !(x != y) { pc = to as usize; } }
                Op::BrIfI32LtSRR { a, b, t } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(s[fp + b as usize])); if x < y { take!(t) } }
                Op::BrIfI32LtSRI { a, imm, t } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(imm as u32 as u64)); if x < y { take!(t) } }
                Op::IfI32LtSRR { a, b, to } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(s[fp + b as usize])); if !(x < y) { pc = to as usize; } }
                Op::IfI32LtSRI { a, imm, to } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(imm as u32 as u64)); if !(x < y) { pc = to as usize; } }
                Op::BrIfI32LtURR { a, b, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if x < y { take!(t) } }
                Op::BrIfI32LtURI { a, imm, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if x < y { take!(t) } }
                Op::IfI32LtURR { a, b, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if !(x < y) { pc = to as usize; } }
                Op::IfI32LtURI { a, imm, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if !(x < y) { pc = to as usize; } }
                Op::BrIfI32GtSRR { a, b, t } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(s[fp + b as usize])); if x > y { take!(t) } }
                Op::BrIfI32GtSRI { a, imm, t } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(imm as u32 as u64)); if x > y { take!(t) } }
                Op::IfI32GtSRR { a, b, to } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(s[fp + b as usize])); if !(x > y) { pc = to as usize; } }
                Op::IfI32GtSRI { a, imm, to } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(imm as u32 as u64)); if !(x > y) { pc = to as usize; } }
                Op::BrIfI32GtURR { a, b, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if x > y { take!(t) } }
                Op::BrIfI32GtURI { a, imm, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if x > y { take!(t) } }
                Op::IfI32GtURR { a, b, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if !(x > y) { pc = to as usize; } }
                Op::IfI32GtURI { a, imm, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if !(x > y) { pc = to as usize; } }
                Op::BrIfI32LeSRR { a, b, t } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(s[fp + b as usize])); if x <= y { take!(t) } }
                Op::BrIfI32LeSRI { a, imm, t } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(imm as u32 as u64)); if x <= y { take!(t) } }
                Op::IfI32LeSRR { a, b, to } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(s[fp + b as usize])); if !(x <= y) { pc = to as usize; } }
                Op::IfI32LeSRI { a, imm, to } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(imm as u32 as u64)); if !(x <= y) { pc = to as usize; } }
                Op::BrIfI32LeURR { a, b, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if x <= y { take!(t) } }
                Op::BrIfI32LeURI { a, imm, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if x <= y { take!(t) } }
                Op::IfI32LeURR { a, b, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if !(x <= y) { pc = to as usize; } }
                Op::IfI32LeURI { a, imm, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if !(x <= y) { pc = to as usize; } }
                Op::BrIfI32GeSRR { a, b, t } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(s[fp + b as usize])); if x >= y { take!(t) } }
                Op::BrIfI32GeSRI { a, imm, t } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(imm as u32 as u64)); if x >= y { take!(t) } }
                Op::IfI32GeSRR { a, b, to } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(s[fp + b as usize])); if !(x >= y) { pc = to as usize; } }
                Op::IfI32GeSRI { a, imm, to } => { let (x, y) = (i32_(s[fp + a as usize]), i32_(imm as u32 as u64)); if !(x >= y) { pc = to as usize; } }
                Op::BrIfI32GeURR { a, b, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if x >= y { take!(t) } }
                Op::BrIfI32GeURI { a, imm, t } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if x >= y { take!(t) } }
                Op::IfI32GeURR { a, b, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(s[fp + b as usize])); if !(x >= y) { pc = to as usize; } }
                Op::IfI32GeURI { a, imm, to } => { let (x, y) = (u32_(s[fp + a as usize]), u32_(imm as u32 as u64)); if !(x >= y) { pc = to as usize; } }
                Op::BrIfI64EqRR { a, b, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if x == y { take!(t) } }
                Op::BrIfI64EqRI { a, imm, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if x == y { take!(t) } }
                Op::IfI64EqRR { a, b, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if !(x == y) { pc = to as usize; } }
                Op::IfI64EqRI { a, imm, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if !(x == y) { pc = to as usize; } }
                Op::BrIfI64NeRR { a, b, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if x != y { take!(t) } }
                Op::BrIfI64NeRI { a, imm, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if x != y { take!(t) } }
                Op::IfI64NeRR { a, b, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if !(x != y) { pc = to as usize; } }
                Op::IfI64NeRI { a, imm, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if !(x != y) { pc = to as usize; } }
                Op::BrIfI64LtSRR { a, b, t } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(s[fp + b as usize])); if x < y { take!(t) } }
                Op::BrIfI64LtSRI { a, imm, t } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(imm as i64 as u64)); if x < y { take!(t) } }
                Op::IfI64LtSRR { a, b, to } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(s[fp + b as usize])); if !(x < y) { pc = to as usize; } }
                Op::IfI64LtSRI { a, imm, to } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(imm as i64 as u64)); if !(x < y) { pc = to as usize; } }
                Op::BrIfI64LtURR { a, b, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if x < y { take!(t) } }
                Op::BrIfI64LtURI { a, imm, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if x < y { take!(t) } }
                Op::IfI64LtURR { a, b, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if !(x < y) { pc = to as usize; } }
                Op::IfI64LtURI { a, imm, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if !(x < y) { pc = to as usize; } }
                Op::BrIfI64GtSRR { a, b, t } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(s[fp + b as usize])); if x > y { take!(t) } }
                Op::BrIfI64GtSRI { a, imm, t } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(imm as i64 as u64)); if x > y { take!(t) } }
                Op::IfI64GtSRR { a, b, to } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(s[fp + b as usize])); if !(x > y) { pc = to as usize; } }
                Op::IfI64GtSRI { a, imm, to } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(imm as i64 as u64)); if !(x > y) { pc = to as usize; } }
                Op::BrIfI64GtURR { a, b, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if x > y { take!(t) } }
                Op::BrIfI64GtURI { a, imm, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if x > y { take!(t) } }
                Op::IfI64GtURR { a, b, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if !(x > y) { pc = to as usize; } }
                Op::IfI64GtURI { a, imm, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if !(x > y) { pc = to as usize; } }
                Op::BrIfI64LeSRR { a, b, t } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(s[fp + b as usize])); if x <= y { take!(t) } }
                Op::BrIfI64LeSRI { a, imm, t } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(imm as i64 as u64)); if x <= y { take!(t) } }
                Op::IfI64LeSRR { a, b, to } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(s[fp + b as usize])); if !(x <= y) { pc = to as usize; } }
                Op::IfI64LeSRI { a, imm, to } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(imm as i64 as u64)); if !(x <= y) { pc = to as usize; } }
                Op::BrIfI64LeURR { a, b, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if x <= y { take!(t) } }
                Op::BrIfI64LeURI { a, imm, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if x <= y { take!(t) } }
                Op::IfI64LeURR { a, b, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if !(x <= y) { pc = to as usize; } }
                Op::IfI64LeURI { a, imm, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if !(x <= y) { pc = to as usize; } }
                Op::BrIfI64GeSRR { a, b, t } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(s[fp + b as usize])); if x >= y { take!(t) } }
                Op::BrIfI64GeSRI { a, imm, t } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(imm as i64 as u64)); if x >= y { take!(t) } }
                Op::IfI64GeSRR { a, b, to } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(s[fp + b as usize])); if !(x >= y) { pc = to as usize; } }
                Op::IfI64GeSRI { a, imm, to } => { let (x, y) = (i64_(s[fp + a as usize]), i64_(imm as i64 as u64)); if !(x >= y) { pc = to as usize; } }
                Op::BrIfI64GeURR { a, b, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if x >= y { take!(t) } }
                Op::BrIfI64GeURI { a, imm, t } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if x >= y { take!(t) } }
                Op::IfI64GeURR { a, b, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(s[fp + b as usize])); if !(x >= y) { pc = to as usize; } }
                Op::IfI64GeURI { a, imm, to } => { let (x, y) = (u64_(s[fp + a as usize]), u64_(imm as i64 as u64)); if !(x >= y) { pc = to as usize; } }
                Op::BrIfI32EqzR { a, t } => { if u32_(s[fp + a as usize]) == 0 { take!(t) } }
                Op::IfI32EqzR { a, to } => { if u32_(s[fp + a as usize]) != 0 { pc = to as usize; } }
                Op::BrIfI64EqzR { a, t } => { if u64_(s[fp + a as usize]) == 0 { take!(t) } }
                Op::IfI64EqzR { a, to } => { if u64_(s[fp + a as usize]) != 0 { pc = to as usize; } }
                Op::GlobalGetR { d, g } => s[fp + d as usize] = globals[g as usize],
                Op::GlobalSetR { g, a } => globals[g as usize] = s[fp + a as usize],
                Op::SelectR { d, a, b, c } => {
                    let v = if s[fp + c as usize] as u32 != 0 { s[fp + a as usize] } else { s[fp + b as usize] };
                    s[fp + d as usize] = v;
                }
                Op::Store32RI { a, imm, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let v = (imm as i64 as u64).to_le_bytes(); mem.get_mut(ea..ea + 4).ok_or("out of bounds memory access")?.copy_from_slice(&v[..4]); }
                Op::Store64RI { a, imm, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let v = (imm as i64 as u64).to_le_bytes(); mem.get_mut(ea..ea + 8).ok_or("out of bounds memory access")?.copy_from_slice(&v[..8]); }
                Op::Store8RI { a, imm, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let v = (imm as i64 as u64).to_le_bytes(); mem.get_mut(ea..ea + 1).ok_or("out of bounds memory access")?.copy_from_slice(&v[..1]); }
                Op::Store16RI { a, imm, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let v = (imm as i64 as u64).to_le_bytes(); mem.get_mut(ea..ea + 2).ok_or("out of bounds memory access")?.copy_from_slice(&v[..2]); }
                Op::IfR { c, to } => {
                    if s[fp + c as usize] as u32 == 0 {
                        pc = to as usize;
                    }
                }
                Op::I32AddRR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_add(y)),
                Op::I32AddRI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| x.wrapping_add(y)),
                Op::I32SubRR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_sub(y)),
                Op::I32SubRI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| x.wrapping_sub(y)),
                Op::I32MulRR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_mul(y)),
                Op::I32MulRI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| x.wrapping_mul(y)),
                Op::I32AndRR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| x & y),
                Op::I32AndRI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| x & y),
                Op::I32OrRR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| x | y),
                Op::I32OrRI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| x | y),
                Op::I32XorRR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| x ^ y),
                Op::I32XorRI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| x ^ y),
                Op::I32ShlRR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_shl(y)),
                Op::I32ShlRI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| x.wrapping_shl(y)),
                Op::I32ShrSRR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| (x as i32).wrapping_shr(y) as u32),
                Op::I32ShrSRI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| (x as i32).wrapping_shr(y) as u32),
                Op::I32ShrURR { d, a, b } => reg!(d, u32_, ru32, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_shr(y)),
                Op::I32ShrURI { d, a, imm } => reg!(d, u32_, ru32, s[fp + a as usize], imm as u32 as u64, |x, y| x.wrapping_shr(y)),
                Op::I32EqRR { d, a, b } => reg!(d, u32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x == y),
                Op::I32EqRI { d, a, imm } => reg!(d, u32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x == y),
                Op::I32NeRR { d, a, b } => reg!(d, u32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x != y),
                Op::I32NeRI { d, a, imm } => reg!(d, u32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x != y),
                Op::I32LtSRR { d, a, b } => reg!(d, i32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x < y),
                Op::I32LtSRI { d, a, imm } => reg!(d, i32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x < y),
                Op::I32LtURR { d, a, b } => reg!(d, u32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x < y),
                Op::I32LtURI { d, a, imm } => reg!(d, u32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x < y),
                Op::I32GtSRR { d, a, b } => reg!(d, i32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x > y),
                Op::I32GtSRI { d, a, imm } => reg!(d, i32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x > y),
                Op::I32GtURR { d, a, b } => reg!(d, u32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x > y),
                Op::I32GtURI { d, a, imm } => reg!(d, u32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x > y),
                Op::I32LeSRR { d, a, b } => reg!(d, i32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x <= y),
                Op::I32LeSRI { d, a, imm } => reg!(d, i32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x <= y),
                Op::I32LeURR { d, a, b } => reg!(d, u32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x <= y),
                Op::I32LeURI { d, a, imm } => reg!(d, u32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x <= y),
                Op::I32GeSRR { d, a, b } => reg!(d, i32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x >= y),
                Op::I32GeSRI { d, a, imm } => reg!(d, i32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x >= y),
                Op::I32GeURR { d, a, b } => reg!(d, u32_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x >= y),
                Op::I32GeURI { d, a, imm } => reg!(d, u32_, rb, s[fp + a as usize], imm as u32 as u64, |x, y| x >= y),
                Op::I64AddRR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_add(y)),
                Op::I64AddRI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| x.wrapping_add(y)),
                Op::I64SubRR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_sub(y)),
                Op::I64SubRI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| x.wrapping_sub(y)),
                Op::I64MulRR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_mul(y)),
                Op::I64MulRI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| x.wrapping_mul(y)),
                Op::I64AndRR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| x & y),
                Op::I64AndRI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| x & y),
                Op::I64OrRR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| x | y),
                Op::I64OrRI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| x | y),
                Op::I64XorRR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| x ^ y),
                Op::I64XorRI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| x ^ y),
                Op::I64ShlRR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_shl(y as u32)),
                Op::I64ShlRI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| x.wrapping_shl(y as u32)),
                Op::I64ShrSRR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| (x as i64).wrapping_shr(y as u32) as u64),
                Op::I64ShrSRI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| (x as i64).wrapping_shr(y as u32) as u64),
                Op::I64ShrURR { d, a, b } => reg!(d, u64_, ru64, s[fp + a as usize], s[fp + b as usize], |x, y| x.wrapping_shr(y as u32)),
                Op::I64ShrURI { d, a, imm } => reg!(d, u64_, ru64, s[fp + a as usize], imm as i64 as u64, |x, y| x.wrapping_shr(y as u32)),
                Op::I64EqRR { d, a, b } => reg!(d, u64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x == y),
                Op::I64EqRI { d, a, imm } => reg!(d, u64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x == y),
                Op::I64NeRR { d, a, b } => reg!(d, u64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x != y),
                Op::I64NeRI { d, a, imm } => reg!(d, u64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x != y),
                Op::I64LtSRR { d, a, b } => reg!(d, i64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x < y),
                Op::I64LtSRI { d, a, imm } => reg!(d, i64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x < y),
                Op::I64LtURR { d, a, b } => reg!(d, u64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x < y),
                Op::I64LtURI { d, a, imm } => reg!(d, u64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x < y),
                Op::I64GtSRR { d, a, b } => reg!(d, i64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x > y),
                Op::I64GtSRI { d, a, imm } => reg!(d, i64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x > y),
                Op::I64GtURR { d, a, b } => reg!(d, u64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x > y),
                Op::I64GtURI { d, a, imm } => reg!(d, u64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x > y),
                Op::I64LeSRR { d, a, b } => reg!(d, i64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x <= y),
                Op::I64LeSRI { d, a, imm } => reg!(d, i64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x <= y),
                Op::I64LeURR { d, a, b } => reg!(d, u64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x <= y),
                Op::I64LeURI { d, a, imm } => reg!(d, u64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x <= y),
                Op::I64GeSRR { d, a, b } => reg!(d, i64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x >= y),
                Op::I64GeSRI { d, a, imm } => reg!(d, i64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x >= y),
                Op::I64GeURR { d, a, b } => reg!(d, u64_, rb, s[fp + a as usize], s[fp + b as usize], |x, y| x >= y),
                Op::I64GeURI { d, a, imm } => reg!(d, u64_, rb, s[fp + a as usize], imm as i64 as u64, |x, y| x >= y),
                Op::I32EqzR { d, a } => { let x = u32_(s[fp + a as usize]); s[fp + d as usize] = rb(x == 0); }
                Op::I64EqzR { d, a } => { let x = u64_(s[fp + a as usize]); s[fp + d as usize] = rb(x == 0); }
                Op::I32WrapI64R { d, a } => { let x = u64_(s[fp + a as usize]); s[fp + d as usize] = ru32(x as u32); }
                Op::I64ExtendI32UR { d, a } => { let x = u32_(s[fp + a as usize]); s[fp + d as usize] = ru64(x as u64); }
                Op::I64ExtendI32SR { d, a } => { let x = i32_(s[fp + a as usize]); s[fp + d as usize] = ri64(x as i64); }
                Op::I32LoadR { d, a, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let b: [u8; 4] = mem.get(ea..ea + 4).ok_or("out of bounds memory access")?.try_into().unwrap(); s[fp + d as usize] = u32::from_le_bytes(b) as u64; }
                Op::I64LoadR { d, a, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let b: [u8; 8] = mem.get(ea..ea + 8).ok_or("out of bounds memory access")?.try_into().unwrap(); s[fp + d as usize] = u64::from_le_bytes(b); }
                Op::Load8UR { d, a, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let b: [u8; 1] = mem.get(ea..ea + 1).ok_or("out of bounds memory access")?.try_into().unwrap(); s[fp + d as usize] = b[0] as u64; }
                Op::Load16UR { d, a, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let b: [u8; 2] = mem.get(ea..ea + 2).ok_or("out of bounds memory access")?.try_into().unwrap(); s[fp + d as usize] = u16::from_le_bytes(b) as u64; }
                Op::I32Load8SR { d, a, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let b: [u8; 1] = mem.get(ea..ea + 1).ok_or("out of bounds memory access")?.try_into().unwrap(); s[fp + d as usize] = ri32(b[0] as i8 as i32); }
                Op::I32Load16SR { d, a, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let b: [u8; 2] = mem.get(ea..ea + 2).ok_or("out of bounds memory access")?.try_into().unwrap(); s[fp + d as usize] = ri32(i16::from_le_bytes(b) as i32); }
                Op::Store32R { a, v, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let v = s[fp + v as usize].to_le_bytes(); mem.get_mut(ea..ea + 4).ok_or("out of bounds memory access")?.copy_from_slice(&v[..4]); }
                Op::Store64R { a, v, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let v = s[fp + v as usize].to_le_bytes(); mem.get_mut(ea..ea + 8).ok_or("out of bounds memory access")?.copy_from_slice(&v[..8]); }
                Op::Store8R { a, v, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let v = s[fp + v as usize].to_le_bytes(); mem.get_mut(ea..ea + 1).ok_or("out of bounds memory access")?.copy_from_slice(&v[..1]); }
                Op::Store16R { a, v, off } => { let ea = s[fp + a as usize] as u32 as usize + off as usize; let v = s[fp + v as usize].to_le_bytes(); mem.get_mut(ea..ea + 2).ok_or("out of bounds memory access")?.copy_from_slice(&v[..2]); }
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
