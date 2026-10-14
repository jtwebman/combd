//! jsc: combd's JS-to-Rust compiler.
//!
//! `jsc app.js out/` writes a Rust crate that builds to a Wasm cell. The cell
//! then runs on combd like any other: the AOT tier gives it yields, guard
//! pages, and let-it-crash for free.
//!
//! Every JS value becomes a `combd_jsrt::Value`. Top-level functions become
//! plain Rust functions, so calls between them are direct. Arrows become
//! closures that copy what they capture. Anything outside the subset we
//! support stops the build with a clear error and a line number.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use std::fmt::Write as _;

mod num;

type Res<T> = Result<T, String>;

#[derive(Clone)]
enum Var {
    /// A Rust local. `captured`: a closure copied it. `assigned`: code writes it.
    Local { rust: String, captured: bool, assigned: bool },
    /// A top-level binding, kept in a thread-local slot.
    Global(String),
    /// A top-level function declaration: a plain Rust fn.
    TopFn(String),
    /// A local that's always a number: a Rust `f64`.
    Num(String),
}

#[derive(Default)]
struct Scope {
    vars: Vec<(String, Var)>,
}

/// A closure being compiled: where its scopes start, and what it captures.
struct FnCtx {
    depth: usize,
    captures: Vec<String>,
}

/// Where `break` and `continue` go.
struct Target {
    label: String,
    /// The Rust for `continue`; None for a switch.
    cont: Option<String>,
}

struct C<'s> {
    src: &'s str,
    scopes: Vec<Scope>,
    funcs: Vec<FnCtx>,
    targets: Vec<Target>,
    statics: String,
    /// Inline-cache slots, one per `o.k` site.
    ics: String,
    items: String,
    n: usize,
    /// Functions with an `f64` twin: (JS name, twin's Rust name, arity).
    num_fns: Vec<(String, String, usize)>,
    /// Compiling a twin, where everything must be typed.
    num_mode: bool,
}

/// Built-ins we call directly: JS name -> function in `combd_jsrt::b`.
const BUILTINS: &[(&str, &str)] = &[
    ("Number", "number"),
    ("String", "string"),
    ("Math.round", "math_round"),
    ("Math.floor", "math_floor"),
    ("Math.min", "math_min"),
    ("Math.max", "math_max"),
    ("JSON.parse", "json_parse"),
    ("JSON.stringify", "json_stringify"),
    ("Array.from", "array_from"),
    ("console.log", "console_log"),
    ("combd.counter", "combd_counter"),
];
const CONSTRUCTORS: &[(&str, &str)] = &[("RegExp", "new_regexp"), ("Error", "new_error")];

