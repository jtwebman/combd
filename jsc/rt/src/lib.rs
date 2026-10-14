//! combd-jsrt: what JS compiled by jsc runs on.
//!
//! Every JS value is a `Value`. Numbers and booleans live inline, string
//! literals point at static text, and objects, arrays, and functions are
//! reference counted. Objects keep keys in insertion order, like JS.
//!
//! This covers the JS that combd apps use, not all of ECMAScript. Where a
//! corner would cost speed or a big rewrite, we take a shortcut and mark it
//! with `ponytail:`. Where a shortcut would give a silently wrong answer, we
//! throw a clear error instead.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

pub mod alloc;
pub mod b;
mod json;

/// What a JS function returns: a value, or a thrown value.
pub type R = Result<Value, Value>;
pub type Fun = dyn Fn(&Value, &[Value]) -> R;
pub type Arr = Rc<RefCell<Vec<Value>>>;

#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Undef,
    Null,
    Bool(bool),
    Num(f64),
    /// A string from the source: no allocation.
    Lit(&'static str),
    Str(Rc<str>),
    Obj(Rc<Obj>),
    Arr(Arr),
    Fun(Rc<Fun>),
    Re(Rc<Re>),
}

#[derive(Clone)]
pub enum Key {
    Lit(&'static str),
    Rc(Rc<str>),
}

impl Key {
    pub fn as_str(&self) -> &str {
        match self {
            Key::Lit(s) => s,
            Key::Rc(s) => s,
        }
    }
}

/// A plain object. ponytail: no prototypes, getters, or property flags.
#[derive(Default)]
pub struct Obj {
    pub props: RefCell<Vec<(Key, Value)>>,
}

/// Keys from the same source literal share an address, so check that first.
#[inline]
fn key_eq(a: &str, b: &str) -> bool {
    (a.as_ptr() == b.as_ptr() && a.len() == b.len()) || a == b
}

impl Obj {
    pub fn get(&self, k: &str) -> Value {
        let p = self.props.borrow();
        p.iter().find(|(key, _)| key_eq(key.as_str(), k)).map_or(Value::Undef, |(_, v)| v.clone())
    }

    /// `get` with an inline cache: `ic` remembers where this site last found
    /// the key. Objects made by the same code share a layout, so it nearly
    /// always hits.
    #[inline]
    pub fn get_ic(&self, k: &str, ic: &AtomicU32) -> Value {
        let p = self.props.borrow();
        let h = ic.load(Relaxed) as usize;
        if let Some((key, v)) = p.get(h) {
            if key_eq(key.as_str(), k) {
                return v.clone();
            }
        }
        match p.iter().position(|(key, _)| key_eq(key.as_str(), k)) {
            Some(i) => {
                ic.store(i as u32, Relaxed);
                p[i].1.clone()
            }
            None => Value::Undef,
        }
    }

