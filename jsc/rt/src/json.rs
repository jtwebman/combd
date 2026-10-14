//! JSON.parse and JSON.stringify, with JS's output byte for byte.
//! ponytail: no reviver, replacer, indent, or toJSON yet.

use super::*;

pub fn parse(s: &str) -> R {
    let mut p = P { src: s, s: s.as_bytes(), i: 0, keys: vec![] };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.err());
    }
    Ok(v)
}

struct P<'a> {
    src: &'a str,
    s: &'a [u8],
    i: usize,
    /// Keys seen so far: arrays of objects repeat the same few keys.
    keys: Vec<Rc<str>>,
}

impl P<'_> {
    fn err(&self) -> Value {
        error("SyntaxError", &format!("Unexpected token in JSON at position {}", self.i))
    }
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> Result<(), Value> {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err())
        }
    }
    fn word(&mut self, w: &str, v: Value) -> R {
        if self.s[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(self.err())
        }
    }
    fn value(&mut self) -> R {
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let o = Obj::default();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        if self.s.get(self.i) != Some(&b'"') {
                            return Err(self.err());
                        }
                        let k = self.key()?;
                        self.eat(b':')?;
                        let v = self.value()?;
                        o.put(Key::Rc(k), v);
                        self.ws();
                        match self.s.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b'}') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err(self.err()),
                        }
                    }
                }
                Ok(Value::Obj(Rc::new(o)))
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = vec![];
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                } else {
                    loop {
                        a.push(self.value()?);
                        self.ws();
                        match self.s.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b']') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err(self.err()),
                        }
                    }
                }
                Ok(Value::arr(a))
            }
            Some(b'"') => Ok(Value::Str(self.rc_string()?)),
            Some(b't') => self.word("true", Value::Bool(true)),
            Some(b'f') => self.word("false", Value::Bool(false)),
            Some(b'n') => self.word("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err()),
        }
    }
    fn number(&mut self) -> R {
        let start = self.i;
        let digits = |p: &mut Self| {
            let d = p.i;
            while p.s.get(p.i).is_some_and(u8::is_ascii_digit) {
                p.i += 1;
            }
            p.i > d
        };
        if self.s[self.i] == b'-' {
            self.i += 1;
        }
        // JSON forbids leading zeros like 01.
        if self.s.get(self.i) == Some(&b'0') {
            self.i += 1;
        } else if !digits(self) {
            return Err(self.err());
        }
        if self.s.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !digits(self) {
                return Err(self.err());
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return Err(self.err());
            }
        }
        let t = &self.src[start..self.i];
        Ok(Value::Num(fast_decimal(t).unwrap_or_else(|| t.parse().unwrap())))
    }
    fn hex4(&mut self) -> Result<u32, Value> {
        let h = self.s.get(self.i..self.i + 4).and_then(|h| std::str::from_utf8(h).ok());
        let v = h.and_then(|h| u32::from_str_radix(h, 16).ok()).ok_or_else(|| self.err())?;
        self.i += 4;
        Ok(v)
    }
    /// The end of a string with no escapes, if this one is that simple.
    fn plain_end(&self) -> Option<usize> {
        let rest = &self.s[self.i + 1..];
        let j = rest.iter().position(|&c| matches!(c, b'"' | b'\\' | 0..=0x1f))?;
        (rest[j] == b'"').then_some(self.i + 1 + j)
    }

    fn rc_string(&mut self) -> Result<Rc<str>, Value> {
        if let Some(end) = self.plain_end() {
            let r = Rc::from(&self.src[self.i + 1..end]);
            self.i = end + 1;
            return Ok(r);
        }
        Ok(self.string()?.into())
    }

    fn key(&mut self) -> Result<Rc<str>, Value> {
        if let Some(end) = self.plain_end() {
            let k = &self.src[self.i + 1..end];
            self.i = end + 1;
            if let Some(hit) = self.keys.iter().find(|c| &***c == k) {
                return Ok(hit.clone());
            }
            let rc: Rc<str> = Rc::from(k);
            if self.keys.len() < 16 {
                self.keys.push(rc.clone());
            }
            return Ok(rc);
        }
        Ok(self.string()?.into())
    }

    fn string(&mut self) -> Result<String, Value> {
        self.i += 1; // the opening quote
        let mut out = String::new();
        loop {
            // Copy the plain run in one go.
            let run = self.i;
            while self.i < self.s.len() && !matches!(self.s[self.i], b'"' | b'\\' | 0..=0x1f) {
                self.i += 1;
            }
            // Runs start and end at ASCII bytes, so they're whole characters.
            out.push_str(&self.src[run..self.i]);
            match self.s.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self.s.get(self.i).ok_or_else(|| self.err())?;
                    self.i += 1;
                    out.push(match c {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xd800..0xdc00).contains(&hi) && self.s[self.i..].starts_with(b"\\u") {
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xdc00..0xe000).contains(&lo) {
                                    0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                                } else {
                                    return Err(self.err()); // ponytail: lone surrogates aren't kept
                                }
                            } else {
                                hi
                            };
                            char::from_u32(cp).ok_or_else(|| self.err())?
                        }
                        _ => return Err(self.err()),
                    });
                }
                _ => return Err(self.err()), // end of input or a raw control character
            }
        }
    }
}

