//! `aot-gen`: compile a Wasm module to plain safe Rust for the `combd-aot`
//! crate. rustc + LLVM then turn it into machine code for any CPU.
//!
//! Every Wasm local and stack slot becomes a `let mut` u64 (LLVM keeps them in
//! registers). Wasm's structured control flow maps onto Rust labeled blocks and
//! loops, so `br` is just `break` or `continue`. Traps are `Err` values passed
//! up with `?`. The output never needs `unsafe`, so rustc, not this generator,
//! guarantees the cell can't touch memory outside its own.
//!
//! A function with 0 or 1 results returns `Result<u64, Trap>` (two
//! registers); one with more returns `Result<[u64; N], Trap>`.
//!
//! ponytail: one memory, one table, imports with at most one result. Error out
//! on anything else so a module either compiles fully or not at all.

use std::collections::BTreeSet;
use std::fmt::Write;
use wasmparser::{
    BlockType, ConstExpr, DataKind, ElementItems, ElementKind, ExternalKind, FrameKind, FuncType, FuncValidator,
    FunctionBody, KnownCustom, Name, Operator, Parser, Payload, TypeRef, ValidPayload, Validator, ValidatorResources,
};

/// EXPERIMENT ONLY: `COMBD_AOT_EXPERIMENT=notick,nostack,unchecked` drops a
/// safety feature from the generated code, to measure what it costs.
fn experiment(flag: &str) -> bool {
    std::env::var("COMBD_AOT_EXPERIMENT").unwrap_or_default().split(',').any(|f| f == flag)
}

fn e(x: impl std::fmt::Display) -> String {
    x.to_string()
}

#[derive(Default)]
struct Info {
    types: Vec<FuncType>,
    func_types: Vec<u32>,
    imports: Vec<(String, String, u32)>,
    globals: Vec<u64>,
    mem: (u32, u32),
    table: Option<u32>,
    elems: Vec<(u32, Vec<u32>)>,
    data: Vec<Vec<u8>>,
    active: Vec<(usize, u32)>,
    exports: Vec<(String, u32)>,
    indirect: BTreeSet<u32>,
    /// Per defined function, for placing fuel ticks: (index, has a loop,
    /// direct callees, size in operators).
    fns: Vec<(u32, bool, Vec<u32>, usize)>,
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
        op => return Err(format!("unsupported constant expression {op:?}")),
    };
    Ok(v)
}

/// Rust for the module, or why it can't be compiled.
/// A module, as Rust. Its bytes are the attacker's: any input that trips a
/// bug here must be a clean error on the builder, not a builder down.
pub fn generate(wasm: &[u8], source: &str) -> Result<String, String> {
    std::panic::catch_unwind(|| generate_in(wasm, source)).unwrap_or_else(|_| Err("the module couldn't be compiled (a bug in combd: please report it)".into()))
}