fn builtin(table: &[(&str, &'static str)], name: &str) -> Option<&'static str> {
    table.iter().find(|(js, _)| *js == name).map(|(_, rs)| *rs)
}

fn rust_name(prefix: &str, js: &str) -> String {
    format!("{prefix}{}", js.replace('$', "_S_"))
}

fn num(x: f64) -> String {
    if x.is_nan() {
        "V::Num(f64::NAN)".into()
    } else if x.is_infinite() {
        "V::Num(f64::INFINITY)".into()
    } else {
        format!("V::Num({x:?})")
    }
}

fn lit(s: &str) -> String {
    format!("V::Lit({s:?})")
}

impl<'s> C<'s> {
    fn err(&self, span: Span, what: &str) -> String {
        let before = &self.src[..span.start as usize];
        let line = before.matches('\n').count() + 1;
        let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
        let snippet: String = self.src[span.start as usize..span.end as usize].chars().take(60).collect();
        format!("app.js:{line}:{col}: jsc doesn't support {what} yet: `{snippet}`")
    }

    fn tmp(&mut self) -> String {
        self.n += 1;
        format!("t{}", self.n)
    }

    fn ic(&mut self) -> String {
        self.n += 1;
        let _ = writeln!(self.ics, "static IC{}: AtomicU32 = AtomicU32::new(0);", self.n);
        format!("IC{}", self.n)
    }

    fn label(&mut self) -> String {
        self.n += 1;
        format!("'l{}", self.n)
    }

    fn at_top(&self) -> bool {
        self.scopes.len() == 1 && self.funcs.is_empty()
    }

    /// Find a name. Reading an outer function's local from a closure records
    /// it as a capture of every closure in between.
    fn lookup(&mut self, name: &str) -> Option<(Var, bool)> {
        for si in (0..self.scopes.len()).rev() {
            let Some(pos) = self.scopes[si].vars.iter().rposition(|(n, _)| n == name) else { continue };
            let mut crossed = false;
            if let Var::Local { rust, captured, .. } = &mut self.scopes[si].vars[pos].1 {
                for f in self.funcs.iter_mut().filter(|f| f.depth > si) {
                    if !f.captures.contains(rust) {
                        f.captures.push(rust.clone());
                    }
                    crossed = true;
                }
                *captured |= crossed;
            }
            return Some((self.scopes[si].vars[pos].1.clone(), crossed));
        }
        None
    }

    fn declare(&mut self, name: &str, val: String) -> String {
        if self.at_top() {
            return format!("rt::gset(&{}, {val});\n", rust_name("G_", name));
        }
        let rust = rust_name("v_", name);
        self.scopes.last_mut().unwrap().vars.push((name.into(), Var::Local { rust: rust.clone(), captured: false, assigned: false }));
        format!("let mut {rust} = {val};\n")
    }

    // ---- the program ----

    fn program(&mut self, p: &Program) -> Res<String> {
        // Hoist: every top-level name is known before any code is compiled.
        let mut top = Scope::default();
        for s in &p.body {
            match s {
                Statement::FunctionDeclaration(f) => {
                    let name = f.id.as_ref().unwrap().name.as_str();
                    top.vars.push((name.into(), Var::TopFn(rust_name("f_", name))));
                }
                Statement::VariableDeclaration(d) => {
                    for decl in &d.declarations {
                        for name in pattern_names(&decl.id) {
                            let g = rust_name("G_", name);
                            let _ = writeln!(self.statics, "static {g}: RefCell<V> = const {{ RefCell::new(V::Undef) }};");
                            top.vars.push((name.into(), Var::Global(g)));
                        }
                    }
                }
                _ => {}
            }
        }
        self.scopes.push(top);
        self.twins(p);
        let mut init = String::new();
        for s in &p.body {
            match s {
                Statement::FunctionDeclaration(f) => self.top_fn(f)?,
                s => init += &self.stmt(s)?,
            }
        }
        Ok(format!(
            "// Generated by jsc. Do not edit.\n\
             #![allow(warnings)]\n\
             use combd_jsrt as rt;\n\
             use rt::{{Key, Value as V, R}};\n\
             use std::cell::RefCell;\n\
             use std::sync::atomic::AtomicU32;\n\n\
             thread_local! {{\n{}}}\n\n{}\n{}\n\
             fn init() -> R {{\n{init}Ok(V::Undef)\n}}\n\n\
             rt::cell!(init);\n",
            self.statics, self.ics, self.items
        ))
    }

    fn top_fn(&mut self, f: &Function) -> Res<()> {
        if f.generator || f.r#async {
            return Err(self.err(f.span, "async or generator functions"));
        }
        let name = rust_name("f_", f.id.as_ref().unwrap().name.as_str());
        let body = self.function(&f.params, f.body.as_ref().unwrap(), false)?.0;
        let _ = writeln!(self.items, "fn {name}(this: &V, args: &[V]) -> R {{\n{body}}}\n");
        Ok(())
    }

    /// A function body. Returns the Rust body and what it captures.
    fn function(&mut self, params: &FormalParameters, body: &FunctionBody, expr_body: bool) -> Res<(String, Vec<String>)> {
        self.funcs.push(FnCtx { depth: self.scopes.len(), captures: vec![] });
        self.scopes.push(Scope::default());
        let targets = std::mem::take(&mut self.targets);
        let r = (|| -> Res<String> {
            let (simple, mut b) = self.params(params)?;
            if expr_body {
                let Some(Statement::ExpressionStatement(e)) = body.statements.first() else { unreachable!() };
                b += &format!("Ok({})\n", self.expr(&e.expression)?);
            } else {
                b += &self.stmts(&body.statements)?;
                b += "Ok(V::Undef)\n";
            }
            // Plain parameters that are only read borrow the caller's value.
            let mut pro = String::new();
            for (i, pos) in simple {
                let Var::Local { rust, captured, assigned } = &self.scopes.last().unwrap().vars[pos].1 else { unreachable!() };
                pro += &if *captured || *assigned {
                    format!("let mut {rust} = rt::arg(args, {i});\n")
                } else {
                    format!("let {rust} = rt::arg_ref(args, {i});\n")
                };
            }
            Ok(pro + &b)
        })();
        self.targets = targets;
        self.scopes.pop();
        let f = self.funcs.pop().unwrap();
        Ok((r?, f.captures))
    }

    fn closure(&mut self, params: &FormalParameters, body: &FunctionBody, expr_body: bool) -> Res<String> {
        let (b, caps) = self.function(params, body, expr_body)?;
        let clones: String = caps.iter().map(|c| format!("let {c} = {c}.clone(); ")).collect();
        Ok(format!("{{ {clones}V::fun(move |this: &V, args: &[V]| -> R {{\n{b}}}) }}"))
    }

    /// Binds parameters. Plain ones are only declared here: `function` picks
    /// how to bind them once the body shows how they're used.
    fn params(&mut self, p: &FormalParameters) -> Res<(Vec<(usize, usize)>, String)> {
        let mut s = String::new();
        let mut simple = vec![];
        for (i, item) in p.items.iter().enumerate() {
            if let (BindingPattern::BindingIdentifier(id), None) = (&item.pattern, &item.initializer) {
                let _ = self.declare(id.name.as_str(), String::new());
                simple.push((i, self.scopes.last().unwrap().vars.len() - 1));
                continue;
            }
            let mut val = format!("rt::arg(args, {i})");
            if let Some(d) = &item.initializer {
                val = self.with_default(val, d)?;
            }
            s += &self.bind(&item.pattern, val)?;
        }
        if let Some(rest) = &p.rest {
            let n = p.items.len();
            s += &self.bind(&rest.rest.argument, format!("V::arr(args.get({n}..).map_or(vec![], |a| a.to_vec()))"))?;
        }
        Ok((simple, s))
    }

    fn with_default(&mut self, val: String, d: &Expression) -> Res<String> {
        let t = self.tmp();
        let d = self.expr(d)?;
        Ok(format!("{{ let {t} = {val}; if matches!({t}, V::Undef) {{ {d} }} else {{ {t} }} }}"))
    }

    /// Bind a pattern (`x`, `{a, b}`, `[a, ...rest]`) to a value.
    fn bind(&mut self, p: &BindingPattern, val: String) -> Res<String> {
        Ok(match p {
            BindingPattern::BindingIdentifier(id) => self.declare(id.name.as_str(), val),
            BindingPattern::ObjectPattern(o) => {
                if let Some(r) = &o.rest {
                    return Err(self.err(r.span, "...rest in object patterns"));
                }
                let t = self.tmp();
                let mut s = format!("let {t} = {val};\n");
                for prop in &o.properties {
                    let get = match (prop.computed, prop.key.static_name()) {
                        (false, Some(k)) => format!("rt::get_ic(&{t}, {k:?}, &{})?", self.ic()),
                        _ => {
                            let k = self.expr(prop.key.as_expression().unwrap())?;
                            format!("rt::index(&{t}, &({k}))?")
                        }
                    };
                    s += &self.bind(&prop.value, get)?;
                }
                s
            }
            BindingPattern::ArrayPattern(a) => {
                let t = self.tmp();
                let mut s = format!("let {t} = {val};\n");
                for (i, el) in a.elements.iter().enumerate() {
                    if let Some(el) = el {
                        s += &self.bind(el, format!("rt::at(&{t}, {i})?"))?;
                    }
                }
                if let Some(r) = &a.rest {
                    s += &self.bind(&r.argument, format!("rt::rest(&{t}, {})?", a.elements.len()))?;
                }
                s
            }
            BindingPattern::AssignmentPattern(ap) => {
                let v = self.with_default(val, &ap.right)?;
                self.bind(&ap.left, v)?
            }
        })
    }

    // ---- statements ----

    fn stmts(&mut self, list: &[Statement]) -> Res<String> {
        let mut s = String::new();
        for st in list {
            if let Statement::FunctionDeclaration(f) = st {
                return Err(self.err(f.span, "functions declared inside functions (use an arrow)"));
            }
            s += &self.stmt(st)?;
        }
        Ok(s)
    }

    /// A nested statement or block, in its own scope.
    fn block(&mut self, st: &Statement) -> Res<String> {
        self.scopes.push(Scope::default());
        let r = match st {
            Statement::BlockStatement(b) => self.stmts(&b.body),
            st => self.stmts(std::slice::from_ref(st)),
        };
        self.scopes.pop();
        r
    }

    fn stmt(&mut self, st: &Statement) -> Res<String> {
        Ok(match st {
            Statement::VariableDeclaration(d) => self.var_decl(d)?,
            Statement::ExpressionStatement(e) => match &e.expression {
                Expression::AssignmentExpression(a) => format!("{};\n", self.assign(a, false)?),
                e => format!("let _ = {};\n", self.expr(e)?),
            },
            Statement::ReturnStatement(r) => match &r.argument {
                Some(e) => format!("return Ok({});\n", self.expr(e)?),
                None => "return Ok(V::Undef);\n".into(),
            },
            Statement::ThrowStatement(t) => format!("return Err({});\n", self.expr(&t.argument)?),
            Statement::IfStatement(i) => {
                let c = self.cond(&i.test)?;
                let a = self.block(&i.consequent)?;
                match &i.alternate {
                    Some(alt) => format!("if {c} {{\n{a}}} else {{\n{}}}\n", self.block(alt)?),
                    None => format!("if {c} {{\n{a}}}\n"),
                }
            }
            Statement::BlockStatement(_) => format!("{{\n{}}}\n", self.block(st)?),
            Statement::EmptyStatement(_) => String::new(),
            Statement::ForStatement(f) => {
                self.scopes.push(Scope::default());
                let r = self.for_loop(f);
                self.scopes.pop();
                r?
            }
            Statement::ForOfStatement(f) => {
                if f.r#await {
                    return Err(self.err(f.span, "for await"));
                }
                let ForStatementLeft::VariableDeclaration(d) = &f.left else {
                    return Err(self.err(f.left.span(), "for…of without a declaration"));
                };
                let (l, a, i) = (self.label(), self.tmp(), self.tmp());
                let arr = self.expr(&f.right)?;
                self.scopes.push(Scope::default());
                let bind = self.bind(&d.declarations[0].id, "item".into())?;
                self.targets.push(Target { label: l.clone(), cont: Some(format!("continue {l}")) });
                let body = self.block(&f.body);
                self.targets.pop();
                self.scopes.pop();
                format!(
                    "{{ let {a} = rt::iter(&({arr}))?; let mut {i} = 0usize;\n\
                     {l}: while let Some(item) = rt::next(&{a}, &mut {i}) {{\n{bind}{}}} }}\n",
                    body?
                )
            }
            Statement::WhileStatement(w) => {
                let l = self.label();
                let c = self.cond(&w.test)?;
                self.targets.push(Target { label: l.clone(), cont: Some(format!("continue {l}")) });
                let body = self.block(&w.body);
                self.targets.pop();
                format!("{l}: while {c} {{\n{}}}\n", body?)
            }
            Statement::SwitchStatement(sw) => self.switch(sw)?,
            Statement::BreakStatement(b) => {
                if b.label.is_some() {
                    return Err(self.err(b.span, "labeled break"));
                }
                let t = self.targets.last().ok_or_else(|| self.err(b.span, "break outside a loop"))?;
                format!("break {};\n", t.label)
            }
            Statement::ContinueStatement(c) => {
                if c.label.is_some() {
                    return Err(self.err(c.span, "labeled continue"));
                }
                let t = self.targets.iter().rev().find_map(|t| t.cont.clone());
                format!("{};\n", t.ok_or_else(|| self.err(c.span, "continue outside a loop"))?)
            }
            st => return Err(self.err(st.span(), "this statement")),
        })
    }

    fn var_decl(&mut self, d: &VariableDeclaration) -> Res<String> {
        let mut s = String::new();
        for decl in &d.declarations {
            // A const that's always a number lives in an f64.
            if let (VariableDeclarationKind::Const, BindingPattern::BindingIdentifier(id), Some(init), false) =
                (d.kind, &decl.id, &decl.init, self.at_top())
            {
                if let Some(n) = self.num(init) {
                    s += &format!("let {}: f64 = {n};\n", self.declare_num(id.name.as_str()));
                    continue;
                }
            }
            let val = match &decl.init {
                Some(e) => self.expr(e)?,
                None => "V::Undef".into(),
            };
            s += &self.bind(&decl.id, val)?;
        }
        Ok(s)
    }

    fn for_loop(&mut self, f: &ForStatement) -> Res<String> {
        let (l, c) = (self.label(), self.label());
        let init = match &f.init {
            None => String::new(),
            Some(ForStatementInit::VariableDeclaration(d)) => self.var_decl(d)?,
            Some(e) => format!("let _ = {};\n", self.expr(e.as_expression().unwrap())?),
        };
        let test = match &f.test {
            Some(t) => self.cond(t)?,
            None => "true".into(),
        };
        // The update compiles before the body, so a closure in the body
        // copies this turn's value, like JS's per-turn `let`.
        let update = match &f.update {
            Some(Expression::AssignmentExpression(a)) => format!("{};", self.assign(a, false)?),
            Some(e) => format!("let _ = {};", self.expr(e)?),
            None => String::new(),
        };
        self.targets.push(Target { label: l.clone(), cont: Some(format!("break {c}")) });
        let body = self.block(&f.body);
        self.targets.pop();
        Ok(format!("{{\n{init}{l}: loop {{\nif !({test}) {{ break; }}\n{c}: {{\n{}}}\n{update}\n}} }}\n", body?))
    }

    fn switch(&mut self, sw: &SwitchStatement) -> Res<String> {
        let (l, d, start) = (self.label(), self.tmp(), self.tmp());
        let mut s = format!("{l}: {{ let {d} = {};\nlet {start} = ", self.expr(&sw.discriminant)?);
        let mut default = sw.cases.len();
        for (i, case) in sw.cases.iter().enumerate() {
            match &case.test {
                Some(t) => s += &format!("if rt::seq(&{d}, &({})) {{ {i} }} else ", self.expr(t)?),
                None => default = i,
            }
        }
        s += &format!("{{ {default} }};\n");
        // Cases fall through: each one runs if we started at or before it.
        self.targets.push(Target { label: l.clone(), cont: None });
        self.scopes.push(Scope::default());
        let mut r = Ok(());
        for (i, case) in sw.cases.iter().enumerate() {
            match self.stmts(&case.consequent) {
                Ok(b) => s += &format!("if {start} <= {i} {{\n{b}}}\n"),
                Err(e) => {
                    r = Err(e);
                    break;
                }
            }
        }
        self.scopes.pop();
        self.targets.pop();
        r?;
        Ok(s + "}\n")
    }

    // ---- expressions: each one is Rust that makes an owned Value ----

    /// A `&Value` for `e`, borrowing locals instead of copying them.
    fn eref(&mut self, e: &Expression) -> Res<String> {
        if let Expression::Identifier(id) = e {
            if let Some((Var::Local { rust, .. }, _)) = self.lookup(id.name.as_str()) {
                return Ok(format!("&{rust}"));
            }
        }
        Ok(format!("&({})", self.expr(e)?))
    }

    /// `e` as a Rust bool.
    fn cond(&mut self, e: &Expression) -> Res<String> {
        if let Some(c) = self.num_cond(e) {
            return Ok(c);
        }
        Ok(match e {
            Expression::BinaryExpression(b) => {
                let f = match b.operator {
                    BinaryOperator::StrictEquality => "rt::seq",
                    BinaryOperator::StrictInequality => "!rt::seq",
                    BinaryOperator::LessThan => "rt::lt",
                    BinaryOperator::LessEqualThan => "rt::le",
                    BinaryOperator::GreaterThan => "rt::gt",
                    BinaryOperator::GreaterEqualThan => "rt::ge",
                    _ => return Ok(format!("rt::truthy(&({}))", self.expr(e)?)),
                };
                let (l, r) = (self.eref(&b.left)?, self.eref(&b.right)?);
                format!("{f}({l}, {r})")
            }
            Expression::UnaryExpression(u) if u.operator == UnaryOperator::LogicalNot => format!("!({})", self.cond(&u.argument)?),
            Expression::LogicalExpression(l) if l.operator != LogicalOperator::Coalesce => {
                let op = if l.operator == LogicalOperator::And { "&&" } else { "||" };
                format!("({} {op} {})", self.cond(&l.left)?, self.cond(&l.right)?)
            }
            Expression::ParenthesizedExpression(p) => self.cond(&p.expression)?,
            Expression::BooleanLiteral(b) => b.value.to_string(),
            e => format!("rt::truthy({})", self.eref(e)?),
        })
    }

    fn args(&mut self, args: &[Argument]) -> Res<String> {
        let mut out = vec![];
        for a in args {
            match a.as_expression() {
                Some(e) => out.push(self.expr(e)?),
                None => return Err(self.err(a.span(), "spread arguments")),
            }
        }
        Ok(format!("&[{}]", out.join(", ")))
    }

    fn ident(&mut self, name: &str, span: Span) -> Res<String> {
        Ok(match self.lookup(name) {
            Some((Var::Local { rust, .. }, _)) => format!("{rust}.clone()"),
            Some((Var::Global(g), _)) => format!("rt::gget(&{g})"),
            Some((Var::TopFn(f), _)) => format!("V::fun({f})"),
            Some((Var::Num(n), _)) => format!("V::Num({n})"),
            None => match name {
                "undefined" => "V::Undef".into(),
                "NaN" => "V::Num(f64::NAN)".into(),
                "Infinity" => "V::Num(f64::INFINITY)".into(),
                "globalThis" => "rt::global_this()".into(),
                _ => return Err(self.err(span, &format!("the global `{name}`"))),
            },
        })
    }

    fn expr(&mut self, e: &Expression) -> Res<String> {
        if matches!(
            e,
            Expression::BinaryExpression(_)
                | Expression::UnaryExpression(_)
                | Expression::CallExpression(_)
                | Expression::ConditionalExpression(_)
                | Expression::Identifier(_)
        ) {
            if let Some(n) = self.num(e) {
                return Ok(format!("V::Num({n})"));
            }
        }
        Ok(match e {
            Expression::NumericLiteral(n) => num(n.value),
            Expression::StringLiteral(s) => lit(s.value.as_str()),
            Expression::BooleanLiteral(b) => format!("V::Bool({})", b.value),
            Expression::NullLiteral(_) => "V::Null".into(),
            Expression::TemplateLiteral(t) => {
                let mut parts = vec![];
                for (i, q) in t.quasis.iter().enumerate() {
                    let cooked = q.value.cooked.as_ref().ok_or_else(|| self.err(q.span, "invalid escapes in templates"))?;
                    if !cooked.is_empty() {
                        parts.push(lit(cooked.as_str()));
                    }
                    if let Some(e) = t.expressions.get(i) {
                        parts.push(self.expr(e)?);
                    }
                }
                match (parts.len(), t.expressions.is_empty()) {
                    (0, _) => lit(""),
                    (1, true) => parts.pop().unwrap(),
                    _ => format!("rt::concat(&[{}])", parts.join(", ")),
                }
            }
            Expression::RegExpLiteral(r) => {
                self.n += 1;
                let name = format!("RE{}", self.n);
                let (src, flags) = (r.regex.pattern.text.as_str(), r.regex.flags.to_string());
                let _ = writeln!(self.statics, "static {name}: V = rt::b::regexp({src:?}, {flags:?}).unwrap();");
                format!("{name}.with(V::clone)")
            }
            Expression::Identifier(id) => self.ident(id.name.as_str(), id.span)?,
            Expression::ArrayExpression(a) => {
                let mut out = vec![];
                for el in &a.elements {
                    match el {
                        ArrayExpressionElement::Elision(_) => out.push("V::Undef".into()),
                        ArrayExpressionElement::SpreadElement(s) => return Err(self.err(s.span, "spread in arrays")),
                        el => out.push(self.expr(el.as_expression().unwrap())?),
                    }
                }
                format!("V::arr(vec![{}])", out.join(", "))
            }
            Expression::ObjectExpression(o) => self.object(o)?,
            Expression::ArrowFunctionExpression(f) => {
                if f.r#async {
                    return Err(self.err(f.span, "async arrows"));
                }
                self.closure(&f.params, &f.body, f.expression)?
            }
            Expression::FunctionExpression(f) => {
                if f.generator || f.r#async {
                    return Err(self.err(f.span, "async or generator functions"));
                }
                // ponytail: no `this`, so a function expression acts like an arrow
                self.closure(&f.params, f.body.as_ref().unwrap(), false)?
            }
            Expression::AssignmentExpression(a) => self.assign(a, true)?,
            Expression::BinaryExpression(b) => {
                let f = match b.operator {
                    BinaryOperator::Addition => "rt::add",
                    BinaryOperator::Subtraction => "rt::sub",
                    BinaryOperator::Multiplication => "rt::mul",
                    BinaryOperator::Division => "rt::div",
                    BinaryOperator::Remainder => "rt::rem",
                    BinaryOperator::StrictEquality
                    | BinaryOperator::StrictInequality
                    | BinaryOperator::LessThan
                    | BinaryOperator::LessEqualThan
                    | BinaryOperator::GreaterThan
                    | BinaryOperator::GreaterEqualThan => return Ok(format!("V::Bool({})", self.cond(e)?)),
                    op => return Err(self.err(b.span, &format!("the `{}` operator", op.as_str()))),
                };
                format!("{f}({}, {})", self.eref(&b.left)?, self.eref(&b.right)?)
            }
            Expression::LogicalExpression(l) => {
                let t = self.tmp();
                let (a, b) = (self.expr(&l.left)?, self.expr(&l.right)?);
                let test = match l.operator {
                    LogicalOperator::And => format!("!rt::truthy(&{t})"),
                    LogicalOperator::Or => format!("rt::truthy(&{t})"),
                    LogicalOperator::Coalesce => format!("!matches!({t}, V::Undef | V::Null)"),
                };
                format!("{{ let {t} = {a}; if {test} {{ {t} }} else {{ {b} }} }}")
            }
            Expression::UnaryExpression(u) => {
                let a = &u.argument;
                match u.operator {
                    UnaryOperator::LogicalNot => format!("V::Bool(!{})", self.cond(a)?),
                    UnaryOperator::UnaryNegation => format!("rt::neg({})", self.eref(a)?),
                    UnaryOperator::UnaryPlus => format!("rt::pos({})", self.eref(a)?),
                    UnaryOperator::Typeof => format!("rt::type_of({})", self.eref(a)?),
                    UnaryOperator::Void => format!("{{ let _ = {}; V::Undef }}", self.expr(a)?),
                    op => return Err(self.err(u.span, &format!("the `{}` operator", op.as_str()))),
                }
            }
            Expression::UpdateExpression(u) => {
                let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument else {
                    return Err(self.err(u.span, "++ and -- on properties"));
                };
                let (read, store) = self.assign_target(id.name.as_str(), id.span)?;
                let step = if u.operator == UpdateOperator::Increment { "1.0" } else { "-1.0" };
                if u.prefix {
                    format!("{{ let new = rt::add(&rt::pos(&({read})), &V::Num({step})); {} new }}", store("new.clone()"))
                } else {
                    format!("{{ let old = rt::pos(&({read})); {} old }}", store(&format!("rt::add(&old, &V::Num({step}))")))
                }
            }
            Expression::ConditionalExpression(c) => {
                format!("(if {} {{ {} }} else {{ {} }})", self.cond(&c.test)?, self.expr(&c.consequent)?, self.expr(&c.alternate)?)
            }
            Expression::SequenceExpression(s) => {
                let mut out = String::from("{ ");
                for (i, e) in s.expressions.iter().enumerate() {
                    let e = self.expr(e)?;
                    out += &if i + 1 < s.expressions.len() { format!("let _ = {e}; ") } else { format!("{e} }}") };
                }
                out
            }
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression)?,
            Expression::CallExpression(c) => self.call(c)?,
            Expression::NewExpression(n) => {
                let ctor = match &n.callee {
                    Expression::Identifier(id) if self.lookup(id.name.as_str()).is_none() => builtin(CONSTRUCTORS, id.name.as_str()),
                    _ => None,
                };
                let Some(f) = ctor else { return Err(self.err(n.span, "this `new`")) };
                format!("rt::b::{f}({})?", self.args(&n.arguments)?)
            }
            Expression::StaticMemberExpression(m) => {
                if m.optional {
                    return Err(self.err(m.span, "optional chaining"));
                }
                let o = self.eref(&m.object)?;
                format!("rt::get_ic({o}, {:?}, &{})?", m.property.name.as_str(), self.ic())
            }
            Expression::ComputedMemberExpression(m) => {
                if m.optional {
                    return Err(self.err(m.span, "optional chaining"));
                }
                format!("rt::index({}, {})?", self.eref(&m.object)?, self.eref(&m.expression)?)
            }
            e => return Err(self.err(e.span(), "this expression")),
        })
    }

    fn object(&mut self, o: &ObjectExpression) -> Res<String> {
        let mut keys = vec![];
        let mut fields = vec![];
        let mut simple = true;
        for p in &o.properties {
            match p {
                ObjectPropertyKind::ObjectProperty(p) => {
                    if p.kind != PropertyKind::Init {
                        return Err(self.err(p.span, "getters and setters"));
                    }
                    let key = match (p.computed, p.key.static_name()) {
                        (false, Some(k)) => {
                            simple &= !keys.contains(&k.to_string());
                            keys.push(k.to_string());
                            format!("Key::Lit({k:?})")
                        }
                        _ => {
                            simple = false;
                            format!("rt::to_key(&({}))", self.expr(p.key.as_expression().unwrap())?)
                        }
                    };
                    fields.push((Some(key), self.expr(&p.value)?));
                }
                ObjectPropertyKind::SpreadProperty(s) => {
                    simple = false;
                    fields.push((None, self.expr(&s.argument)?));
                }
            }
        }
        if simple {
            // All keys known and distinct: build the property list in one go.
            let f: Vec<String> = fields.into_iter().map(|(k, v)| format!("({}, {v})", k.unwrap())).collect();
            return Ok(format!("V::obj(vec![{}])", f.join(", ")));
        }
        let mut s = String::from("{ let o = rt::Obj::default(); ");
        for (k, v) in fields {
            s += &match k {
                Some(k) => format!("o.put({k}, {v}); "),
                None => format!("rt::spread(&o, &({v})); "),
            };
        }
        Ok(s + "V::Obj(std::rc::Rc::new(o)) }")
    }

    fn call(&mut self, c: &CallExpression) -> Res<String> {
        if c.optional {
            return Err(self.err(c.span, "optional calls"));
        }
        match &c.callee {
            Expression::Identifier(id) => {
                let name = id.name.as_str();
                match self.lookup(name) {
                    Some((Var::TopFn(f), _)) => {
                        let twin = self.num_fns.iter().find(|(js, _, n)| js == name && *n == c.arguments.len()).cloned();
                        let Some((_, twin, n)) = twin else {
                            return Ok(format!("{f}(&V::Undef, {})?", self.args(&c.arguments)?));
                        };
                        // Numbers take the twin; anything else, the general version.
                        let mut s = String::from("{ ");
                        for (i, a) in c.arguments.iter().enumerate() {
                            let a = a.as_expression().ok_or_else(|| self.err(c.span, "spread arguments"))?;
                            s += &format!("let a{i} = {}; ", self.expr(a)?);
                        }
                        let pats: Vec<String> = (0..n).map(|i| format!("V::Num(x{i})")).collect();
                        let refs: Vec<String> = (0..n).map(|i| format!("&a{i}")).collect();
                        let xs: Vec<String> = (0..n).map(|i| format!("*x{i}")).collect();
                        let vals: Vec<String> = (0..n).map(|i| format!("a{i}")).collect();
                        s += &format!(
                            "if let ({},) = ({},) {{ V::Num({twin}({})) }} else {{ {f}(&V::Undef, &[{}])? }} }}",
                            pats.join(", "),
                            refs.join(", "),
                            xs.join(", "),
                            vals.join(", ")
                        );
                        return Ok(s);
                    }
                    None if builtin(BUILTINS, name).is_some() => {
                        return Ok(format!("rt::b::{}({})?", builtin(BUILTINS, name).unwrap(), self.args(&c.arguments)?));
                    }
                    _ => {}
                }
            }
            Expression::StaticMemberExpression(m) => {
                let method = m.property.name.as_str();
                if let Expression::Identifier(o) = &m.object {
                    let full = format!("{}.{method}", o.name);
                    if let Some(b) = builtin(BUILTINS, &full).filter(|_| self.lookup(o.name.as_str()).is_none()) {
                        return Ok(format!("rt::b::{b}({})?", self.args(&c.arguments)?));
                    }
                }
                let recv = self.eref(&m.object)?;
                return Ok(format!("rt::call_method({recv}, {method:?}, {})?", self.args(&c.arguments)?));
            }
            _ => {}
        }
        let f = self.eref(&c.callee)?;
        Ok(format!("rt::call({f}, &V::Undef, {})?", self.args(&c.arguments)?))
    }

    /// How to read and write a plain variable. Writing one a closure copied
    /// would leave the copy stale, so that's an error for now.
    #[allow(clippy::type_complexity)]
    fn assign_target(&mut self, name: &str, span: Span) -> Res<(String, Box<dyn Fn(&str) -> String>)> {
        match self.lookup(name) {
            Some((Var::Local { rust, captured, .. }, crossed)) => {
                if captured || crossed {
                    return Err(self.err(span, &format!("assigning to `{name}` after a closure captured it")));
                }
                for sc in self.scopes.iter_mut().rev() {
                    if let Some((_, Var::Local { assigned, .. })) = sc.vars.iter_mut().rev().find(|(n, _)| n == name) {
                        *assigned = true;
                        break;
                    }
                }
                let r2 = rust.clone();
                Ok((format!("{rust}.clone()"), Box::new(move |v| format!("{r2} = {v};"))))
            }
            Some((Var::Global(g), _)) => {
                let g2 = g.clone();
                Ok((format!("rt::gget(&{g})"), Box::new(move |v| format!("rt::gset(&{g2}, {v});"))))
            }
            Some((Var::Num(_), _)) => Err(self.err(span, &format!("assigning to the const `{name}`"))),
            _ => Err(self.err(span, &format!("assigning to `{name}`"))),
        }
    }

    /// `a = b`, `a.k += b`, `a[k] = b`. `value`: the expression's result is used.
    fn assign(&mut self, a: &AssignmentExpression, value: bool) -> Res<String> {
        let op = match a.operator {
            AssignmentOperator::Assign => None,
            AssignmentOperator::Addition => Some("rt::add"),
            AssignmentOperator::Subtraction => Some("rt::sub"),
            AssignmentOperator::Multiplication => Some("rt::mul"),
            AssignmentOperator::Division => Some("rt::div"),
            AssignmentOperator::Remainder => Some("rt::rem"),
            op => return Err(self.err(a.span, &format!("the `{}` operator", op.as_str()))),
        };
        let t = self.tmp();
        let (o, k) = (self.tmp(), self.tmp());
        let (setup, read, store): (String, String, Box<dyn Fn(&str) -> String>) = match &a.left {
            AssignmentTarget::AssignmentTargetIdentifier(id) => {
                let (read, store) = self.assign_target(id.name.as_str(), id.span)?;
                (String::new(), read, store)
            }
            AssignmentTarget::StaticMemberExpression(m) => {
                let name = m.property.name.as_str().to_string();
                let setup = format!("let {o} = {}; ", self.expr(&m.object)?);
                let o2 = o.clone();
                let n2 = name.clone();
                (setup, format!("rt::get(&{o}, {name:?})?"), Box::new(move |v| format!("rt::set(&{o2}, Key::Lit({n2:?}), {v})?;")))
            }
            AssignmentTarget::ComputedMemberExpression(m) => {
                let setup = format!("let {o} = {}; let {k} = {}; ", self.expr(&m.object)?, self.expr(&m.expression)?);
                let (o2, k2) = (o.clone(), k.clone());
                (setup, format!("rt::index(&{o}, &{k})?"), Box::new(move |v| format!("rt::set_index(&{o2}, &{k2}, {v})?;")))
            }
            l => return Err(self.err(l.span(), "destructuring assignment")),
        };
        let rhs = self.expr(&a.right)?;
        let val = match op {
            None => rhs,
            Some(f) => format!("{f}(&({read}), &({rhs}))"),
        };
        Ok(if value {
            format!("{{ {setup}let {t} = {val}; {} {t} }}", store(&format!("{t}.clone()")))
        } else {
            format!("{{ {setup}{} }}", store(&val))
        })
    }
}

