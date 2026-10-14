//! The runtime generated code calls into: the cell's state, traps, and the
//! Wasm semantics that need more than one Rust expression.

pub const PAGE: usize = 65536;
/// How much native stack a cell's call may use before it traps. Keeps runaway
/// recursion from overflowing the host's stack. Cells must run on threads with
/// at least 2 MiB of stack (the Rust default).
pub const STACK_BUDGET: usize = 1 << 20;

/// One byte, so `Result<u64, Trap>` comes back in two registers. A host
/// error's message waits in `Ctx::host_err`.
#[derive(Debug, Clone, Copy, PartialEq)]
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
    Host,
}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trap::Unreachable => write!(f, "unreachable"),
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

/// Everything a compiled cell owns, except its linear memory. Memory is passed
/// to every compiled function as its own argument, so LLVM can keep its
/// pointer and length in registers.
pub struct Ctx {
    pub max_pages: u32,
    pub globals: Vec<u64>,
    pub table: Vec<Option<u32>>,
    pub data: &'static [&'static [u8]],
    pub dropped: Vec<bool>,
    pub fuel: i64,
    /// Calls trap when the native stack reaches below this address.
    pub stack_floor: usize,
    pub host: Box<dyn Host>,
    pub host_err: Option<String>,
}

impl Clone for Ctx {
    fn clone(&self) -> Self {
        Ctx {
            max_pages: self.max_pages,
            globals: self.globals.clone(),
            table: self.table.clone(),
            data: self.data,
            dropped: self.dropped.clone(),
            fuel: self.fuel,
            stack_floor: self.stack_floor,
            host: self.host.boxed_clone(),
            host_err: None,
        }
    }
}

/// Where the native stack is right now. Taking a local's address is safe.
#[inline(always)]
pub fn stack_here() -> usize {
    let marker = 0u8;
    std::ptr::addr_of!(marker) as usize
}

impl Ctx {
    pub fn new(min_pages: u32, max_pages: u32, data: &'static [&'static [u8]], host: Box<dyn Host>) -> Result<(Ctx, Mem), Trap> {
        if min_pages > max_pages {
            return Err(Trap::OutOfBounds);
        }
        let c = Ctx {
            max_pages,
            globals: vec![],
            table: vec![],
            data,
            dropped: vec![false; data.len()],
            fuel: i64::MAX,
            stack_floor: 0,
            host,
            host_err: None,
        };
        Ok((c, Mem::new(min_pages as usize * PAGE)))
    }

    pub fn init_data(&mut self, m: &mut Mem, seg: usize, offset: u32) -> Result<(), Trap> {
        let d = self.data[seg];
        let to = m.get_mut(offset as usize, d.len()).ok_or(Trap::OutOfBounds)?;
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

    /// Start a call from the host: everything below here is the cell's to use.
    pub fn start(&mut self) {
        self.stack_floor = stack_here().saturating_sub(STACK_BUDGET);
    }

    /// The real native stack check: one compare against `stack_floor`.
    #[inline(always)]
    pub fn enter(&self) -> Result<(), Trap> {
        if stack_here() < self.stack_floor {
            return Err(Trap::CallStackExhausted);
        }
        Ok(())
    }

    #[inline(always)]
    pub fn tick(&mut self) -> Result<(), Trap> {
        self.fuel -= 1;
        if self.fuel < 0 {
            return Err(Trap::OutOfFuel);
        }
        Ok(())
    }

    pub fn import(&mut self, m: &mut Mem, idx: u32, args: &[u64]) -> Result<u64, Trap> {
        self.host.call(idx, args, m.bytes()).map_err(|e| {
            self.host_err = Some(e);
            Trap::Host
        })
    }

    #[inline]
    pub fn table_get(&self, i: u64) -> Result<u32, Trap> {
        match self.table.get(i as u32 as usize) {
            None => Err(Trap::UndefinedElement),
            Some(None) => Err(Trap::UninitializedElement),
            Some(Some(f)) => Ok(*f),
        }
    }

    pub fn mem_init(&mut self, m: &mut Mem, seg: usize, dst: u64, src: u64, n: u64) -> Result<(), Trap> {
        let (d, s, n) = (dst as u32 as usize, src as u32 as usize, n as u32 as usize);
        let data: &[u8] = if self.dropped[seg] { &[] } else { self.data[seg] };
        let from = data.get(s..s + n).ok_or(Trap::OutOfBounds)?;
        m.get_mut(d, n).ok_or(Trap::OutOfBounds)?.copy_from_slice(from);
        Ok(())
    }
}

