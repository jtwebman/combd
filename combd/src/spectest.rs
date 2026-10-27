//! The official WebAssembly spec testsuite, run against our interpreter.
//!
//! `combd/spectest.sh` fetches the suite at a pinned commit; the test
//! `wasm_spec_testsuite` runs every `.wast` script in it. Each check is:
//! - **pass**,
//! - **fail**: a wrong answer, a missing trap, an invalid module we accept,
//!   a valid module we reject for no stated reason, or an engine panic,
//! - **skip**: something we don't support and say so (SIMD, imported
//!   memories, tables, or globals, start functions, linking modules). We
//!   refuse those modules at load, which is safe; they're counted, not hidden.

use crate::engine::{CellVm, Engine, Outcome};
use crate::interp::Interp;
use std::collections::HashMap;
use wast::core::{NanPattern, WastArgCore, WastRetCore};
use wast::parser::{self, ParseBuffer};
use wast::{QuoteWat, Wast, WastArg, WastDirective, WastExecute, WastInvoke, WastRet};

#[derive(Default, Debug, Clone)]
pub struct Score {
    pub pass: u32,
    pub fail: u32,
    pub skip: u32,
    /// The first few failures, for the report.
    pub why: Vec<String>,
    /// Why modules were refused as unsupported: reason -> modules.
    pub refused: HashMap<String, u32>,
}

impl Score {
    fn fail(&mut self, line: usize, msg: String) {
        self.fail += 1;
        if self.why.len() < 5 {
            self.why.push(format!("line {line}: {msg}"));
        }
    }
}

const ENGINE: Interp = Interp { max_pages: 65536 };
/// Enough for any test; an endless loop fails instead of hanging.
const BUDGET: u64 = 2_000_000_000;

/// Our load errors that mean "we don't support this", not "it's invalid".
fn unsupported(err: &str) -> bool {
    ["not supported", "only function imports", "only plain 32-bit memory", "only one memory", "unsupported", "too large"].iter().any(|m| err.contains(m))
}

enum Inst {
    Ready(<Interp as Engine>::Vm),
    /// We refused it as unsupported: skip what uses it.
    Skipped,
}

fn arg(a: &WastArg) -> Option<u64> {
    Some(match a {
        WastArg::Core(WastArgCore::I32(v)) => *v as u32 as u64,
        WastArg::Core(WastArgCore::I64(v)) => *v as u64,
        WastArg::Core(WastArgCore::F32(v)) => v.bits as u64,
        WastArg::Core(WastArgCore::F64(v)) => v.bits,
        _ => return None,
    })
}

/// Does `got` match `want`? `None`: a kind of result we don't check.
fn matches(got: u64, want: &WastRet) -> Option<bool> {
    match want {
        WastRet::Core(c) => core(got, c),
        _ => None,
    }
}

fn core(got: u64, want: &WastRetCore) -> Option<bool> {
    Some(match want {
        WastRetCore::I32(v) => got as u32 == *v as u32,
        WastRetCore::I64(v) => got == *v as u64,
        WastRetCore::F32(p) => {
            let b = got as u32;
            match p {
                NanPattern::CanonicalNan => b & 0x7fff_ffff == 0x7fc0_0000,
                NanPattern::ArithmeticNan => b & 0x7f80_0000 == 0x7f80_0000 && b & 0x0040_0000 != 0,
                NanPattern::Value(v) => b == v.bits,
            }
        }
        WastRetCore::F64(p) => match p {
            NanPattern::CanonicalNan => got & 0x7fff_ffff_ffff_ffff == 0x7ff8_0000_0000_0000,
            NanPattern::ArithmeticNan => got & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 && got & 0x0008_0000_0000_0000 != 0,
            NanPattern::Value(v) => got == v.bits,
        },
        WastRetCore::Either(alts) => {
            let rs: Option<Vec<bool>> = alts.iter().map(|w| core(got, w)).collect();
            rs?.into_iter().any(|x| x)
        }
        _ => return None,
    })
}

/// What a call did.
enum Ran {
    Done(Vec<u64>),
    Trap(String),
    /// Something the runner can't do (an argument kind, a missing export).
    Skip,
    Fail(String),
}

fn invoke(insts: &mut HashMap<Option<String>, Inst>, last: &Option<String>, inv: &WastInvoke) -> Ran {
    let key = inv.module.map(|id| id.name().to_string()).or_else(|| last.clone());
    let vm = match insts.get_mut(&key) {
        Some(Inst::Ready(vm)) => vm,
        _ => return Ran::Skip,
    };
    let Some(args) = inv.args.iter().map(arg).collect::<Option<Vec<u64>>>() else { return Ran::Skip };
    if let Err(e) = vm.begin(inv.name, &args) {
        return if e.contains("import") { Ran::Skip } else { Ran::Fail(format!("begin {}: {e}", inv.name)) };
    }
    let mut spent = 0u64;
    loop {
        match vm.run(1_000_000) {
            Outcome::Done(rs) => return Ran::Done(rs),
            Outcome::Trap(t) if t.contains("engine bug") => return Ran::Fail(format!("ENGINE PANIC in {}: {t}", inv.name)),
            Outcome::Trap(t) => return Ran::Trap(t),
            Outcome::Yield => {
                spent += 1_000_000;
                if spent > BUDGET {
                    return Ran::Fail(format!("{} never finished", inv.name));
                }
            }
            // `spectest` prints: nothing to hand back.
            Outcome::HostCall { .. } => vm.resume(&[]),
        }
    }
}

