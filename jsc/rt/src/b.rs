//! Built-ins: the global functions and the methods on arrays, strings, and regexes.
//! Only what combd apps have needed so far; the rest throws a clear error.

use super::*;

fn unsupported(what: &str) -> Value {
    type_error(format!("jsc: {what} isn't supported yet"))
}

// ---- globals ----

pub fn number(a: &[Value]) -> R {
    Ok(Value::Num(if a.is_empty() { 0.0 } else { to_num(&a[0]) }))
}

pub fn string(a: &[Value]) -> R {
    Ok(if a.is_empty() { Value::Lit("") } else { to_string(a[0].clone()) })
}

pub fn math_round(a: &[Value]) -> R {
    // JS rounds halves up, toward +Infinity.
    Ok(Value::Num(fround(to_num(&arg(a, 0)))))
}

pub fn math_floor(a: &[Value]) -> R {
    Ok(Value::Num(to_num(&arg(a, 0)).floor()))
}

pub fn math_min(a: &[Value]) -> R {
    let mut m = f64::INFINITY;
    for v in a {
        let x = to_num(v);
        if x.is_nan() {
            return Ok(Value::Num(f64::NAN));
        }
        m = m.min(x);
    }
    Ok(Value::Num(m))
}

pub fn math_max(a: &[Value]) -> R {
    let mut m = f64::NEG_INFINITY;
    for v in a {
        let x = to_num(v);
        if x.is_nan() {
            return Ok(Value::Num(f64::NAN));
        }
        m = m.max(x);
    }
    Ok(Value::Num(m))
}

pub fn json_parse(a: &[Value]) -> R {
    json::parse(&to_str(&arg(a, 0)))
}

pub fn json_stringify(a: &[Value]) -> R {
    if a.len() > 1 && !matches!(a[1], Value::Undef | Value::Null) {
        return Err(unsupported("JSON.stringify with a replacer or indent"));
    }
    Ok(json::stringify(&arg(a, 0)))
}

/// Array.from(arrayLike or array, mapFn?)
pub fn array_from(a: &[Value]) -> R {
    let src = arg(a, 0);
    let f = arg(a, 1);
    let items: Vec<Value> = match &src {
        Value::Arr(x) => x.borrow().clone(),
        Value::Obj(o) => {
            let n = to_num(&o.get("length"));
            let n = if n.is_nan() || n < 0.0 { 0 } else { n as usize };
            (0..n).map(|i| o.get(&i.to_string())).collect()
        }
        v if v.as_str().is_some() => v.as_str().unwrap().chars().map(|c| Value::str(c.to_string())).collect(),
        _ => vec![],
    };
    if matches!(f, Value::Undef) {
        return Ok(Value::arr(items));
    }
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.into_iter().enumerate() {
        out.push(call(&f, &Value::Undef, &[v, Value::Num(i as f64)])?);
    }
    Ok(Value::arr(out))
}

/// ponytail: values print with String(), not Node's inspect format.
pub fn console_log(a: &[Value]) -> R {
    let mut s = String::new();
    for (i, v) in a.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        write_str(v, &mut s);
    }
    s.push('\n');
    write_stdout(s.as_bytes());
    Ok(Value::Undef)
}

#[cfg(target_arch = "wasm32")]
fn write_stdout(b: &[u8]) {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn fd_write(fd: i32, iovs: *const [u32; 2], n: i32, written: *mut u32) -> i32;
    }
    let iov = [b.as_ptr() as u32, b.len() as u32];
    let mut n = 0;
    unsafe { fd_write(1, &iov, 1, &mut n) };
}

#[cfg(not(target_arch = "wasm32"))]
fn write_stdout(b: &[u8]) {
    use std::io::Write;
    let _ = std::io::stdout().write_all(b);
}

