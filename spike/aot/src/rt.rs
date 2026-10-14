//! The runtime generated code calls into: the cell's state, traps, and the
//! Wasm semantics that need more than one Rust expression.

pub const PAGE: usize = 65536;
/// Deepest call nesting before a cell traps. Keeps runaway recursion from
/// overflowing the host's native stack.
pub const MAX_DEPTH: u32 = 5_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Trap {
    Unreachable,
    OutOfBounds,
    DivideByZero,
    IntegerOverflow,
    InvalidConversion,
    UndefinedElement,
    UninitializedElement,
    IndirectTypeMismatch,
    CallStackExhausted,
    /// The CPU budget ran out. Compiled cells can't pause mid-call yet.
    OutOfFuel,
    Host(String),
}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trap::Unreachable => write!(f, "unreachable"),
            Trap::Host(e) => write!(f, "{e}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// How compiled code reaches the host. `import` is the module's import index.
pub trait Host {
    fn call(&mut self, import: u32, args: &[u64], mem: &mut [u8]) -> Result<u64, String>;
    fn boxed_clone(&self) -> Box<dyn Host>;
    fn out(&self) -> Vec<u8>;
}

/// Everything a compiled cell owns. Plain data apart from the host handle,
/// so it can be copied whenever no call is running.
pub struct Ctx {
    pub mem: Vec<u8>,
    pub max_pages: u32,
    pub globals: Vec<u64>,
    pub table: Vec<Option<u32>>,
    pub data: &'static [&'static [u8]],
    pub dropped: Vec<bool>,
    pub fuel: i64,
    pub depth: u32,
    pub host: Box<dyn Host>,
}

impl Clone for Ctx {
    fn clone(&self) -> Self {
        Ctx {
            mem: self.mem.clone(),
            max_pages: self.max_pages,
            globals: self.globals.clone(),
            table: self.table.clone(),
            data: self.data,
            dropped: self.dropped.clone(),
            fuel: self.fuel,
            depth: self.depth,
            host: self.host.boxed_clone(),
        }
    }
}

impl Ctx {
    pub fn new(min_pages: u32, max_pages: u32, data: &'static [&'static [u8]], host: Box<dyn Host>) -> Result<Ctx, Trap> {
        if min_pages > max_pages {
            return Err(Trap::OutOfBounds);
        }
        Ok(Ctx {
            mem: vec![0; min_pages as usize * PAGE],
            max_pages,
            globals: vec![],
            table: vec![],
            data,
            dropped: vec![false; data.len()],
            fuel: i64::MAX,
            depth: 0,
            host,
        })
    }

    pub fn init_data(&mut self, seg: usize, offset: u32) -> Result<(), Trap> {
        let d = self.data[seg];
        let to = self.mem.get_mut(offset as usize..offset as usize + d.len()).ok_or(Trap::OutOfBounds)?;
        to.copy_from_slice(d);
        self.dropped[seg] = true;
        Ok(())
    }

    pub fn init_elem(&mut self, offset: u32, funcs: &[u32]) -> Result<(), Trap> {
        let slots = self.table.get_mut(offset as usize..offset as usize + funcs.len()).ok_or(Trap::OutOfBounds)?;
        for (s, f) in slots.iter_mut().zip(funcs) {
            *s = Some(*f);
        }
        Ok(())
    }

    #[inline]
    pub fn enter(&mut self) -> Result<(), Trap> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Trap::CallStackExhausted);
        }
        Ok(())
    }

    #[inline]
    pub fn tick(&mut self) -> Result<(), Trap> {
        self.fuel -= 1;
        if self.fuel < 0 {
            return Err(Trap::OutOfFuel);
        }
        Ok(())
    }

    pub fn import(&mut self, idx: u32, args: &[u64]) -> Result<u64, Trap> {
        self.host.call(idx, args, &mut self.mem).map_err(Trap::Host)
    }

    #[inline]
    pub fn table_get(&self, i: u64) -> Result<u32, Trap> {
        match self.table.get(i as u32 as usize) {
            None => Err(Trap::UndefinedElement),
            Some(None) => Err(Trap::UninitializedElement),
            Some(Some(f)) => Ok(*f),
        }
    }

    pub fn size(&self) -> u64 {
        (self.mem.len() / PAGE) as u64
    }

    pub fn grow(&mut self, n: u64) -> u64 {
        let old = self.mem.len() / PAGE;
        let new = old as u64 + n as u32 as u64;
        if new > self.max_pages as u64 {
            return u32::MAX as u64;
        }
        self.mem.resize(new as usize * PAGE, 0);
        old as u64
    }

    pub fn copy(&mut self, dst: u64, src: u64, n: u64) -> Result<(), Trap> {
        let (d, s, n) = (dst as u32 as usize, src as u32 as usize, n as u32 as usize);
        if s + n > self.mem.len() || d + n > self.mem.len() {
            return Err(Trap::OutOfBounds);
        }
        self.mem.copy_within(s..s + n, d);
        Ok(())
    }

    pub fn fill(&mut self, dst: u64, val: u64, n: u64) -> Result<(), Trap> {
        let (d, n) = (dst as u32 as usize, n as u32 as usize);
        self.mem.get_mut(d..d + n).ok_or(Trap::OutOfBounds)?.fill(val as u8);
        Ok(())
    }

    pub fn mem_init(&mut self, seg: usize, dst: u64, src: u64, n: u64) -> Result<(), Trap> {
        let (d, s, n) = (dst as u32 as usize, src as u32 as usize, n as u32 as usize);
        let data: &[u8] = if self.dropped[seg] { &[] } else { self.data[seg] };
        let from = data.get(s..s + n).ok_or(Trap::OutOfBounds)?;
        self.mem.get_mut(d..d + n).ok_or(Trap::OutOfBounds)?.copy_from_slice(from);
        Ok(())
    }
}