/// A cell's linear memory, exactly the Wasm memory's size, so every access is
/// one bounds check. Growing writes zeros only into the new part.
pub struct Mem {
    pub(crate) buf: Vec<u8>,
}

const CHUNK: usize = 16 * 1024;

/// A fresh zeroed buffer holding `from`. All-zero chunks are skipped, so a
/// snapshot copy never touches pages the cell left empty.
fn copy_nonzero(from: &[u8]) -> Vec<u8> {
    let mut nb = vec![0u8; from.len()];
    for (i, c) in from.chunks(CHUNK).enumerate() {
        // An OR-fold vectorizes; `any` stops early and doesn't.
        if c.iter().fold(0u8, |acc, &b| acc | b) != 0 {
            nb[i * CHUNK..i * CHUNK + c.len()].copy_from_slice(c);
        }
    }
    nb
}

impl Mem {
    pub fn new(len: usize) -> Mem {
        Mem { buf: vec![0; len] }
    }

    #[inline(always)]
    pub fn bytes(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[inline(always)]
    pub fn get(&self, a: usize, n: usize) -> Option<&[u8]> {
        self.buf.get(a..a + n)
    }

    #[inline(always)]
    pub fn get_mut(&mut self, a: usize, n: usize) -> Option<&mut [u8]> {
        self.buf.get_mut(a..a + n)
    }

    pub fn grow(&mut self, max_pages: u32, n: u64) -> u64 {
        let old = self.buf.len() / PAGE;
        let new = old as u64 + n as u32 as u64;
        if new > max_pages as u64 {
            return u32::MAX as u64;
        }
        self.buf.resize(new as usize * PAGE, 0);
        old as u64
    }
}

impl Clone for Mem {
    fn clone(&self) -> Self {
        Mem { buf: copy_nonzero(&self.buf) }
    }
}

pub fn size(m: &Mem) -> u64 {
    (m.len() / PAGE) as u64
}

pub fn grow(m: &mut Mem, max_pages: u32, n: u64) -> u64 {
    m.grow(max_pages, n)
}

pub fn copy(m: &mut Mem, dst: u64, src: u64, n: u64) -> Result<(), Trap> {
    let (d, s, n) = (dst as u32 as usize, src as u32 as usize, n as u32 as usize);
    if s + n > m.len() || d + n > m.len() {
        return Err(Trap::OutOfBounds);
    }
    m.bytes().copy_within(s..s + n, d);
    Ok(())
}

pub fn fill(m: &mut Mem, dst: u64, val: u64, n: u64) -> Result<(), Trap> {
    let (d, n) = (dst as u32 as usize, n as u32 as usize);
    m.get_mut(d, n).ok_or(Trap::OutOfBounds)?.fill(val as u8);
    Ok(())
}

#[inline(always)]
fn ea(addr: u64, off: u32) -> usize {
    addr as u32 as usize + off as usize
}

macro_rules! loads {
    ($($name:ident $n:literal $t:ty;)*) => {$(
        #[inline(always)]
        pub fn $name(m: &Mem, addr: u64, off: u32) -> Result<$t, Trap> {
            let a = ea(addr, off);
            match m.get(a, $n) {
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
        pub fn $name(m: &mut Mem, addr: u64, off: u32, v: $t) -> Result<(), Trap> {
            let a = ea(addr, off);
            match m.get_mut(a, $n) {
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

/// EXPERIMENT ONLY, NEVER SHIP: loads and stores with no bounds check, to
/// measure what the checks cost. A bad address here is undefined behavior.
#[cfg(feature = "experiment-unchecked")]
pub mod unchecked {
    use super::Mem;
    macro_rules! u {
        ($($ld:ident $st:ident $n:literal $t:ty;)*) => {$(
            #[inline(always)]
            pub fn $ld(m: &Mem, addr: u64, off: u32) -> $t {
                let a = addr as u32 as usize + off as usize;
                unsafe { <$t>::from_le_bytes(*(m.buf.as_ptr().add(a) as *const [u8; $n])) }
            }
            #[inline(always)]
            pub fn $st(m: &mut Mem, addr: u64, off: u32, v: $t) {
                let a = addr as u32 as usize + off as usize;
                unsafe { *(m.buf.as_mut_ptr().add(a) as *mut [u8; $n]) = v.to_le_bytes() }
            }
        )*};
    }
    u! { ld1 st1 1 u8; ld2 st2 2 u16; ld4 st4 4 u32; ld8 st8 8 u64; }
}