    pub fn put(&self, k: Key, v: Value) {
        let mut p = self.props.borrow_mut();
        match p.iter_mut().find(|(key, _)| key_eq(key.as_str(), k.as_str())) {
            Some(slot) => slot.1 = v,
            None => p.push((k, v)),
        }
    }
}

pub struct Re {
    pub re: regress::Regex,
    pub global: bool,
    pub src: Rc<str>,
}

impl Value {
    pub fn str(s: String) -> Value {
        Value::Str(s.into())
    }
    /// A copy of `s` (one allocation, where `str(s.to_string())` makes two).
    pub fn copy(s: &str) -> Value {
        Value::Str(s.into())
    }
    pub fn fun(f: impl Fn(&Value, &[Value]) -> R + 'static) -> Value {
        Value::Fun(Rc::new(f))
    }
    pub fn arr(v: Vec<Value>) -> Value {
        Value::Arr(Rc::new(RefCell::new(v)))
    }
    pub fn obj(props: Vec<(Key, Value)>) -> Value {
        Value::Obj(Rc::new(Obj { props: RefCell::new(props) }))
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Lit(s) => Some(s),
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Argument `i`, or undefined if the caller passed fewer.
pub fn arg(a: &[Value], i: usize) -> Value {
    a.get(i).cloned().unwrap_or_default()
}

struct SyncUndef(Value);
// Safety: it's always `Undef`, which holds no Rc.
unsafe impl Sync for SyncUndef {}
static UNDEF: SyncUndef = SyncUndef(Value::Undef);

/// Argument `i`, borrowed: no copy for parameters that are only read.
#[inline]
pub fn arg_ref(a: &[Value], i: usize) -> &Value {
    a.get(i).unwrap_or(&UNDEF.0)
}

pub fn error(kind: &str, msg: &str) -> Value {
    Value::obj(vec![(Key::Lit("name"), Value::str(kind.into())), (Key::Lit("message"), Value::str(msg.into()))])
}

pub fn type_error(msg: impl Into<String>) -> Value {
    error("TypeError", &msg.into())
}

/// An exception nothing caught: the cell crashes, and the host answers 500.
pub fn uncaught(e: Value) -> ! {
    let msg = match &e {
        Value::Obj(o) => format!("{}: {}", to_str(&o.get("name")), to_str(&o.get("message"))),
        v => to_str(v),
    };
    panic!("uncaught {msg}")
}

// ---- conversions ----

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Undef | Value::Null => false,
        Value::Bool(b) => *b,
        Value::Num(n) => *n != 0.0 && !n.is_nan(),
        Value::Lit(s) => !s.is_empty(),
        Value::Str(s) => !s.is_empty(),
        _ => true,
    }
}

pub fn to_num(v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        Value::Undef => f64::NAN,
        Value::Null => 0.0,
        Value::Bool(b) => *b as u8 as f64,
        Value::Lit(s) => str_to_num(s),
        Value::Str(s) => str_to_num(s),
        Value::Arr(_) => str_to_num(&to_str(v)),
        _ => f64::NAN,
    }
}

fn str_to_num(s: &str) -> f64 {
    let t = s.trim(); // ponytail: Rust's whitespace set, close to JS's
    if t.is_empty() {
        return 0.0;
    }
    for (p, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(d) = t.strip_prefix(p) {
            // ponytail: exact up to 2^64, which covers real input
            return u64::from_str_radix(d, radix).map_or(f64::NAN, |v| v as f64);
        }
    }
    match t {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        // Rust also takes "inf" and "nan"; JS doesn't.
        _ if t.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) => t.parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

/// JS Number::toString.
pub fn num_to(x: f64, out: &mut String) {
    if x.is_nan() {
        out.push_str("NaN");
    } else if x == 0.0 {
        out.push('0'); // also -0
    } else if x.is_infinite() {
        out.push_str(if x > 0.0 { "Infinity" } else { "-Infinity" });
    } else if x.fract() == 0.0 && x.abs() < 1e15 {
        let mut n = x.abs() as u64;
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        if x < 0.0 {
            out.push('-');
        }
        out.push_str(std::str::from_utf8(&buf[i..]).unwrap());
    } else if x.abs() >= 1e21 || x.abs() < 1e-6 {
        // Both use the shortest digits that round-trip; JS writes "e+21".
        let s = format!("{x:e}");
        match s.split_once('e') {
            Some((m, e)) if !e.starts_with('-') => {
                let _ = write!(out, "{m}e+{e}");
            }
            _ => out.push_str(&s),
        }
    } else {
        let _ = write!(out, "{x}");
    }
}

pub fn write_str(v: &Value, out: &mut String) {
    match v {
        Value::Undef => out.push_str("undefined"),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => num_to(*n, out),
        Value::Lit(s) => out.push_str(s),
        Value::Str(s) => out.push_str(s),
        Value::Arr(a) => {
            for (i, e) in a.borrow().iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !matches!(e, Value::Undef | Value::Null) {
                    write_str(e, out);
                }
            }
        }
        Value::Obj(_) => out.push_str("[object Object]"),
        Value::Fun(_) => out.push_str("function () { [native code] }"),
        Value::Re(r) => {
            let _ = write!(out, "/{}/{}", r.src, if r.global { "g" } else { "" });
        }
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&to_str(self))
    }
}