/// combd.counter(): a host call that counts up per cell.
pub fn combd_counter(_: &[Value]) -> R {
    #[cfg(target_arch = "wasm32")]
    {
        #[link(wasm_import_module = "combd")]
        extern "C" {
            fn counter_incr() -> i64;
        }
        Ok(Value::Num(unsafe { counter_incr() } as f64))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        thread_local!(static N: std::cell::Cell<i64> = const { std::cell::Cell::new(0) });
        Ok(Value::Num(N.with(|n| {
            n.set(n.get() + 1);
            n.get()
        }) as f64))
    }
}

pub fn new_error(a: &[Value]) -> R {
    Ok(error("Error", &to_str(&arg(a, 0))))
}

pub fn new_regexp(a: &[Value]) -> R {
    let flags = match arg(a, 1) {
        Value::Undef => String::new(),
        f => to_str(&f),
    };
    regexp(&to_str(&arg(a, 0)), &flags)
}

/// A regex literal or `new RegExp`. ponytail: `lastIndex` isn't tracked, so
/// exec() on a /g regex always starts at 0.
pub fn regexp(src: &str, flags: &str) -> R {
    if flags.contains('y') || flags.contains('d') {
        return Err(unsupported("the y and d regex flags"));
    }
    let global = flags.contains('g');
    let rflags: String = flags.chars().filter(|&c| c != 'g').collect();
    match regress::Regex::with_flags(src, rflags.as_str()) {
        Ok(re) => Ok(Value::Re(Rc::new(Re { re, global, src: src.into() }))),
        Err(e) => Err(error("SyntaxError", &format!("Invalid regular expression: /{src}/: {e}"))),
    }
}

// ---- array methods ----

pub fn array_method(a: &Arr, o: &Value, name: &str, args: &[Value]) -> R {
    // Callbacks may touch the array, so never hold a borrow across one.
    let item = |i: usize| a.borrow().get(i).cloned();
    match name {
        "push" => {
            let mut v = a.borrow_mut();
            v.extend_from_slice(args);
            Ok(Value::Num(v.len() as f64))
        }
        "slice" => {
            let v = a.borrow();
            let s = rel_index(&arg(args, 0), v.len(), 0);
            let e = rel_index(&arg(args, 1), v.len(), v.len());
            Ok(Value::arr(if s < e { v[s..e].to_vec() } else { vec![] }))
        }
        "map" | "forEach" => {
            let f = arg(args, 0);
            let n = a.borrow().len();
            let mut out = Vec::with_capacity(if name == "map" { n } else { 0 });
            let mut i = 0;
            while let Some(v) = item(i).filter(|_| i < n) {
                let r = call(&f, &Value::Undef, &[v, Value::Num(i as f64), o.clone()])?;
                if name == "map" {
                    out.push(r);
                }
                i += 1;
            }
            Ok(if name == "map" { Value::arr(out) } else { Value::Undef })
        }
        "reduce" => {
            let f = arg(args, 0);
            let n = a.borrow().len();
            let (mut acc, mut i) = match args.get(1) {
                Some(init) => (init.clone(), 0),
                None => match item(0) {
                    Some(v) => (v, 1),
                    None => return Err(type_error("Reduce of empty array with no initial value")),
                },
            };
            while let Some(v) = item(i).filter(|_| i < n) {
                acc = call(&f, &Value::Undef, &[acc, v, Value::Num(i as f64), o.clone()])?;
                i += 1;
            }
            Ok(acc)
        }
        "join" => {
            let sep = match arg(args, 0) {
                Value::Undef => ",".to_string(),
                s => to_str(&s),
            };
            let mut s = String::new();
            for (i, e) in a.borrow().iter().enumerate() {
                if i > 0 {
                    s.push_str(&sep);
                }
                if !matches!(e, Value::Undef | Value::Null) {
                    write_str(e, &mut s);
                }
            }
            Ok(Value::str(s))
        }
        _ => Err(unsupported(&format!("Array.prototype.{name}"))),
    }
}

// ---- string methods ----