fn generate_in(wasm: &[u8], source: &str) -> Result<String, String> {
    let mut v = Validator::new();
    let mut m = Info { mem: (0, 0), ..Default::default() };
    let mut funcs = String::new();
    let mut defined = 0u32;
    let mut names = String::new();
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(e)?;
        if let ValidPayload::Func(f, body) = v.payload(&payload).map_err(e)? {
            let idx = m.imports.len() as u32 + defined;
            gen_func(&mut m, idx, f.into_validator(Default::default()), &body, &mut funcs)?;
            defined += 1;
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
                    let TypeRef::Func(t) = i.ty else { return Err("only function imports are supported".into()) };
                    let results = m.types[t as usize].results().len() as u32;
                    if results > 1 {
                        return Err("imports with more than one result are not supported".into());
                    }
                    m.imports.push((i.module.into(), i.name.into(), results));
                    m.func_types.push(t);
                }
            }
            Payload::FunctionSection(r) => {
                for t in r {
                    m.func_types.push(t.map_err(e)?);
                }
            }
            Payload::TableSection(r) => {
                for t in r {
                    if m.table.is_some() {
                        return Err("only one table is supported".into());
                    }
                    // Capped as the interpreter caps it: every cell
                    // allocates its table at start.
                    let initial = t.map_err(e)?.ty.initial;
                    if initial > crate::interp::MAX_TABLE {
                        return Err("table too large".into());
                    }
                    m.table = Some(initial as u32);
                }
            }
            Payload::MemorySection(r) => {
                for mt in r {
                    let mt = mt.map_err(e)?;
                    if mt.memory64 || mt.shared {
                        return Err("only plain 32-bit memory is supported".into());
                    }
                    m.mem = (mt.initial as u32, mt.maximum.map_or(16384, |x| x.min(16384) as u32));
                }
            }
            Payload::GlobalSection(r) => {
                for g in r {
                    let val = const_val(&g.map_err(e)?.init_expr, &m.globals)?;
                    if m.globals.len() == 64 {
                        return Err("more than 64 globals is not supported".into());
                    }
                    m.globals.push(val);
                }
            }
            Payload::ExportSection(r) => {
                for x in r {
                    let x = x.map_err(e)?;
                    if x.kind == ExternalKind::Func {
                        m.exports.push((x.name.into(), x.index));
                    }
                }
            }
            Payload::StartSection { .. } => return Err("start functions are not supported".into()),
            Payload::ElementSection(r) => {
                for el in r {
                    let el = el.map_err(e)?;
                    let ElementKind::Active { offset_expr, .. } = el.kind else { continue };
                    let offset = const_val(&offset_expr, &m.globals)? as u32;
                    let fs: Vec<u32> = match el.items {
                        ElementItems::Functions(fs) => fs.into_iter().collect::<Result<_, _>>().map_err(e)?,
                        ElementItems::Expressions(_, xs) => xs
                            .into_iter()
                            .map(|x| const_val(&x.map_err(e)?, &m.globals).map(|v| v as u32))
                            .collect::<Result<_, _>>()?,
                    };
                    m.elems.push((offset, fs));
                }
            }
            Payload::DataSection(r) => {
                for d in r {
                    let d = d.map_err(e)?;
                    let seg = m.data.len();
                    m.data.push(d.data.to_vec());
                    if let DataKind::Active { memory_index: 0, offset_expr } = d.kind {
                        m.active.push((seg, const_val(&offset_expr, &m.globals)? as u32));
                    }
                }
            }
            // Function names, so a profile of `fN` can be read. Escaped
            // (`{:?}`): the module wrote them, and a line break would end the
            // comment and put the rest in the code the builder compiles.
            Payload::CustomSection(c) => {
                if let KnownCustom::Name(r) = c.as_known() {
                    for n in r.into_iter().flatten() {
                        if let Name::Function(map) = n {
                            for f in map.into_iter().flatten() {
                                let _ = writeln!(names, "// f{} = {:?}", f.index, f.name);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let funcs = place_ticks(&m, &funcs);
    Ok(module_text(&m, &funcs, source, crate_hash(wasm)) + &names)
}

fn crate_hash(wasm: &[u8]) -> u64 {
    wasm.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// What a function with `nr` results returns.
fn ret_ty(nr: usize) -> String {
    if nr <= 1 { "u64".into() } else { format!("[u64; {nr}]") }
}

/// A call `x` (with fuel saved around it) whose `nr` results go to `s{at}..`.
fn call_into(mark: &str, x: &str, nr: usize, at: usize) -> String {
    match nr {
        0 => format!("{mark}c.fuel = fuel; {x}; fuel = c.fuel;"),
        1 => format!("{mark}c.fuel = fuel; s{at} = {x}; fuel = c.fuel;"),
        _ => {
            let unpack: String = (0..nr).map(|i| format!(" s{} = r[{i}];", at + i)).collect();
            format!("{{ {mark}c.fuel = fuel; let r = {x}; fuel = c.fuel;{unpack} }}")
        }
    }
}

fn args(n: usize, prefix: &str) -> String {
    (0..n).map(|i| format!(", {prefix}{i}")).collect()
}

fn module_text(m: &Info, funcs: &str, source: &str, hash: u64) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "//! Generated by `combd-spike aot-gen` from {source:?}. Do not edit.\n");
    o.push_str("#![allow(unused_mut, unused_variables, unused_assignments, unreachable_code, unused_labels, unused_parens, dead_code, clippy::all)]\n\n");
    o.push_str("#![forbid(unsafe_code)]\n");
    o.push_str("use crate::rt::{self, Ctx, Trap};\n\n");
    let _ = writeln!(o, "pub const MODULE: crate::Compiled = crate::Compiled {{ hash: {hash:#x}, imports: IMPORTS, new, export, call }};\n");
    o.push_str("const IMPORTS: &[(&str, &str, u32)] = &[");
    for (md, n, r) in &m.imports {
        let _ = write!(o, "({md:?}, {n:?}, {r}), ");
    }
    o.push_str("];\n\nstatic DATA: &[&[u8]] = &[\n");
    for d in &m.data {
        o.push_str("    b\"");
        for &b in d {
            match b {
                b'"' | b'\\' => {
                    let _ = write!(o, "\\{}", b as char);
                }
                0x20..=0x7e => o.push(b as char),
                _ => {
                    let _ = write!(o, "\\x{b:02x}");
                }
            }
        }
        o.push_str("\",\n");
    }
    o.push_str("];\n\n");
    // The entry points catch a panic (none should be reachable: every
    // fallible op is a `Trap`) as a trap, so it ends the cell, never the
    // node, as the interpreter's `catch_unwind` does. `Unreachable`: a new
    // trap kind would change the pin, and every builder with it.
    o.push_str("fn new(host: Box<dyn rt::Host>) -> Result<(Ctx, rt::Mem), Trap> {\n    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| new_in(host))).unwrap_or(Err(Trap::Unreachable))\n}\n\n");
    o.push_str("fn call(c: &mut Ctx, m: &mut rt::Mem, f: u32, a: &[u64]) -> Result<Vec<u64>, Trap> {\n    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call_in(c, m, f, a))).unwrap_or(Err(Trap::Unreachable))\n}\n\n");
    let _ = writeln!(o, "fn new_in(host: Box<dyn rt::Host>) -> Result<(Ctx, rt::Mem), Trap> {{");
    let _ = writeln!(o, "    let (mut c, mut m) = Ctx::new({}, {}, DATA, host)?;", m.mem.0, m.mem.1);
    for (i, g) in m.globals.iter().enumerate() {
        let _ = writeln!(o, "    c.globals[{i}] = {g:#x};");
    }
    let _ = writeln!(o, "    c.table = vec![None; {}];", m.table.unwrap_or(0));
    for (off, fs) in &m.elems {
        let _ = writeln!(o, "    c.init_elem({off}, &{fs:?})?;");
    }
    for (seg, off) in &m.active {
        let _ = writeln!(o, "    c.init_data(&mut m, {seg}, {off})?;");
    }
    o.push_str("    Ok((c, m))\n}\n\n");
    o.push_str("fn export(name: &str) -> Option<u32> {\n    match name {\n");
    for (n, i) in &m.exports {
        let _ = writeln!(o, "        {n:?} => Some({i}),");
    }
    o.push_str("        _ => None,\n    }\n}\n\n");
    o.push_str("fn call_in(c: &mut Ctx, m: &mut rt::Mem, f: u32, a: &[u64]) -> Result<Vec<u64>, Trap> {\n    c.start();\n    match f {\n");
    for (_, i) in &m.exports {
        let ty = &m.types[m.func_types[*i as usize] as usize];
        let (np, nr) = (ty.params().len(), ty.results().len());
        let argv: String = (0..np).map(|k| format!(", a[{k}]")).collect();
        let callx = if (*i as usize) < m.imports.len() {
            format!("c.import(m, {i}, &[{}])?", (0..np).map(|k| format!("a[{k}]")).collect::<Vec<_>>().join(", "))
        } else {
            format!("f{i}(c, m, rt::STACK_BUDGET as u32{argv})?")
        };
        let results = match nr {
            0 => "vec![]",
            1 => "vec![r]",
            _ => "r.to_vec()",
        };
        let _ = writeln!(o, "        {i} if a.len() == {np} => {{ let r = {callx}; Ok({results}) }}");
    }
    o.push_str("        _ => Err(Trap::Unreachable),\n    }\n}\n\n");
    // One `call_indirect` dispatcher per signature: only matching functions can be reached.
    for &t in &m.indirect {
        let ty = &m.types[t as usize];
        let (np, nr) = (ty.params().len(), ty.results().len());
        let _ = writeln!(o, "fn ci{t}(c: &mut Ctx, m: &mut rt::Mem, sb: u32, i: u64{}) -> Result<{}, Trap> {{", (0..np).map(|k| format!(", a{k}: u64")).collect::<String>(), ret_ty(nr));
        o.push_str("    match c.table_get(i)? {\n");
        for (f, ft) in m.func_types.iter().enumerate() {
            if m.types[*ft as usize] != *ty {
                continue;
            }
            if f < m.imports.len() {
                let _ = writeln!(o, "        {f} => c.import(m, {f}, &[{}]),", (0..np).map(|k| format!("a{k}")).collect::<Vec<_>>().join(", "));
            } else {
                let _ = writeln!(o, "        {f} => f{f}(c, m, sb{}),", args(np, "a"));
            }
        }
        o.push_str("        _ => Err(Trap::IndirectTypeMismatch),\n    }\n}\n\n");
    }
    o.push_str(funcs);
    o
}

#[derive(Clone, Copy, PartialEq)]
enum Lk {
    Root,
    Block,
    Loop,
    If,
}

fn arity(m: &Info, bt: BlockType) -> (usize, usize) {
    match bt {
        BlockType::Empty => (0, 0),
        BlockType::Type(_) => (0, 1),
        BlockType::FuncType(i) => (m.types[i as usize].params().len(), m.types[i as usize].results().len()),
    }
}

fn gen_func(
    m: &mut Info,
    idx: u32,
    mut fv: FuncValidator<ValidatorResources>,
    body: &FunctionBody,
    out: &mut String,
) -> Result<(), String> {
    let ty = m.types[m.func_types[idx as usize] as usize].clone();
    let (np, nr) = (ty.params().len(), ty.results().len());
    let mut nl = np;
    let mut locals = body.get_locals_reader().map_err(e)?;
    for _ in 0..locals.get_count() {
        let offset = locals.original_position() as u64;
        let (n, t) = locals.read().map_err(e)?;
        fv.define_locals(offset, n, t).map_err(e)?;
        nl += n as usize;
        if nl > 50_000 {
            return Err("too many locals".into());
        }
    }
    // Return the top `nr` values.
    let ret = |top: usize| -> String {
        match nr {
            0 => "c.fuel = fuel; return Ok(0);".into(),
            1 => format!("c.fuel = fuel; return Ok(s{});", top - 1),
            _ => format!("c.fuel = fuel; return Ok([{}]);", (top - nr..top).map(|i| format!("s{i}")).collect::<Vec<_>>().join(", ")),
        }
    };
    let mut b = String::new();
    let mut ctl: Vec<(Lk, u32)> = vec![(Lk::Root, 0)];
    let mut next = 0u32;
    let mut max_h = 0usize;
    let mut callees = vec![];
    let mut size = 0usize;
    let mut r = body.get_operators_reader().map_err(e)?;
    while !r.eof() {
        size += 1;
        let offset = r.original_position() as u64;
        let op = r.read().map_err(e)?;
        let h = fv.operand_stack_height() as usize;
        let dead = fv.get_control_frame(0).map_or(true, |f| f.unreachable);
        // Branch to `depth` with the top `keep` values sitting at `top`.
        let jump = |fv: &FuncValidator<ValidatorResources>, depth: u32, top: usize| -> Result<String, String> {
            let fr = fv.get_control_frame(depth as usize).ok_or("bad branch depth")?;
            let (params, results) = arity(m, fr.block_type);
            let keep = if fr.kind == FrameKind::Loop { params } else { results };
            let (kind, id) = ctl[ctl.len() - 1 - depth as usize];
            if kind == Lk::Root {
                return Ok(ret(top));
            }
            let mut s = String::new();
            for i in 0..keep {
                let (dst, src) = (fr.height + i, top - keep + i);
                if dst != src {
                    let _ = write!(s, "s{dst} = s{src}; ");
                }
            }
            let _ = write!(s, "{} 'b{id};", if kind == Lk::Loop { "continue" } else { "break" });
            Ok(s)
        };
        let line: String = match &op {
            Operator::Block { .. } => {
                next += 1;
                ctl.push((Lk::Block, next));
                format!("'b{next}: {{")
            }
            Operator::Loop { .. } => {
                next += 1;
                ctl.push((Lk::Loop, next));
                if experiment("notick") {
                    format!("'b{next}: loop {{")
                } else {
                    format!("'b{next}: loop {{ fuel -= 1; if fuel < 0 {{ c.fuel = fuel; c.out_of_fuel()?; fuel = c.fuel; }}")
                }
            }
            Operator::If { .. } => {
                next += 1;
                ctl.push((Lk::If, next));
                let cond = if dead { "false".to_string() } else { format!("s{} as u32 != 0", h - 1) };
                format!("'b{next}: {{ if {cond} {{")
            }
            Operator::Else => "} else {".into(),
            Operator::End => match ctl.pop().ok_or("unbalanced end")? {
                (Lk::Root, _) => if dead { String::new() } else { ret(h) },
                (Lk::Block, _) => "}".into(),
                (Lk::Loop, id) => format!("break 'b{id}; }}"),
                (Lk::If, _) => "} }".into(),
            },
            _ if dead => String::new(),
            Operator::Nop | Operator::Drop => String::new(),
            Operator::Unreachable => "return Err(Trap::Unreachable);".into(),
            Operator::Return => ret(h),
            Operator::Br { relative_depth } => jump(&fv, *relative_depth, h)?,
            Operator::BrIf { relative_depth } => {
                format!("if s{} as u32 != 0 {{ {} }}", h - 1, jump(&fv, *relative_depth, h - 1)?)
            }
            Operator::BrTable { targets } => {
                let mut s = format!("match s{} as u32 {{ ", h - 1);
                for (i, d) in targets.targets().enumerate() {
                    let _ = write!(s, "{i} => {{ {} }} ", jump(&fv, d.map_err(e)?, h - 1)?);
                }
                let _ = write!(s, "_ => {{ {} }} }}", jump(&fv, targets.default(), h - 1)?);
                s
            }
            Operator::Call { function_index } => {
                let f = *function_index as usize;
                let cty = &m.types[m.func_types[f] as usize];
                let (n, cr) = (cty.params().len(), cty.results().len());
                let a: Vec<String> = (h - n..h).map(|i| format!("s{i}")).collect();
                let (x, mark) = if f < m.imports.len() {
                    (format!("c.import(m, {f}, &[{}])?", a.join(", ")), String::new())
                } else {
                    callees.push(f as u32);
                    (format!("f{f}(c, m, sb{})?", a.iter().map(|x| format!(", {x}")).collect::<String>()), format!("/*T{idx}:{f}*/"))
                };
                call_into(&mark, &x, cr, h - n)
            }
            Operator::CallIndirect { type_index, table_index } => {
                if *table_index != 0 {
                    return Err("only one table is supported".into());
                }
                let cty = &m.types[*type_index as usize];
                let (n, cr) = (cty.params().len(), cty.results().len());
                m.indirect.insert(*type_index);
                let a: String = (h - 1 - n..h - 1).map(|i| format!(", s{i}")).collect();
                // An indirect call can go anywhere, recursion included: always tick.
                let x = format!("ci{type_index}(c, m, sb, s{}{a})?", h - 1);
                call_into(&format!("/*TI{idx}*/"), &x, cr, h - 1 - n)
            }
            Operator::LocalGet { local_index } => format!("s{h} = l{local_index};"),
            Operator::LocalSet { local_index } | Operator::LocalTee { local_index } => {
                format!("l{local_index} = s{};", h - 1)
            }
            Operator::GlobalGet { global_index } => format!("s{h} = c.globals[{global_index}];"),
            Operator::GlobalSet { global_index } => format!("c.globals[{global_index}] = s{};", h - 1),
            Operator::I32Const { value } => format!("s{h} = {:#x};", *value as u32 as u64),
            Operator::I64Const { value } => format!("s{h} = {:#x};", *value as u64),
            Operator::F32Const { value } => format!("s{h} = {:#x};", value.bits() as u64),
            Operator::F64Const { value } => format!("s{h} = {:#x};", value.bits()),
            Operator::Select | Operator::TypedSelect { .. } => {
                format!("s{} = if s{} as u32 != 0 {{ s{} }} else {{ s{} }};", h - 3, h - 1, h - 3, h - 2)
            }
            Operator::MemorySize { mem: 0 } => format!("s{h} = rt::size(m);"),
            Operator::MemoryGrow { mem: 0 } => format!("s{0} = rt::grow(m, c.max_pages, s{0});", h - 1),
            Operator::MemoryCopy { dst_mem: 0, src_mem: 0 } => format!("rt::copy(m, s{}, s{}, s{})?;", h - 3, h - 2, h - 1),
            Operator::MemoryFill { mem: 0 } => format!("rt::fill(m, s{}, s{}, s{})?;", h - 3, h - 2, h - 1),
            Operator::MemoryInit { data_index, mem: 0 } => {
                format!("c.mem_init(m, {data_index}, s{}, s{}, s{})?;", h - 3, h - 2, h - 1)
            }
            Operator::DataDrop { data_index } => format!("c.dropped[{data_index}] = true;"),
            other => {
                let unchecked = |t: &str| {
                    if experiment("unchecked") {
                        t.replace("rt::ld", "rt::unchecked::ld").replace("rt::st", "rt::unchecked::st").replace(")?", ")")
                    } else {
                        t.to_string()
                    }
                };
                if let Some((load, off)) = load_expr(other) {
                    let load = unchecked(load);
                    format!("s{0} = {1};", h - 1, load.replace("%a", &format!("s{}", h - 1)).replace("%o", &off.to_string()))
                } else if let Some((store, off)) = store_expr(other) {
                    unchecked(store).replace("%a", &format!("s{}", h - 2)).replace("%v", &format!("s{}", h - 1)).replace("%o", &off.to_string())
                } else if let Some(t) = bin_expr(other) {
                    format!("s{} = {};", h - 2, t.replace("%a", &format!("s{}", h - 2)).replace("%b", &format!("s{}", h - 1)))
                } else if let Some(t) = un_expr(other) {
                    format!("s{} = {};", h - 1, t.replace("%a", &format!("s{}", h - 1)))
                } else {
                    return Err(format!("unsupported operator {other:?}"));
                }
            }
        };
        b.push_str(&line);
        b.push('\n');
        fv.op(offset, &op).map_err(e)?;
        max_h = max_h.max(fv.operand_stack_height() as usize);
    }
    r.finish().map_err(e)?;
    let _ = writeln!(out, "fn f{idx}(c: &mut Ctx, m: &mut rt::Mem, sb: u32{}) -> Result<{}, Trap> {{", (0..np).map(|i| format!(", p{i}: u64")).collect::<String>(), ret_ty(nr));
    if !experiment("nostack") {
        // `sb` is the stack budget left, in bytes, carried in a register. Each
        // function charges a conservative estimate of its frame: every value
        // spilled, twice over, plus a margin. Big frames also check the real
        // stack, so a low estimate can never overflow it.
        let cost = (np + nl + max_h + 1) * 16 + 256;
        let _ = writeln!(out, "if sb < {cost} {{ return Err(Trap::CallStackExhausted); }} let sb = sb - {cost};");
        if cost > 4096 {
            out.push_str("c.enter()?;\n");
        }
    }
    // Fuel lives in a register; it is saved to the cell only around calls and
    // returns. Loops spend it at their heads. `place_ticks` decides where else
    // to spend it, once it can see the whole call graph: `/*E..*/` marks the
    // entry and `/*T..*/` each call.
    let has_loop = b.contains(": loop {");
    if has_loop {
        out.push_str("let mut fuel = c.fuel;\n");
    } else {
        b = b.replace("c.fuel = fuel; ", "").replace(" fuel = c.fuel;", "");
    }
    let _ = writeln!(out, "/*E{idx}*/");
    m.fns.push((idx, has_loop, callees, size));
    for i in 0..nl {
        if i < np {
            let _ = writeln!(out, "let mut l{i}: u64 = p{i};");
        } else {
            let _ = writeln!(out, "let mut l{i}: u64 = 0;");
        }
    }
    for i in 0..=max_h {
        let _ = writeln!(out, "let mut s{i}: u64 = 0;");
    }
    out.push_str(&b);
    out.push_str("Err(Trap::Unreachable)\n}\n\n");
    Ok(())
}

fn load_expr(op: &Operator) -> Option<(&'static str, u32)> {
    let (t, ma) = match op {
        Operator::I32Load { memarg } | Operator::F32Load { memarg } | Operator::I64Load32U { memarg } => {
            ("rt::ld4(m, %a, %o)? as u64", memarg)
        }
        Operator::I64Load { memarg } | Operator::F64Load { memarg } => ("rt::ld8(m, %a, %o)?", memarg),
        Operator::I32Load8U { memarg } | Operator::I64Load8U { memarg } => ("rt::ld1(m, %a, %o)? as u64", memarg),
        Operator::I32Load16U { memarg } | Operator::I64Load16U { memarg } => ("rt::ld2(m, %a, %o)? as u64", memarg),
        Operator::I32Load8S { memarg } => ("rt::ld1(m, %a, %o)? as i8 as i32 as u32 as u64", memarg),
        Operator::I32Load16S { memarg } => ("rt::ld2(m, %a, %o)? as i16 as i32 as u32 as u64", memarg),
        Operator::I64Load8S { memarg } => ("rt::ld1(m, %a, %o)? as i8 as i64 as u64", memarg),
        Operator::I64Load16S { memarg } => ("rt::ld2(m, %a, %o)? as i16 as i64 as u64", memarg),
        Operator::I64Load32S { memarg } => ("rt::ld4(m, %a, %o)? as i32 as i64 as u64", memarg),
        _ => return None,
    };
    (ma.memory == 0).then_some((t, ma.offset as u32))
}

fn store_expr(op: &Operator) -> Option<(&'static str, u32)> {
    let (t, ma) = match op {
        Operator::I32Store { memarg } | Operator::F32Store { memarg } | Operator::I64Store32 { memarg } => {
            ("rt::st4(m, %a, %o, %v as u32)?;", memarg)
        }
        Operator::I64Store { memarg } | Operator::F64Store { memarg } => ("rt::st8(m, %a, %o, %v)?;", memarg),
        Operator::I32Store8 { memarg } | Operator::I64Store8 { memarg } => ("rt::st1(m, %a, %o, %v as u8)?;", memarg),
        Operator::I32Store16 { memarg } | Operator::I64Store16 { memarg } => {
            ("rt::st2(m, %a, %o, %v as u16)?;", memarg)
        }
        _ => return None,
    };
    (ma.memory == 0).then_some((t, ma.offset as u32))
}

fn bin_expr(op: &Operator) -> Option<&'static str> {
    use Operator::*;
    Some(match op {
        I32Add => "(%a as u32).wrapping_add(%b as u32) as u64",
        I32Sub => "(%a as u32).wrapping_sub(%b as u32) as u64",
        I32Mul => "(%a as u32).wrapping_mul(%b as u32) as u64",
        I32DivS => "rt::i32_div_s(%a, %b)?",
        I32DivU => "rt::i32_div_u(%a, %b)?",
        I32RemS => "rt::i32_rem_s(%a, %b)?",
        I32RemU => "rt::i32_rem_u(%a, %b)?",
        I32And => "(%a & %b)",
        I32Or => "(%a | %b)",
        I32Xor => "(%a ^ %b)",
        I32Shl => "(%a as u32).wrapping_shl(%b as u32) as u64",
        I32ShrS => "((%a as u32 as i32).wrapping_shr(%b as u32) as u32) as u64",
        I32ShrU => "(%a as u32).wrapping_shr(%b as u32) as u64",
        I32Rotl => "(%a as u32).rotate_left(%b as u32 % 32) as u64",
        I32Rotr => "(%a as u32).rotate_right(%b as u32 % 32) as u64",
        I32Eq => "((%a as u32) == (%b as u32)) as u64",
        I32Ne => "((%a as u32) != (%b as u32)) as u64",
        I32LtS => "((%a as u32 as i32) < (%b as u32 as i32)) as u64",
        I32LtU => "((%a as u32) < (%b as u32)) as u64",
        I32GtS => "((%a as u32 as i32) > (%b as u32 as i32)) as u64",
        I32GtU => "((%a as u32) > (%b as u32)) as u64",
        I32LeS => "((%a as u32 as i32) <= (%b as u32 as i32)) as u64",
        I32LeU => "((%a as u32) <= (%b as u32)) as u64",
        I32GeS => "((%a as u32 as i32) >= (%b as u32 as i32)) as u64",
        I32GeU => "((%a as u32) >= (%b as u32)) as u64",
        I64Add => "%a.wrapping_add(%b)",
        I64Sub => "%a.wrapping_sub(%b)",
        I64Mul => "%a.wrapping_mul(%b)",
        I64DivS => "rt::i64_div_s(%a, %b)?",
        I64DivU => "rt::i64_div_u(%a, %b)?",
        I64RemS => "rt::i64_rem_s(%a, %b)?",
        I64RemU => "rt::i64_rem_u(%a, %b)?",
        I64And => "(%a & %b)",
        I64Or => "(%a | %b)",
        I64Xor => "(%a ^ %b)",
        I64Shl => "%a.wrapping_shl(%b as u32)",
        I64ShrS => "((%a as i64).wrapping_shr(%b as u32) as u64)",
        I64ShrU => "%a.wrapping_shr(%b as u32)",
        I64Rotl => "%a.rotate_left((%b % 64) as u32)",
        I64Rotr => "%a.rotate_right((%b % 64) as u32)",
        I64Eq => "(%a == %b) as u64",
        I64Ne => "(%a != %b) as u64",
        I64LtS => "((%a as i64) < (%b as i64)) as u64",
        I64LtU => "(%a < %b) as u64",
        I64GtS => "((%a as i64) > (%b as i64)) as u64",
        I64GtU => "(%a > %b) as u64",
        I64LeS => "((%a as i64) <= (%b as i64)) as u64",
        I64LeU => "(%a <= %b) as u64",
        I64GeS => "((%a as i64) >= (%b as i64)) as u64",
        I64GeU => "(%a >= %b) as u64",
        F32Add => "(f32::from_bits(%a as u32) + f32::from_bits(%b as u32)).to_bits() as u64",
        F32Sub => "(f32::from_bits(%a as u32) - f32::from_bits(%b as u32)).to_bits() as u64",
        F32Mul => "(f32::from_bits(%a as u32) * f32::from_bits(%b as u32)).to_bits() as u64",
        F32Div => "(f32::from_bits(%a as u32) / f32::from_bits(%b as u32)).to_bits() as u64",
        F32Min => "rt::fmin32(f32::from_bits(%a as u32), f32::from_bits(%b as u32)).to_bits() as u64",
        F32Max => "rt::fmax32(f32::from_bits(%a as u32), f32::from_bits(%b as u32)).to_bits() as u64",
        F32Copysign => "f32::from_bits(%a as u32).copysign(f32::from_bits(%b as u32)).to_bits() as u64",
        F32Eq => "(f32::from_bits(%a as u32) == f32::from_bits(%b as u32)) as u64",
        F32Ne => "(f32::from_bits(%a as u32) != f32::from_bits(%b as u32)) as u64",
        F32Lt => "(f32::from_bits(%a as u32) < f32::from_bits(%b as u32)) as u64",
        F32Gt => "(f32::from_bits(%a as u32) > f32::from_bits(%b as u32)) as u64",
        F32Le => "(f32::from_bits(%a as u32) <= f32::from_bits(%b as u32)) as u64",
        F32Ge => "(f32::from_bits(%a as u32) >= f32::from_bits(%b as u32)) as u64",
        F64Add => "(f64::from_bits(%a) + f64::from_bits(%b)).to_bits()",
        F64Sub => "(f64::from_bits(%a) - f64::from_bits(%b)).to_bits()",
        F64Mul => "(f64::from_bits(%a) * f64::from_bits(%b)).to_bits()",
        F64Div => "(f64::from_bits(%a) / f64::from_bits(%b)).to_bits()",
        F64Min => "rt::fmin64(f64::from_bits(%a), f64::from_bits(%b)).to_bits()",
        F64Max => "rt::fmax64(f64::from_bits(%a), f64::from_bits(%b)).to_bits()",
        F64Copysign => "f64::from_bits(%a).copysign(f64::from_bits(%b)).to_bits()",
        F64Eq => "(f64::from_bits(%a) == f64::from_bits(%b)) as u64",
        F64Ne => "(f64::from_bits(%a) != f64::from_bits(%b)) as u64",
        F64Lt => "(f64::from_bits(%a) < f64::from_bits(%b)) as u64",
        F64Gt => "(f64::from_bits(%a) > f64::from_bits(%b)) as u64",
        F64Le => "(f64::from_bits(%a) <= f64::from_bits(%b)) as u64",
        F64Ge => "(f64::from_bits(%a) >= f64::from_bits(%b)) as u64",
        _ => return None,
    })
}

fn un_expr(op: &Operator) -> Option<&'static str> {
    use Operator::*;
    Some(match op {
        I32Eqz => "((%a as u32) == 0) as u64",
        I64Eqz => "(%a == 0) as u64",
        I32Clz => "(%a as u32).leading_zeros() as u64",
        I32Ctz => "(%a as u32).trailing_zeros() as u64",
        I32Popcnt => "(%a as u32).count_ones() as u64",
        I64Clz => "%a.leading_zeros() as u64",
        I64Ctz => "%a.trailing_zeros() as u64",
        I64Popcnt => "%a.count_ones() as u64",
        F32Abs => "f32::from_bits(%a as u32).abs().to_bits() as u64",
        F32Neg => "(-f32::from_bits(%a as u32)).to_bits() as u64",
        F32Ceil => "f32::from_bits(%a as u32).ceil().to_bits() as u64",
        F32Floor => "f32::from_bits(%a as u32).floor().to_bits() as u64",
        F32Trunc => "f32::from_bits(%a as u32).trunc().to_bits() as u64",
        F32Nearest => "f32::from_bits(%a as u32).round_ties_even().to_bits() as u64",
        F32Sqrt => "f32::from_bits(%a as u32).sqrt().to_bits() as u64",
        F64Abs => "f64::from_bits(%a).abs().to_bits()",
        F64Neg => "(-f64::from_bits(%a)).to_bits()",
        F64Ceil => "f64::from_bits(%a).ceil().to_bits()",
        F64Floor => "f64::from_bits(%a).floor().to_bits()",
        F64Trunc => "f64::from_bits(%a).trunc().to_bits()",
        F64Nearest => "f64::from_bits(%a).round_ties_even().to_bits()",
        F64Sqrt => "f64::from_bits(%a).sqrt().to_bits()",
        I32WrapI64 => "(%a as u32) as u64",
        I32TruncF32S => "(rt::trunc(f32::from_bits(%a as u32) as f64, -2147483648.0, 2147483648.0)? as i32 as u32) as u64",
        I32TruncF32U => "(rt::trunc(f32::from_bits(%a as u32) as f64, 0.0, 4294967296.0)? as u32) as u64",
        I32TruncF64S => "(rt::trunc(f64::from_bits(%a), -2147483648.0, 2147483648.0)? as i32 as u32) as u64",
        I32TruncF64U => "(rt::trunc(f64::from_bits(%a), 0.0, 4294967296.0)? as u32) as u64",
        I64ExtendI32S => "(%a as u32 as i32 as i64) as u64",
        I64ExtendI32U => "(%a as u32) as u64",
        I64TruncF32S => "(rt::trunc(f32::from_bits(%a as u32) as f64, -9223372036854775808.0, 9223372036854775808.0)? as i64) as u64",
        I64TruncF32U => "(rt::trunc(f32::from_bits(%a as u32) as f64, 0.0, 18446744073709551616.0)? as u64)",
        I64TruncF64S => "(rt::trunc(f64::from_bits(%a), -9223372036854775808.0, 9223372036854775808.0)? as i64) as u64",
        I64TruncF64U => "(rt::trunc(f64::from_bits(%a), 0.0, 18446744073709551616.0)? as u64)",
        F32ConvertI32S => "((%a as u32 as i32) as f32).to_bits() as u64",
        F32ConvertI32U => "((%a as u32) as f32).to_bits() as u64",
        F32ConvertI64S => "((%a as i64) as f32).to_bits() as u64",
        F32ConvertI64U => "(%a as f32).to_bits() as u64",
        F32DemoteF64 => "(f64::from_bits(%a) as f32).to_bits() as u64",
        F64ConvertI32S => "((%a as u32 as i32) as f64).to_bits()",
        F64ConvertI32U => "((%a as u32) as f64).to_bits()",
        F64ConvertI64S => "((%a as i64) as f64).to_bits()",
        F64ConvertI64U => "(%a as f64).to_bits()",
        F64PromoteF32 => "(f32::from_bits(%a as u32) as f64).to_bits()",
        // Values are raw bits already, so reinterpreting is free.
        I32ReinterpretF32 | I64ReinterpretF64 | F32ReinterpretI32 | F64ReinterpretI64 => "%a",
        I32Extend8S => "(%a as u8 as i8 as i32 as u32) as u64",
        I32Extend16S => "(%a as u16 as i16 as i32 as u32) as u64",
        I64Extend8S => "(%a as u8 as i8 as i64) as u64",
        I64Extend16S => "(%a as u16 as i16 as i64) as u64",
        I64Extend32S => "(%a as u32 as i32 as i64) as u64",
        // Rust's `as` already saturates and maps NaN to 0, exactly like wasm.
        I32TruncSatF32S => "(f32::from_bits(%a as u32) as i32 as u32) as u64",
        I32TruncSatF32U => "(f32::from_bits(%a as u32) as u32) as u64",
        I32TruncSatF64S => "(f64::from_bits(%a) as i32 as u32) as u64",
        I32TruncSatF64U => "(f64::from_bits(%a) as u32) as u64",
        I64TruncSatF32S => "(f32::from_bits(%a as u32) as i64) as u64",
        I64TruncSatF32U => "(f32::from_bits(%a as u32) as u64)",
        I64TruncSatF64S => "(f64::from_bits(%a) as i64) as u64",
        I64TruncSatF64U => "(f64::from_bits(%a) as u64)",
        _ => return None,
    })
}

/// Most operators a function may run, callees included, before it must spend
/// fuel. At 50 us slices this is noise, and it keeps tiny helpers tick-free.
const TICK_FREE_WORK: usize = 4096;

/// Spend fuel wherever long work could run without passing a loop head:
/// - every call that can recurse (inside a cycle of the call graph),
/// - every indirect call (it could recurse too),
/// - the entry of any function whose worst-case work without a tick is big.
///   Loop-free code can still blow up: f calls g twice, g calls h twice...
///
/// So any stretch with no tick is bounded, and a recursive call (fib) always
/// ticks. A small helper, or the common path of a drop, never does.
fn place_ticks(m: &Info, funcs: &str) -> String {
    let off = m.imports.len() as u32;
    let n = m.fns.len();
    let at = |f: u32| (f - off) as usize;
    // Tarjan's SCCs: they come out callees first.
    struct T<'a> {
        fns: &'a [(u32, bool, Vec<u32>, usize)],
        off: u32,
        index: Vec<usize>,
        low: Vec<usize>,
        on: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        scc: Vec<usize>,
        order: Vec<Vec<usize>>,
    }
    fn start(t: &mut T, v: usize) {
        t.index[v] = t.next;
        t.low[v] = t.next;
        t.next += 1;
        t.stack.push(v);
        t.on[v] = true;
    }
    // With a stack of its own, not recursion: a module's call chain can be
    // a million deep, and the builder's thread stack can't.
    fn visit(t: &mut T, root: usize) {
        start(t, root);
        let mut work = vec![(root, 0usize)];
        while let Some(&(v, i)) = work.last() {
            if let Some(&g) = t.fns[v].2.get(i) {
                work.last_mut().unwrap().1 += 1;
                let w = (g - t.off) as usize;
                if t.index[w] == usize::MAX {
                    start(t, w);
                    work.push((w, 0));
                } else if t.on[w] {
                    t.low[v] = t.low[v].min(t.index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(p, _)) = work.last() {
                t.low[p] = t.low[p].min(t.low[v]);
            }
            finish(t, v);
        }
    }
    fn finish(t: &mut T, v: usize) {
        if t.low[v] == t.index[v] {
            let mut comp = vec![];
            loop {
                let w = t.stack.pop().unwrap();
                t.on[w] = false;
                t.scc[w] = t.order.len();
                comp.push(w);
                if w == v {
                    break;
                }
            }
            t.order.push(comp);
        }
    }
    let mut t = T {
        fns: &m.fns,
        off,
        index: vec![usize::MAX; n],
        low: vec![0; n],
        on: vec![false; n],
        stack: vec![],
        next: 0,
        scc: vec![0; n],
        order: vec![],
    };
    for v in 0..n {
        if t.index[v] == usize::MAX {
            visit(&mut t, v);
        }
    }
    // Worst-case work before the first tick. A callee that ticks on entry
    // counts as zero: its tick starts a new stretch.
    let mut work = vec![0usize; n];
    let mut entry_tick = vec![false; n];
    for comp in &t.order {
        for &v in comp {
            let mut w = m.fns[v].3;
            for &g in &m.fns[v].2 {
                let g = at(g);
                if t.scc[g] != t.scc[v] && !entry_tick[g] {
                    w = w.saturating_add(work[g]);
                }
            }
            work[v] = w;
            // EXPERIMENT ONLY: `entrytick` is the old rule, a tick on entry to
            // every function that calls.
            entry_tick[v] = if experiment("entrytick") { !m.fns[v].2.is_empty() } else { w > TICK_FREE_WORK };
        }
    }
    let tick = |f: u32| {
        if experiment("notick") {
            ""
        } else if m.fns[at(f)].1 {
            "fuel -= 1; if fuel < 0 { c.fuel = fuel; c.out_of_fuel()?; fuel = c.fuel; } "
        } else {
            "c.fuel -= 1; if c.fuel < 0 { c.out_of_fuel()?; } "
        }
    };
    let mut out = String::with_capacity(funcs.len());
    let mut rest = funcs;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        let end = i + rest[i..].find("*/").unwrap() + 2;
        let mark = &rest[i + 2..end - 2];
        if let Some(f) = mark.strip_prefix("TI") {
            out.push_str(tick(f.parse().unwrap()));
        } else if let Some(f) = mark.strip_prefix('E') {
            let f: u32 = f.parse().unwrap();
            if entry_tick[at(f)] {
                out.push_str(tick(f));
            }
        } else if let Some(fg) = mark.strip_prefix('T') {
            let (f, g) = fg.split_once(':').unwrap();
            let (f, g): (u32, u32) = (f.parse().unwrap(), g.parse().unwrap());
            if t.scc[at(f)] == t.scc[at(g)] && !experiment("entrytick") {
                out.push_str(tick(f));
            }
        } else {
            out.push_str(&rest[i..end]); // not ours
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    /// A module's function names go into the generated source as comments,
    /// and that source is compiled on a builder: a name with a line break in
    /// it must not get out of its comment. (It did: `compile_error!` with
    /// `include_str!` put a builder's files in the error a deploy shows.)
    #[test]
    fn a_function_name_cannot_inject_code() {
        let name = "x\ncompile_error!(include_str!(\"/etc/hostname\"));\r\u{2028}\u{85}y";
        let mut wasm = wat::parse_str("(module (func (export \"f\")))").unwrap();
        // A "name" section: function names (subsection 1), f0 = `name`.
        let mut sub = vec![1, 0, name.len() as u8];
        sub.extend_from_slice(name.as_bytes());
        let mut sec = vec![4];
        sec.extend_from_slice(b"name");
        sec.extend_from_slice(&[1, sub.len() as u8]);
        sec.extend_from_slice(&sub);
        wasm.extend_from_slice(&[0, sec.len() as u8]);
        wasm.extend_from_slice(&sec);
        let src = super::generate(&wasm, "test").unwrap();
        assert!(src.contains("// f0 = "), "the name is written:\n{src}");
        for line in src.lines().filter(|l| l.contains("compile_error")) {
            assert!(line.trim_start().starts_with("//"), "out of its comment: {line:?}");
        }
    }

    /// A call chain 100,000 deep: the call graph's walk takes no stack per
    /// level (it overflowed the builder's, recursing).
    #[test]
    fn a_deep_call_chain_compiles() {
        let n = 100_000;
        let mut w = String::from("(module\n");
        for i in 0..n - 1 {
            w.push_str(&format!("(func call {})\n", i + 1));
        }
        w.push_str("(func) (export \"f\" (func 0)))");
        let src = super::generate(&wat::parse_str(&w).unwrap(), "test").unwrap();
        assert!(src.contains(&format!("fn f{}(", n - 1)));
    }

    /// Every cell allocates its table at start: a huge one is refused, as
    /// the interpreter refuses it, not allocated (16 GB for 2e9 entries).
    #[test]
    fn a_huge_table_is_refused() {
        let wasm = wat::parse_str("(module (table 2000000000 funcref))").unwrap();
        assert_eq!(super::generate(&wasm, "test").err().as_deref(), Some("table too large"));
        let ok = wat::parse_str("(module (table 1024 funcref))").unwrap();
        assert!(super::generate(&ok, "test").is_ok());
    }
}