pub fn to_str(v: &Value) -> String {
    let mut s = String::new();
    write_str(v, &mut s);
    s
}

/// ToString, but strings stay as they are (no copy).
pub fn to_string(v: Value) -> Value {
    match v {
        Value::Lit(_) | Value::Str(_) => v,
        v => Value::str(to_str(&v)),
    }
}

pub fn to_key(v: &Value) -> Key {
    match v {
        Value::Lit(s) => Key::Lit(s),
        Value::Str(s) => Key::Rc(s.clone()),
        v => Key::Rc(to_str(v).into()),
    }
}

/// ToIntegerOrInfinity, clamped into 0..=len the way slice() and friends do.
fn rel_index(v: &Value, len: usize, default: usize) -> usize {
    if matches!(v, Value::Undef) {
        return default;
    }
    let n = to_num(v);
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    if n < 0.0 {
        (len as f64 + n).max(0.0) as usize
    } else {
        n.min(len as f64) as usize
    }
}

// ---- strings: JS indexes UTF-16 units; we keep UTF-8 and convert when needed ----

fn u16_len(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.encode_utf16().count()
    }
}

/// The byte offset of UTF-16 index `i`. ponytail: an index inside a surrogate
/// pair rounds up to the next character.
fn byte_of(s: &str, i: usize) -> usize {
    if s.is_ascii() {
        return i.min(s.len());
    }
    let mut units = 0;
    for (b, c) in s.char_indices() {
        if units >= i {
            return b;
        }
        units += c.len_utf16();
    }
    s.len()
}

fn u16_of(s: &str, byte: usize) -> usize {
    if s.is_ascii() {
        byte
    } else {
        s[..byte].encode_utf16().count()
    }
}

// ---- operators ----

#[inline]
pub fn add(a: &Value, b: &Value) -> Value {
    if let (Value::Num(x), Value::Num(y)) = (a, b) {
        return Value::Num(x + y);
    }
    add_slow(a, b)
}

fn add_slow(a: &Value, b: &Value) -> Value {
    let prim = |v: &Value| matches!(v, Value::Lit(_) | Value::Str(_) | Value::Obj(_) | Value::Arr(_) | Value::Fun(_) | Value::Re(_));
    if prim(a) || prim(b) {
        concat(&[a.clone(), b.clone()])
    } else {
        Value::Num(to_num(a) + to_num(b))
    }
}

macro_rules! arith {
    ($($name:ident $op:tt),*) => {$(
        #[inline]
        pub fn $name(a: &Value, b: &Value) -> Value {
            if let (Value::Num(x), Value::Num(y)) = (a, b) {
                return Value::Num(x $op y);
            }
            Value::Num(to_num(a) $op to_num(b))
        }
    )*};
}
arith!(sub -, mul *, div /, rem %);

pub fn neg(a: &Value) -> Value {
    Value::Num(-to_num(a))
}

pub fn pos(a: &Value) -> Value {
    Value::Num(to_num(a))
}

/// `===`
pub fn seq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undef, Value::Undef) | (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Num(x), Value::Num(y)) => x == y,
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        (Value::Arr(x), Value::Arr(y)) => Rc::ptr_eq(x, y),
        (Value::Fun(x), Value::Fun(y)) => Rc::ptr_eq(x, y),
        (Value::Re(x), Value::Re(y)) => Rc::ptr_eq(x, y),
        _ => match (a.as_str(), b.as_str()) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// `<`, `<=`, `>`, `>=`: numbers, or strings when both sides are strings.
/// ponytail: strings compare by UTF-8 bytes, which only differs from JS's
/// UTF-16 order for characters above U+FFFF.
macro_rules! compare {
    ($($name:ident $op:tt),*) => {$(
        #[inline]
        pub fn $name(a: &Value, b: &Value) -> bool {
            if let (Value::Num(x), Value::Num(y)) = (a, b) {
                return x $op y;
            }
            match (a.as_str(), b.as_str()) {
                (Some(x), Some(y)) => x $op y,
                _ => to_num(a) $op to_num(b),
            }
        }
    )*};
}
compare!(lt <, le <=, gt >, ge >=);