#[inline(always)]
fn ea(addr: u64, off: u32) -> usize {
    addr as u32 as usize + off as usize
}

macro_rules! loads {
    ($($name:ident $n:literal $t:ty;)*) => {$(
        #[inline(always)]
        pub fn $name(m: &[u8], addr: u64, off: u32) -> Result<$t, Trap> {
            let a = ea(addr, off);
            match m.get(a..a + $n) {
                Some(b) => Ok(<$t>::from_le_bytes(b.try_into().unwrap())),
                None => Err(Trap::OutOfBounds),
            }
        }
    )*};
}
loads! { ld1 1 u8; ld2 2 u16; ld4 4 u32; ld8 8 u64; }

macro_rules! stores {
    ($($name:ident $n:literal $t:ty;)*) => {$(
        #[inline(always)]
        pub fn $name(m: &mut [u8], addr: u64, off: u32, v: $t) -> Result<(), Trap> {
            let a = ea(addr, off);
            match m.get_mut(a..a + $n) {
                Some(b) => {
                    b.copy_from_slice(&v.to_le_bytes());
                    Ok(())
                }
                None => Err(Trap::OutOfBounds),
            }
        }
    )*};
}
stores! { st1 1 u8; st2 2 u16; st4 4 u32; st8 8 u64; }

pub fn i32_div_s(a: u64, b: u64) -> Result<u64, Trap> {
    match (a as u32 as i32, b as u32 as i32) {
        (_, 0) => Err(Trap::DivideByZero),
        (i32::MIN, -1) => Err(Trap::IntegerOverflow),
        (x, y) => Ok((x / y) as u32 as u64),
    }
}
pub fn i32_div_u(a: u64, b: u64) -> Result<u64, Trap> {
    (a as u32).checked_div(b as u32).map(|v| v as u64).ok_or(Trap::DivideByZero)
}
pub fn i32_rem_s(a: u64, b: u64) -> Result<u64, Trap> {
    match b as u32 as i32 {
        0 => Err(Trap::DivideByZero),
        y => Ok((a as u32 as i32).wrapping_rem(y) as u32 as u64),
    }
}
pub fn i32_rem_u(a: u64, b: u64) -> Result<u64, Trap> {
    (a as u32).checked_rem(b as u32).map(|v| v as u64).ok_or(Trap::DivideByZero)
}
pub fn i64_div_s(a: u64, b: u64) -> Result<u64, Trap> {
    match (a as i64, b as i64) {
        (_, 0) => Err(Trap::DivideByZero),
        (i64::MIN, -1) => Err(Trap::IntegerOverflow),
        (x, y) => Ok((x / y) as u64),
    }
}
pub fn i64_div_u(a: u64, b: u64) -> Result<u64, Trap> {
    a.checked_div(b).ok_or(Trap::DivideByZero)
}
pub fn i64_rem_s(a: u64, b: u64) -> Result<u64, Trap> {
    match b as i64 {
        0 => Err(Trap::DivideByZero),
        y => Ok((a as i64).wrapping_rem(y) as u64),
    }
}
pub fn i64_rem_u(a: u64, b: u64) -> Result<u64, Trap> {
    a.checked_rem(b).ok_or(Trap::DivideByZero)
}

/// Wasm's trapping float-to-int rule: trap on NaN or when the truncated value
/// falls outside [lo, hi).
#[inline]
pub fn trunc(x: f64, lo: f64, hi: f64) -> Result<f64, Trap> {
    if x.is_nan() {
        return Err(Trap::InvalidConversion);
    }
    let t = x.trunc();
    if t < lo || t >= hi {
        return Err(Trap::IntegerOverflow);
    }
    Ok(t)
}

macro_rules! fminmax {
    ($min:ident, $max:ident, $t:ty) => {
        #[inline]
        pub fn $min(a: $t, b: $t) -> $t {
            if a.is_nan() || b.is_nan() {
                <$t>::NAN
            } else if a == b {
                if a.is_sign_negative() { a } else { b }
            } else {
                a.min(b)
            }
        }
        #[inline]
        pub fn $max(a: $t, b: $t) -> $t {
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