pub fn string_method(o: &Value, s: &str, name: &str, args: &[Value]) -> R {
    match name {
        "indexOf" => {
            let needle = to_str(&arg(args, 0));
            let from = byte_of(s, rel_index(&arg(args, 1), u16_len(s), 0));
            let hit = match needle.as_bytes() {
                &[c] => s.as_bytes()[from..].iter().position(|&b| b == c),
                _ => s[from..].find(&needle),
            };
            Ok(Value::Num(hit.map_or(-1.0, |b| u16_of(s, from + b) as f64)))
        }
        "slice" => {
            let n = u16_len(s);
            let st = rel_index(&arg(args, 0), n, 0);
            let e = rel_index(&arg(args, 1), n, n);
            if st >= e {
                return Ok(Value::Lit(""));
            }
            Ok(Value::copy(&s[byte_of(s, st)..byte_of(s, e)]))
        }
        "split" => {
            let sep = arg(args, 0);
            if args.len() > 1 {
                return Err(unsupported("split with a limit"));
            }
            let parts: Vec<Value> = match &sep {
                Value::Undef => vec![o.clone()],
                Value::Re(_) => return Err(unsupported("split with a regex")),
                sep => {
                    let sep = to_str(sep);
                    if sep.is_empty() {
                        s.chars().map(|c| Value::str(c.to_string())).collect() // ponytail: chars, not UTF-16 units
                    } else if sep.len() == 1 {
                        s.split(sep.as_bytes()[0] as char).map(Value::copy).collect()
                    } else {
                        s.split(sep.as_str()).map(Value::copy).collect()
                    }
                }
            };
            Ok(Value::arr(parts))
        }
        "toUpperCase" => Ok(Value::str(s.to_uppercase())),
        "toLowerCase" => Ok(Value::str(s.to_lowercase())),
        "replace" => replace(s, &arg(args, 0), &arg(args, 1)),
        _ => Err(unsupported(&format!("String.prototype.{name}"))),
    }
}

fn replace(s: &str, pat: &Value, with: &Value) -> R {
    if let Some(w) = with.as_str() {
        if w.contains('$') {
            return Err(unsupported("$ patterns in replace()"));
        }
    }
    let mut out = String::new();
    let mut last = 0;
    // One match: its byte range and its capture groups.
    let mut sub = |start: usize, end: usize, caps: Vec<Value>, out: &mut String| -> Result<(), Value> {
        out.push_str(&s[last..start]);
        match with {
            Value::Fun(_) => {
                let mut a = vec![Value::copy(&s[start..end])];
                a.extend(caps);
                a.push(Value::Num(u16_of(s, start) as f64));
                a.push(Value::copy(s));
                write_str(&call(with, &Value::Undef, &a)?, out);
            }
            w => write_str(w, out),
        }
        last = end;
        Ok(())
    };
    match pat {
        Value::Re(r) => {
            let caps = |m: &regress::Match| -> Vec<Value> {
                m.captures.iter().map(|c| c.clone().map_or(Value::Undef, |c| Value::copy(&s[c]))).collect()
            };
            if r.global {
                for m in r.re.find_iter(s) {
                    sub(m.start(), m.end(), caps(&m), &mut out)?;
                }
            } else if let Some(m) = r.re.find(s) {
                sub(m.start(), m.end(), caps(&m), &mut out)?;
            }
        }
        p => {
            let p = to_str(p);
            if let Some(b) = s.find(&p) {
                sub(b, b + p.len(), vec![], &mut out)?;
            }
        }
    }
    out.push_str(&s[last..]);
    Ok(Value::str(out))
}

// ---- regex methods ----

pub fn regexp_method(r: &Re, name: &str, args: &[Value]) -> R {
    let s = to_str(&arg(args, 0));
    match name {
        // ponytail: the match array has no `index`, `input`, or `groups`.
        "exec" => Ok(match r.re.find(&s) {
            None => Value::Null,
            Some(m) => {
                let mut v = vec![Value::copy(&s[m.range()])];
                v.extend(m.captures.iter().map(|c| c.clone().map_or(Value::Undef, |c| Value::copy(&s[c]))));
                Value::arr(v)
            }
        }),
        "test" => Ok(Value::Bool(r.re.find(&s).is_some())),
        _ => Err(unsupported(&format!("RegExp.prototype.{name}"))),
    }
}