/// Math.min and Math.max on two numbers: NaN wins, and -0 is below 0.
pub fn fmin(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}

pub fn fmax(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_positive() { a } else { b }
    } else {
        a.max(b)
    }
}

/// Math.round: halves go up, toward +Infinity.
pub fn fround(x: f64) -> f64 {
    if x.fract() == -0.5 {
        x.ceil()
    } else {
        x.round()
    }
}

pub fn type_of(v: &Value) -> Value {
    Value::Lit(match v {
        Value::Undef => "undefined",
        Value::Bool(_) => "boolean",
        Value::Num(_) => "number",
        Value::Lit(_) | Value::Str(_) => "string",
        Value::Fun(_) => "function",
        _ => "object",
    })
}

/// Template literals.
pub fn concat(parts: &[Value]) -> Value {
    let mut s = String::with_capacity(parts.iter().map(|p| p.as_str().map_or(8, str::len)).sum());
    for p in parts {
        write_str(p, &mut s);
    }
    Value::str(s)
}

// ---- properties ----

fn not_object(o: &Value, k: &str) -> Value {
    type_error(format!("Cannot read properties of {} (reading '{k}')", to_str(o)))
}

/// `o.k`
pub fn get(o: &Value, k: &str) -> R {
    Ok(match o {
        Value::Obj(ob) => ob.get(k),
        Value::Arr(a) if k == "length" => Value::Num(a.borrow().len() as f64),
        Value::Arr(a) => match k.parse::<usize>() {
            Ok(i) => a.borrow().get(i).cloned().unwrap_or_default(),
            Err(_) => Value::Undef,
        },
        Value::Lit(_) | Value::Str(_) if k == "length" => Value::Num(u16_len(o.as_str().unwrap()) as f64),
        Value::Re(r) if k == "source" => Value::Str(r.src.clone()),
        Value::Re(r) if k == "global" => Value::Bool(r.global),
        Value::Undef | Value::Null => return Err(not_object(o, k)),
        _ => Value::Undef,
    })
}

/// `o.k` at a site with an inline cache.
#[inline]
pub fn get_ic(o: &Value, k: &str, ic: &AtomicU32) -> R {
    match o {
        Value::Obj(ob) => Ok(ob.get_ic(k, ic)),
        _ => get(o, k),
    }
}

/// `o[k]`
pub fn index(o: &Value, k: &Value) -> R {
    if let (Value::Arr(a), Value::Num(n)) = (o, k) {
        let i = *n as usize;
        return Ok(if i as f64 == *n { a.borrow().get(i).cloned().unwrap_or_default() } else { Value::Undef });
    }
    if let (Some(s), Value::Num(n)) = (o.as_str(), k) {
        let i = *n as usize;
        if i as f64 != *n || i >= u16_len(s) {
            return Ok(Value::Undef);
        }
        let b = byte_of(s, i);
        let c = s[b..].chars().next().unwrap();
        return Ok(Value::copy(c.encode_utf8(&mut [0; 4]))); // ponytail: a whole char, not half a surrogate pair
    }
    get(o, to_key(k).as_str())
}

/// `o.k = v`
pub fn set(o: &Value, k: Key, v: Value) -> R {
    match o {
        Value::Obj(ob) => ob.put(k, v),
        Value::Arr(_) => match k.as_str().parse::<f64>() {
            Ok(n) => return set_index(o, &Value::Num(n), v),
            Err(_) => return Err(type_error(format!("jsc: can't set '{}' on an array yet", k.as_str()))),
        },
        Value::Undef | Value::Null => {
            return Err(type_error(format!("Cannot set properties of {} (setting '{}')", to_str(o), k.as_str())))
        }
        _ => {} // primitives ignore writes
    }
    Ok(Value::Undef)
}

