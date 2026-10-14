//! Typed fast paths for number code.
//!
//! A top-level function whose body only does number math also gets a plain
//! `f64` twin, `f_name_n(f64, ..) -> f64`. Calls that pass numbers use the
//! twin; anything else takes the general version, so behavior never changes.
//! `const x = <number math>` inside a function becomes an `f64` local too.
//!
//! "Only number math" is checked, not guessed: every expression must have a
//! number type, every path must `return`, and calls may only go to Math or to
//! other twins. If one check fails, there's simply no twin.

use super::*;

impl C<'_> {
    /// Rust `f64` code for `e`, if `e` is always a number here.
    pub(crate) fn num(&mut self, e: &Expression) -> Option<String> {
        Some(match e {
            Expression::NumericLiteral(n) => f64_lit(n.value),
            Expression::ParenthesizedExpression(p) => self.num(&p.expression)?,
            Expression::Identifier(id) => match self.lookup(id.name.as_str())? {
                (Var::Num(rust), _) => rust,
                _ => return None,
            },
            Expression::UnaryExpression(u) => match u.operator {
                UnaryOperator::UnaryNegation => format!("(-{})", self.as_f64(&u.argument)?),
                UnaryOperator::UnaryPlus => self.as_f64(&u.argument)?,
                _ => return None,
            },
            Expression::BinaryExpression(b) => {
                let op = match b.operator {
                    // Always numbers, whatever the operands are.
                    BinaryOperator::Subtraction => "-",
                    BinaryOperator::Multiplication => "*",
                    BinaryOperator::Division => "/",
                    BinaryOperator::Remainder => "%",
                    // A number only when both sides are.
                    BinaryOperator::Addition => {
                        let (l, r) = (self.num(&b.left)?, self.num(&b.right)?);
                        return Some(format!("({l} + {r})"));
                    }
                    _ => return None,
                };
                format!("({} {op} {})", self.as_f64(&b.left)?, self.as_f64(&b.right)?)
            }
            Expression::ConditionalExpression(c) => {
                let t = self.num_cond(&c.test)?;
                format!("(if {t} {{ {} }} else {{ {} }})", self.num(&c.consequent)?, self.num(&c.alternate)?)
            }
            Expression::CallExpression(c) if !c.optional => self.num_call(c)?,
            _ => return None,
        })
    }

    /// `e` as an `f64`: typed if we can, else ToNumber on the value. In a
    /// twin's body everything must be typed.
    fn as_f64(&mut self, e: &Expression) -> Option<String> {
        if let Some(n) = self.num(e) {
            return Some(n);
        }
        if self.num_mode {
            return None;
        }
        Some(format!("rt::to_num({})", self.eref(e).ok()?))
    }

    fn num_args(&mut self, args: &[Argument]) -> Option<Vec<String>> {
        args.iter().map(|a| self.num(a.as_expression()?)).collect()
    }

    fn num_call(&mut self, c: &CallExpression) -> Option<String> {
        match &c.callee {
            Expression::Identifier(id) => {
                let name = id.name.as_str();
                match self.lookup(name) {
                    Some((Var::TopFn(_), _)) => {
                        let (_, twin, arity) = self.num_fns.iter().find(|(js, _, _)| js == name)?.clone();
                        if c.arguments.len() != arity {
                            return None;
                        }
                        Some(format!("{twin}({})", self.num_args(&c.arguments)?.join(", ")))
                    }
                    None if name == "Number" && c.arguments.len() == 1 => self.as_f64(c.arguments[0].as_expression()?),
                    _ => None,
                }
            }
            Expression::StaticMemberExpression(m) => {
                let Expression::Identifier(o) = &m.object else { return None };
                if o.name != "Math" || self.lookup("Math").is_some() {
                    return None;
                }
                let a = self.num_args(&c.arguments)?;
                Some(match (m.property.name.as_str(), &a[..]) {
                    ("min", [x, y]) => format!("rt::fmin({x}, {y})"),
                    ("max", [x, y]) => format!("rt::fmax({x}, {y})"),
                    ("round", [x]) => format!("rt::fround({x})"),
                    ("floor", [x]) => format!("{x}.floor()"),
                    _ => return None,
                })
            }
            _ => None,
        }
    }

    /// A Rust bool for `e`, if it compares numbers.
    pub(crate) fn num_cond(&mut self, e: &Expression) -> Option<String> {
        Some(match e {
            Expression::BinaryExpression(b) => {
                let op = match b.operator {
                    BinaryOperator::StrictEquality => "==",
                    BinaryOperator::StrictInequality => "!=",
                    BinaryOperator::LessThan => "<",
                    BinaryOperator::LessEqualThan => "<=",
                    BinaryOperator::GreaterThan => ">",
                    BinaryOperator::GreaterEqualThan => ">=",
                    _ => return None,
                };
                format!("({} {op} {})", self.num(&b.left)?, self.num(&b.right)?)
            }
            Expression::UnaryExpression(u) if u.operator == UnaryOperator::LogicalNot => format!("!{}", self.num_cond(&u.argument)?),
            Expression::LogicalExpression(l) if l.operator != LogicalOperator::Coalesce => {
                let op = if l.operator == LogicalOperator::And { "&&" } else { "||" };
                format!("({} {op} {})", self.num_cond(&l.left)?, self.num_cond(&l.right)?)
            }
            Expression::ParenthesizedExpression(p) => self.num_cond(&p.expression)?,
            Expression::BooleanLiteral(b) => b.value.to_string(),
            _ => return None,
        })
    }

    /// Build every twin we can. Twins may call each other, so start by
    /// assuming all of them exist and drop the ones that fail until none do.
    pub(crate) fn twins(&mut self, p: &Program) {
        let fns: Vec<&Function> = p
            .body
            .iter()
            .filter_map(|s| match s {
                Statement::FunctionDeclaration(f) if !f.generator && !f.r#async && f.params.rest.is_none() => Some(&**f),
                _ => None,
            })
            .filter(|f| f.params.items.iter().all(|i| matches!(i.pattern, BindingPattern::BindingIdentifier(_)) && i.initializer.is_none()))
            .collect();
        let name = |f: &Function| f.id.as_ref().unwrap().name.as_str().to_string();
        self.num_fns = fns.iter().map(|f| (name(f), rust_name("f_", &name(f)) + "_n", f.params.items.len())).collect();
        loop {
            let mut code = String::new();
            let mut failed = vec![];
            for f in &fns {
                if !self.num_fns.iter().any(|(js, _, _)| *js == name(f)) {
                    continue;
                }
                match self.twin(f) {
                    Some(c) => code += &c,
                    None => failed.push(name(f)),
                }
            }
            if failed.is_empty() {
                self.items += &code;
                return;
            }
            self.num_fns.retain(|(js, _, _)| !failed.contains(js));
        }
    }

    fn twin(&mut self, f: &Function) -> Option<String> {
        let body = f.body.as_ref()?;
        if !always_returns(&body.statements) {
            return None;
        }
        let (scopes, targets) = (self.scopes.len(), std::mem::take(&mut self.targets));
        self.num_mode = true;
        self.funcs.push(FnCtx { depth: self.scopes.len(), captures: vec![] });
        self.scopes.push(Scope::default());
        let mut params = vec![];
        for p in &f.params.items {
            let BindingPattern::BindingIdentifier(id) = &p.pattern else { unreachable!() };
            params.push(format!("{}: f64", self.declare_num(id.name.as_str())));
        }
        let b = self.num_stmts(&body.statements);
        self.num_mode = false;
        self.funcs.pop();
        self.scopes.truncate(scopes);
        self.targets = targets;
        let (_, twin, _) = self.num_fns.iter().find(|(js, _, _)| js == f.id.as_ref().unwrap().name.as_str())?;
        Some(format!("fn {twin}({}) -> f64 {{\n{}}}\n\n", params.join(", "), b?))
    }

    pub(crate) fn declare_num(&mut self, name: &str) -> String {
        let rust = rust_name("n_", name);
        self.scopes.last_mut().unwrap().vars.push((name.into(), Var::Num(rust.clone())));
        rust
    }

    fn num_stmts(&mut self, list: &[Statement]) -> Option<String> {
        list.iter().map(|s| self.num_stmt(s)).collect()
    }

    fn num_block(&mut self, st: &Statement) -> Option<String> {
        self.scopes.push(Scope::default());
        let r = match st {
            Statement::BlockStatement(b) => self.num_stmts(&b.body),
            st => self.num_stmt(st),
        };
        self.scopes.pop();
        r
    }

    fn num_stmt(&mut self, st: &Statement) -> Option<String> {
        Some(match st {
            Statement::ReturnStatement(r) => format!("return {};\n", self.num(r.argument.as_ref()?)?),
            Statement::IfStatement(i) => {
                let c = self.num_cond(&i.test)?;
                let a = self.num_block(&i.consequent)?;
                match &i.alternate {
                    Some(alt) => format!("if {c} {{\n{a}}} else {{\n{}}}\n", self.num_block(alt)?),
                    None => format!("if {c} {{\n{a}}}\n"),
                }
            }
            Statement::BlockStatement(_) => format!("{{\n{}}}\n", self.num_block(st)?),
            Statement::EmptyStatement(_) => String::new(),
            Statement::VariableDeclaration(d) => self.num_decl(d)?,
            Statement::ExpressionStatement(e) => self.num_effect(&e.expression)? + "\n",
            Statement::WhileStatement(w) => {
                let l = self.label();
                let c = self.num_cond(&w.test)?;
                self.targets.push(Target { label: l.clone(), cont: Some(format!("continue {l}")) });
                let body = self.num_block(&w.body);
                self.targets.pop();
                format!("{l}: while {c} {{\n{}}}\n", body?)
            }
            Statement::ForStatement(f) => {
                self.scopes.push(Scope::default());
                let r = self.num_for(f);
                self.scopes.pop();
                r?
            }
            Statement::BreakStatement(b) if b.label.is_none() => format!("break {};\n", self.targets.last()?.label),
            Statement::ContinueStatement(c) if c.label.is_none() => format!("{};\n", self.targets.iter().rev().find_map(|t| t.cont.clone())?),
            _ => return None,
        })
    }

    fn num_decl(&mut self, d: &VariableDeclaration) -> Option<String> {
        let mut s = String::new();
        for decl in &d.declarations {
            let BindingPattern::BindingIdentifier(id) = &decl.id else { return None };
            let v = self.num(decl.init.as_ref()?)?;
            s += &format!("let mut {}: f64 = {v};\n", self.declare_num(id.name.as_str()));
        }
        Some(s)
    }

    fn num_for(&mut self, f: &ForStatement) -> Option<String> {
        let (l, c) = (self.label(), self.label());
        let init = match &f.init {
            None => String::new(),
            Some(ForStatementInit::VariableDeclaration(d)) => self.num_decl(d)?,
            Some(e) => self.num_effect(e.as_expression()?)?,
        };
        let test = match &f.test {
            Some(t) => self.num_cond(t)?,
            None => "true".into(),
        };
        let update = match &f.update {
            Some(e) => self.num_effect(e)?,
            None => String::new(),
        };
        self.targets.push(Target { label: l.clone(), cont: Some(format!("break {c}")) });
        let body = self.num_block(&f.body);
        self.targets.pop();
        Some(format!("{{\n{init}{l}: loop {{\nif !({test}) {{ break; }}\n{c}: {{\n{}}}\n{update}\n}} }}\n", body?))
    }

    /// An expression run for its effect: `x = ..`, `x += ..`, `x++`.
    fn num_effect(&mut self, e: &Expression) -> Option<String> {
        let (name, rhs) = match e {
            Expression::AssignmentExpression(a) => {
                let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left else { return None };
                let r = self.num(&a.right)?;
                let op = match a.operator {
                    AssignmentOperator::Assign => "",
                    AssignmentOperator::Addition => "+",
                    AssignmentOperator::Subtraction => "-",
                    AssignmentOperator::Multiplication => "*",
                    AssignmentOperator::Division => "/",
                    AssignmentOperator::Remainder => "%",
                    _ => return None,
                };
                (id.name.as_str(), format!("{op}= {r}"))
            }
            Expression::UpdateExpression(u) => {
                let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument else { return None };
                (id.name.as_str(), if u.operator == UpdateOperator::Increment { "+= 1.0".into() } else { "-= 1.0".into() })
            }
            _ => return None,
        };
        match self.lookup(name)? {
            (Var::Num(rust), false) => Some(format!("{rust} {rhs};")),
            _ => None,
        }
    }
}

fn f64_lit(x: f64) -> String {
    if x.is_nan() {
        "f64::NAN".into()
    } else if x.is_infinite() {
        "f64::INFINITY".into()
    } else {
        format!("{x:?}_f64")
    }
}

/// True if every path through `list` ends in `return` (or `throw`).
fn always_returns(list: &[Statement]) -> bool {
    match list.last() {
        Some(Statement::ReturnStatement(r)) => r.argument.is_some(),
        Some(Statement::IfStatement(i)) => {
            let ends = |s: &Statement| match s {
                Statement::BlockStatement(b) => always_returns(&b.body),
                s => always_returns(std::slice::from_ref(s)),
            };
            ends(&i.consequent) && i.alternate.as_ref().is_some_and(ends)
        }
        Some(Statement::BlockStatement(b)) => always_returns(&b.body),
        _ => false,
    }
}