/// Every name a pattern binds.
fn pattern_names<'a>(p: &'a BindingPattern<'a>) -> Vec<&'a str> {
    match p {
        BindingPattern::BindingIdentifier(id) => vec![id.name.as_str()],
        BindingPattern::ObjectPattern(o) => {
            let mut v: Vec<_> = o.properties.iter().flat_map(|p| pattern_names(&p.value)).collect();
            v.extend(o.rest.iter().flat_map(|r| pattern_names(&r.argument)));
            v
        }
        BindingPattern::ArrayPattern(a) => {
            let mut v: Vec<_> = a.elements.iter().flatten().flat_map(pattern_names).collect();
            v.extend(a.rest.iter().flat_map(|r| pattern_names(&r.argument)));
            v
        }
        BindingPattern::AssignmentPattern(a) => pattern_names(&a.left),
    }
}

pub fn compile(src: &str) -> Res<String> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, src, SourceType::mjs()).parse();
    if ret.panicked || ret.diagnostics.has_errors() {
        let all: Vec<String> = ret.diagnostics.iter().map(|d| format!("app.js: {d}")).collect();
        return Err(all.join("\n"));
    }
    let mut c = C { src, scopes: vec![], funcs: vec![], targets: vec![], statics: String::new(), ics: String::new(), items: String::new(), n: 0, num_fns: vec![], num_mode: false };
    c.program(&ret.program)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, out] = &args[..] else {
        eprintln!("usage: jsc app.js out_dir");
        std::process::exit(2);
    };
    let src = std::fs::read_to_string(input).unwrap_or_else(|e| panic!("read {input}: {e}"));
    let code = match compile(&src) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let rt = concat!(env!("CARGO_MANIFEST_DIR"), "/rt");
    let out = std::path::Path::new(out);
    std::fs::create_dir_all(out.join("src")).unwrap();
    std::fs::write(
        out.join("Cargo.toml"),
        format!(
            "# Generated by jsc. Do not edit.\n[package]\nname = \"combd-js-app\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
             [workspace]\n\n[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n\n\
             [dependencies]\ncombd-jsrt = {{ path = {rt:?} }}\n\n\
             [profile.release]\nopt-level = 3\nlto = true\ncodegen-units = 1\npanic = \"abort\"\nstrip = \"debuginfo\"\n"
        ),
    )
    .unwrap();
    let lib = out.join("src/lib.rs");
    std::fs::write(&lib, code).unwrap();
    // Readable output helps when something goes wrong; it's fine without.
    let _ = std::process::Command::new("rustfmt").arg("--edition=2021").arg(&lib).status();
}