/// `o[k] = v`
pub fn set_index(o: &Value, k: &Value, v: Value) -> R {
    if let (Value::Arr(a), Value::Num(n)) = (o, k) {
        let i = *n as usize;
        if i as f64 == *n && i < u32::MAX as usize {
            let mut a = a.borrow_mut();
            if i >= a.len() {
                a.resize(i + 1, Value::Undef); // ponytail: holes are undefined
            }
            a[i] = v;
            return Ok(Value::Undef);
        }
    }
    set(o, to_key(k), v)
}

/// `{ ...v }` into `o`
pub fn spread(o: &Obj, v: &Value) {
    match v {
        Value::Obj(src) if o.props.borrow().is_empty() => *o.props.borrow_mut() = src.props.borrow().clone(),
        Value::Obj(src) => {
            for (k, val) in src.props.borrow().iter() {
                o.put(k.clone(), val.clone());
            }
        }
        Value::Arr(a) => {
            for (i, val) in a.borrow().iter().enumerate() {
                o.put(Key::Rc(i.to_string().into()), val.clone());
            }
        }
        _ => {} // ponytail: spreading a string by characters isn't supported
    }
}

/// Element `i` for array destructuring. ponytail: arrays and strings, not iterators.
pub fn at(o: &Value, i: usize) -> R {
    match o {
        Value::Arr(a) => Ok(a.borrow().get(i).cloned().unwrap_or_default()),
        _ if o.as_str().is_some() => index(o, &Value::Num(i as f64)),
        _ => Err(type_error(format!("{} is not iterable", to_str(o)))),
    }
}

/// `...rest` in array destructuring.
pub fn rest(o: &Value, i: usize) -> R {
    match o {
        Value::Arr(a) => Ok(Value::arr(a.borrow().get(i..).map_or(vec![], |s| s.to_vec()))),
        _ => Err(type_error(format!("jsc: ...rest from {} isn't supported yet", to_str(o)))),
    }
}

/// The array `for…of` walks. It sees pushes made during the loop, like JS.
pub fn iter(o: &Value) -> Result<Arr, Value> {
    match o {
        Value::Arr(a) => Ok(a.clone()),
        _ => Err(type_error(format!("jsc: for…of over {} isn't supported yet", to_str(o)))),
    }
}

pub fn next(a: &Arr, i: &mut usize) -> Option<Value> {
    let v = a.borrow().get(*i).cloned();
    *i += 1;
    v
}

// ---- calls ----

pub fn call(f: &Value, this: &Value, args: &[Value]) -> R {
    match f {
        Value::Fun(f) => f(this, args),
        v => Err(type_error(format!("{} is not a function", to_str(v)))),
    }
}

/// `o.name(args)`: built-in methods first, then the object's own functions.
pub fn call_method(o: &Value, name: &str, args: &[Value]) -> R {
    match o {
        Value::Arr(a) => b::array_method(a, o, name, args),
        Value::Lit(_) | Value::Str(_) => b::string_method(o, o.as_str().unwrap(), name, args),
        Value::Re(r) => b::regexp_method(r, name, args),
        Value::Obj(ob) => match ob.get(name) {
            Value::Fun(f) => f(o, args),
            _ => Err(type_error(format!("o.{name} is not a function"))),
        },
        Value::Undef | Value::Null => Err(not_object(o, name)),
        _ => Err(type_error(format!("jsc: {}.{name}() isn't supported yet", to_str(&type_of(o))))),
    }
}

// ---- globals ----

thread_local! {
    static GLOBAL_THIS: Rc<Obj> = Rc::default();
}

/// A top-level JS binding.
pub type Global = std::thread::LocalKey<RefCell<Value>>;

pub fn gget(g: &'static Global) -> Value {
    g.with(|v| v.borrow().clone())
}

pub fn gset(g: &'static Global, v: Value) {
    g.with(|s| *s.borrow_mut() = v)
}

pub fn global_this() -> Value {
    GLOBAL_THIS.with(|g| Value::Obj(g.clone()))
}

// ---- the cell ABI: alloc, handle, release ----

pub mod cell {
    use super::*;

    pub fn alloc(len: u32) -> *mut u8 {
        let mut v = Vec::<u8>::with_capacity(len as usize);
        let p = v.as_mut_ptr();
        std::mem::forget(v);
        p
    }