/// `123` or `-4.25`: a whole number of at most 15 digits divided by a power of
/// ten is exact in f64, so one division rounds correctly (Clinger's fast path).
fn fast_decimal(t: &str) -> Option<f64> {
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if int.len() + frac.len() > 15 || frac.contains(['e', 'E']) || int.contains(['e', 'E']) {
        return None;
    }
    const POW10: [f64; 16] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15];
    let m = int.bytes().chain(frac.bytes()).fold(0u64, |m, b| m * 10 + (b - b'0') as u64);
    let x = m as f64 / POW10[frac.len()];
    Some(if neg { -x } else { x })
}

/// JSON.stringify(v): undefined when v has no JSON form, like JS.
pub fn stringify(v: &Value) -> Value {
    let mut out = String::with_capacity(256);
    if write(v, &mut out) {
        Value::str(out)
    } else {
        Value::Undef
    }
}

/// Writes `v`; false if it has no JSON form (undefined or a function).
fn write(v: &Value, out: &mut String) -> bool {
    match v {
        Value::Undef | Value::Fun(_) => return false,
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) if n.is_finite() => num_to(*n, out),
        Value::Num(_) => out.push_str("null"),
        Value::Lit(s) => quote(s, out),
        Value::Str(s) => quote(s, out),
        Value::Re(_) => out.push_str("{}"),
        Value::Arr(a) => {
            out.push('[');
            for (i, e) in a.borrow().iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !write(e, out) {
                    out.push_str("null");
                }
            }
            out.push(']');
        }
        Value::Obj(o) => {
            out.push('{');
            let mut first = true;
            for (k, e) in o.props.borrow().iter() {
                if matches!(e, Value::Undef | Value::Fun(_)) {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                quote(k.as_str(), out);
                out.push(':');
                write(e, out);
            }
            out.push('}');
        }
    }
    true
}

/// Any byte below 0x20, or a quote or backslash? Eight bytes at a time.
fn needs_escape(b: &[u8]) -> bool {
    const L: u64 = 0x0101_0101_0101_0101;
    const H: u64 = 0x8080_8080_8080_8080;
    let zero = |x: u64| x.wrapping_sub(L) & !x & H;
    let mut words = b.chunks_exact(8);
    for w in &mut words {
        let x = u64::from_le_bytes(w.try_into().unwrap());
        // A byte below 0x20 (exact for bytes under 0x80; others can't match anyway).
        let low = x.wrapping_sub(L * 0x20) & !x & H;
        if low | zero(x ^ (L * b'"' as u64)) | zero(x ^ (L * b'\\' as u64)) != 0 {
            return true;
        }
    }
    words.remainder().iter().any(|&c| c < 0x20 || c == b'"' || c == b'\\')
}

fn quote(s: &str, out: &mut String) {
    out.push('"');
    let b = s.as_bytes();
    if !needs_escape(b) {
        out.push_str(s);
        out.push('"');
        return;
    }
    let mut run = 0;
    for (i, &c) in b.iter().enumerate() {
        let esc = match c {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            8 => "\\b",
            12 => "\\f",
            0..=0x1f => "",
            _ => continue,
        };
        out.push_str(&s[run..i]);
        if esc.is_empty() {
            let _ = write!(out, "\\u{c:04x}");
        } else {
            out.push_str(esc);
        }
        run = i + 1;
    }
    out.push_str(&s[run..]);
    out.push('"');
}