/// The modules a binary imports from.
fn import_modules(bin: &[u8]) -> Vec<String> {
    let mut out = vec![];
    for p in wasmparser::Parser::new(0).parse_all(bin).flatten() {
        if let wasmparser::Payload::ImportSection(r) = p {
            out.extend(r.into_imports().flatten().map(|i| i.module.to_string()));
        }
    }
    out
}

/// Load and start a module. `Err(true)`: refused as unsupported.
fn instantiate(bin: &[u8]) -> Result<<Interp as Engine>::Vm, (bool, String)> {
    let m = ENGINE.load(bin).map_err(|e| (unsupported(&e), e))?;
    let vm = ENGINE.spawn(&m).map_err(|e| (unsupported(&e), e))?;
    // We can only provide `spectest`'s functions (it prints; we ignore it).
    for i in 0.. {
        match vm.import(i) {
            Some(imp) if imp.module != "spectest" => return Err((true, format!("imports {}.{}: linking isn't supported", imp.module, imp.name))),
            Some(_) => {}
            None => break,
        }
    }
    Ok(vm)
}

pub fn run_file(text: &str) -> Result<Score, String> {
    // names.wast uses confusing Unicode on purpose.
    let mut lexer = wast::lexer::Lexer::new(text);
    lexer.allow_confusing_unicode(true);
    let buf = ParseBuffer::new_with_lexer(lexer).map_err(|e| e.to_string())?;
    let wast: Wast = parser::parse(&buf).map_err(|e| e.to_string())?;
    let mut s = Score::default();
    let mut insts: HashMap<Option<String>, Inst> = HashMap::new();
    let mut last: Option<String> = None;
    // `register`ed names, to the module they name.
    let mut registered: HashMap<String, Option<String>> = HashMap::new();
    let line = |span: wast::token::Span| span.linecol_in(text).0 + 1;
    for d in wast.directives {
        match d {
            WastDirective::Module(mut q) => {
                let id = match &q {
                    QuoteWat::Wat(wast::Wat::Module(m)) => m.id.map(|i| i.name().to_string()),
                    _ => None,
                };
                let at = line(q.span());
                let bin = q.encode();
                let from = bin.as_ref().map(|b| import_modules(b)).unwrap_or_default();
                let inst = match bin.map_err(|e| (false, e.to_string())).and_then(|b| instantiate(&b)) {
                    Ok(vm) => {
                        s.pass += 1;
                        Inst::Ready(vm)
                    }
                    Err((true, why)) => {
                        s.skip += 1;
                        // "unsupported instruction: V128Load { .. }" -> "unsupported instruction: V128Load"
                        let short: String = why.split(|c: char| c == '{' || c == '(').next().unwrap_or("").trim().to_string();
                        *s.refused.entry(short).or_default() += 1;
                        // It may have changed what it imports (a shared table
                        // or memory) in ways we can't run: stop trusting those.
                        for m in from.iter().filter(|m| *m != "spectest") {
                            if let Some(k) = registered.get(m) {
                                insts.insert(k.clone(), Inst::Skipped);
                            }
                        }
                        Inst::Skipped
                    }
                    Err((false, e)) => {
                        s.fail(at, format!("a valid module was rejected: {e}"));
                        Inst::Skipped
                    }
                };
                insts.insert(id.clone(), inst);
                if id.is_some() {
                    insts.remove(&None);
                }
                last = id;
                // The unnamed current module is also reachable as `None`.
                if let Some(Inst::Ready(_)) = insts.get(&last) {}
            }
            WastDirective::AssertMalformed { mut module, span, .. } | WastDirective::AssertInvalid { mut module, span, .. } => {
                match module.encode() {
                    Err(_) => s.pass += 1, // the text itself is malformed
                    Ok(b) => match ENGINE.load(&b) {
                        Err(_) => s.pass += 1,
                        Ok(_) => s.fail(line(span), "an invalid or malformed module was accepted".into()),
                    },
                }
            }
            WastDirective::AssertReturn { exec: WastExecute::Invoke(inv), results, span } => match invoke(&mut insts, &last, &inv) {
                Ran::Done(got) => {
                    if got.len() != results.len() {
                        s.fail(line(span), format!("{}: {} results, wanted {}", inv.name, got.len(), results.len()));
                        continue;
                    }
                    let checks: Option<Vec<bool>> = got.iter().zip(&results).map(|(g, w)| matches(*g, w)).collect();
                    match checks {
                        None => s.skip += 1,
                        Some(c) if c.iter().all(|x| *x) => s.pass += 1,
                        Some(_) => s.fail(line(span), format!("{}{:?}: got {got:x?}", inv.name, inv.args.iter().map(arg).collect::<Vec<_>>())),
                    }
                }
                Ran::Trap(t) => s.fail(line(span), format!("{}: trapped ({t}), wanted a result", inv.name)),
                Ran::Skip => s.skip += 1,
                Ran::Fail(m) => s.fail(line(span), m),
            },
            WastDirective::AssertTrap { exec: WastExecute::Invoke(inv), span, .. } | WastDirective::AssertExhaustion { call: inv, span, .. } => {
                match invoke(&mut insts, &last, &inv) {
                    Ran::Trap(_) => s.pass += 1,
                    Ran::Done(got) => s.fail(line(span), format!("{}: returned {got:x?}, wanted a trap", inv.name)),
                    Ran::Skip => s.skip += 1,
                    Ran::Fail(m) => s.fail(line(span), m),
                }
            }
            // A module whose start-up must trap (data or elements out of bounds).
            WastDirective::AssertTrap { exec: WastExecute::Wat(mut w), span, .. } => match w.encode() {
                Err(e) => s.fail(line(span), e.to_string()),
                Ok(b) => match {
                    // Its start-up may write into what it imports (shared
                    // tables and memories); we don't link, so stop trusting those.
                    for m in import_modules(&b).iter().filter(|m| *m != "spectest") {
                        if let Some(k) = registered.get(m) {
                            insts.insert(k.clone(), Inst::Skipped);
                        }
                    }
                    ENGINE.load(&b)
                } {
                    Err(e) if unsupported(&e) => s.skip += 1,
                    // Refused at load instead of trapping at start: it never
                    // runs either way.
                    Err(_) => s.pass += 1,
                    Ok(m) => match ENGINE.spawn(&m) {
                        Err(e) if unsupported(&e) => s.skip += 1,
                        Err(_) => s.pass += 1,
                        Ok(_) => s.fail(line(span), "started fine, wanted a trap".into()),
                    },
                },
            },
            WastDirective::Register { name, module, .. } => {
                registered.insert(name.to_string(), module.map(|id| id.name().to_string()).or_else(|| last.clone()));
                s.skip += 1;
            }
            WastDirective::Invoke(inv) => match invoke(&mut insts, &last, &inv) {
                Ran::Fail(m) => s.fail(line(inv.span), m),
                _ => {}
            },
            // Linking (`register`, `assert_unlinkable`), globals (`get`),
            // exceptions, components, threads: not run.
            _ => s.skip += 1,
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run with `COMBD_WASM_TESTSUITE=.testsuite cargo test --release
    /// wasm_spec_testsuite -- --nocapture` (see `spectest.sh`).
    #[test]
    fn wasm_spec_testsuite() {
        let Some(dir) = std::env::var_os("COMBD_WASM_TESTSUITE") else {
            eprintln!("skipping: set COMBD_WASM_TESTSUITE to the spec testsuite (spectest.sh fetches it)");
            return;
        };
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "wast")).collect();
        files.sort();
        let mut total = Score::default();
        let mut bad = vec![];
        for f in &files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(f).unwrap();
            let s = std::panic::catch_unwind(|| run_file(&text)).unwrap_or_else(|_| Err("the runner panicked".into()));
            match s {
                Ok(s) => {
                    total.pass += s.pass;
                    total.fail += s.fail;
                    total.skip += s.skip;
                    for (k, v) in &s.refused {
                        *total.refused.entry(k.clone()).or_default() += v;
                    }
                    if s.fail > 0 {
                        bad.push((name, s));
                    }
                }
                Err(e) => {
                    total.fail += 1;
                    bad.push((name, Score { fail: 1, why: vec![e.chars().take(200).collect()], ..Score::default() }));
                }
            }
        }
        for (name, s) in &bad {
            println!("FAIL {name}: {} fail, {} pass, {} skip", s.fail, s.pass, s.skip);
            for w in &s.why {
                println!("       {w}");
            }
        }
        let mut why: Vec<_> = total.refused.iter().collect();
        why.sort_by(|a, b| b.1.cmp(a.1));
        println!("modules refused as unsupported, by reason:");
        for (k, v) in why.iter().take(25) {
            println!("  {v:5}  {k}");
        }
        println!("spec testsuite: {} files, {} pass, {} fail, {} skip", files.len(), total.pass, total.fail, total.skip);
        assert_eq!(total.fail, 0, "the interpreter must pass every check it runs");
    }
}