    /// # Safety
    /// `ptr, len` must be an answer that `handle` returned.
    pub unsafe fn release(ptr: *mut u8, len: u32) {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len as usize)));
    }

    /// Calls `globalThis.handle(req)`.
    pub fn answer(req: &str) -> Value {
        let h = GLOBAL_THIS.with(|g| g.get("handle"));
        match call(&h, &Value::Undef, &[Value::copy(req)]) {
            Ok(v) => v,
            Err(e) => uncaught(e),
        }
    }

    pub fn run(req: &str) -> String {
        to_str(&answer(req))
    }

    /// The Wasm entry: `globalThis.handle(req)`, returned the answer as (ptr << 32) | len.
    ///
    /// # Safety
    /// `ptr, len` must be a buffer from `alloc`, filled by the host.
    pub unsafe fn handle(ptr: *mut u8, len: u32) -> u64 {
        let req = Vec::from_raw_parts(ptr, len as usize, len as usize);
        let v = answer(&String::from_utf8_lossy(&req));
        let out: Box<[u8]> = match v.as_str() {
            Some(s) => s.as_bytes().into(),
            None => to_str(&v).into_bytes().into(),
        };
        let out = Box::into_raw(out);
        ((out as *mut u8 as u64) << 32) | out.len() as u64
    }
}

/// The exports every compiled JS cell has. `$init` runs the top-level code.
#[macro_export]
macro_rules! cell {
    ($init:path) => {
        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static ALLOC: $crate::alloc::Pool = $crate::alloc::Pool::new();

        #[no_mangle]
        pub extern "C" fn _initialize() {
            if let Err(e) = $init() {
                $crate::uncaught(e)
            }
        }
        #[no_mangle]
        pub extern "C" fn alloc(len: u32) -> *mut u8 {
            $crate::cell::alloc(len)
        }
        #[no_mangle]
        pub extern "C" fn release(ptr: *mut u8, len: u32) {
            unsafe { $crate::cell::release(ptr, len) }
        }
        #[no_mangle]
        pub extern "C" fn handle(ptr: *mut u8, len: u32) -> u64 {
            unsafe { $crate::cell::handle(ptr, len) }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_js() {
        let p = |x: f64| {
            let mut s = String::new();
            num_to(x, &mut s);
            s
        };
        assert_eq!(p(42.0), "42");
        assert_eq!(p(-0.0), "0");
        assert_eq!(p(437.5), "437.5");
        assert_eq!(p(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(p(1e21), "1e+21");
        assert_eq!(p(1.5e-7), "1.5e-7");
        assert_eq!(p(123456789012345680000.0), "123456789012345680000");
        assert_eq!(p(f64::NAN), "NaN");
        assert_eq!(str_to_num(" 42 "), 42.0);
        assert_eq!(str_to_num(""), 0.0);
        assert!(str_to_num("inf").is_nan());
        assert_eq!(str_to_num("0x10"), 16.0);
    }

    #[test]
    fn strings_index_by_utf16() {
        let s = Value::Lit("héllo😀x");
        assert_eq!(to_num(&get(&s, "length").unwrap()), 8.0);
        let r = b::string_method(&s, s.as_str().unwrap(), "slice", &[Value::Num(1.0), Value::Num(2.0)]).unwrap();
        assert_eq!(r.as_str(), Some("é"));
        let r = b::string_method(&s, s.as_str().unwrap(), "indexOf", &[Value::Lit("x")]).unwrap();
        assert_eq!(to_num(&r), 7.0);
    }

    #[test]
    fn json_round_trips() {
        let src = r#"{"a":1,"b":[true,null,"x\né😀"],"c":{"d":-1.5e3},"a":2}"#;
        let v = json::parse(src).unwrap();
        assert_eq!(to_str(&json::stringify(&v)), r#"{"a":2,"b":[true,null,"x\né😀"],"c":{"d":-1500}}"#);
        assert!(json::parse("{bad").is_err());
    }
}
