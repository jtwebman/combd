//! Guard-page linear memory. Each cell reserves 4 GiB + 64 KiB of address
//! space (address space only, no RAM). Only the live Wasm memory is mapped
//! read-write; the rest is PROT_NONE. A Wasm address is a u32 plus a static
//! offset under 64 KiB, so it always lands inside the reservation: on live
//! memory, or on a guard page, where `trap.c` turns the fault into a trap.
//!
//! This file and `trap.c` are the only `unsafe` code in combd-aot. The one
//! invariant they rely on: every address the fast path touches is below
//! `RESERVE`, which `rt` guarantees by sending big offsets down the checked path.

use std::ffi::c_void;
use std::sync::Once;

pub const GUARD: usize = 64 << 10;
pub const RESERVE: usize = (1 << 32) + GUARD;
/// Static offsets up to this use the unchecked fast path. The rest of the
/// guard covers the access size (at most 8 bytes).
pub const MAX_FAST_OFFSET: u32 = (GUARD - 16) as u32;

pub struct Mem {
    base: *mut u8,
    len: usize,
}

// A cell's memory has one owner at a time.
unsafe impl Send for Mem {}

impl Mem {
    pub fn new(len: usize) -> Mem {
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                RESERVE,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert!(base != libc::MAP_FAILED, "could not reserve address space for a cell");
        // Build it in place: a temporary Mem would unmap the reservation on drop.
        let mut m = Mem { base: base as *mut u8, len: 0 };
        m.commit(0, len);
        m.len = len;
        m
    }

    /// Make [from, to) read-write. Fresh pages from the kernel are always zero.
    fn commit(&self, from: usize, to: usize) {
        if to > from {
            assert!(to <= RESERVE);
            let r = unsafe {
                libc::mprotect(self.base.add(from) as *mut c_void, to - from, libc::PROT_READ | libc::PROT_WRITE)
            };
            assert_eq!(r, 0, "mprotect failed");
        }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    fn live(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base, self.len) }
    }

    #[inline(always)]
    pub fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.base, self.len) }
    }

    #[inline(always)]
    pub fn get(&self, a: usize, n: usize) -> Option<&[u8]> {
        self.live().get(a..a.checked_add(n)?)
    }

    #[inline(always)]
    pub fn get_mut(&mut self, a: usize, n: usize) -> Option<&mut [u8]> {
        self.bytes().get_mut(a..a.checked_add(n)?)
    }

    pub fn grow(&mut self, max_pages: u32, n: u64) -> u64 {
        let old = self.len / crate::rt::PAGE;
        let new = old as u64 + n as u32 as u64;
        if new > max_pages as u64 || new as usize * crate::rt::PAGE > RESERVE - GUARD {
            return u32::MAX as u64;
        }
        let new_len = new as usize * crate::rt::PAGE;
        self.commit(self.len, new_len);
        self.len = new_len;
        old as u64
    }

    /// No bounds check: past the live memory is a guard page, and the fault
    /// becomes a trap. Callers keep `a + N <= RESERVE` (see `rt`).
    #[inline(always)]
    pub(crate) fn load<const N: usize>(&self, a: usize) -> [u8; N] {
        debug_assert!(a + N <= RESERVE);
        let v = unsafe { std::ptr::read_unaligned(self.base.add(a) as *const [u8; N]) };
        keep(&v);
        v
    }

    #[inline(always)]
    pub(crate) fn store<const N: usize>(&mut self, a: usize, v: [u8; N]) {
        debug_assert!(a + N <= RESERVE);
        unsafe { std::ptr::write_unaligned(self.base.add(a) as *mut [u8; N], v) }
    }

    /// The whole reservation, for `catch`.
    pub fn range(&self) -> (usize, usize) {
        (self.base as usize, RESERVE)
    }
}

/// A Wasm load must trap even if nothing uses its value. To LLVM a plain read
/// has no side effects, so it deletes unused ones, and then the guard page
/// never sees them (found by differential fuzzing: a load from a 0-page memory
/// returned instead of trapping). An empty asm that takes the value keeps the
/// load. It costs a register, and every other optimization still applies
/// (`nomem`: it touches no memory). wasm2c does the same (`FORCE_READ`).
#[inline(always)]
fn keep<const N: usize>(v: &[u8; N]) {
    let mut w = [0u8; 8];
    w[..N.min(8)].copy_from_slice(&v[..N.min(8)]);
    let w = u64::from_ne_bytes(w);
    unsafe { std::arch::asm!("/* {0} */", in(reg) w, options(nomem, nostack, preserves_flags)) };
}

impl Clone for Mem {
    /// A fresh reservation holding the live bytes. All-zero chunks are skipped,
    /// so pages the cell left empty stay untouched (and zero).
    fn clone(&self) -> Mem {
        const CHUNK: usize = 16 * 1024;
        let mut m = Mem::new(self.len);
        let to = m.bytes();
        for (i, c) in self.live().chunks(CHUNK).enumerate() {
            if c.iter().fold(0u8, |acc, &b| acc | b) != 0 {
                to[i * CHUNK..i * CHUNK + c.len()].copy_from_slice(c);
            }
        }
        m
    }
}

impl Drop for Mem {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut c_void, RESERVE) };
    }
}

extern "C" {
    fn combd_install();
    fn combd_catch(cb: extern "C" fn(*mut c_void), arg: *mut c_void, lo: *mut c_void, len: usize) -> i32;
}

/// Run `f` with `range` (a cell's reservation) armed. Returns `None` if a
/// guard-page fault inside that range ended it as a trap.
pub fn catch<F: FnOnce() -> R, R>(range: (usize, usize), f: F) -> Option<R> {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| unsafe { combd_install() });
    struct Slot<F, R> {
        f: Option<F>,
        r: Option<std::thread::Result<R>>,
    }
    extern "C" fn tramp<F: FnOnce() -> R, R>(p: *mut c_void) {
        let s = unsafe { &mut *(p as *mut Slot<F, R>) };
        let f = s.f.take().unwrap();
        // A panic must never unwind through the C frame.
        s.r = Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
    }
    let mut slot = Slot { f: Some(f), r: None };
    let trapped = unsafe { combd_catch(tramp::<F, R>, &mut slot as *mut _ as *mut c_void, range.0 as *mut c_void, range.1) };
    if trapped != 0 {
        return None;
    }
    match slot.r {
        Some(Ok(r)) => Some(r),
        Some(Err(p)) => std::panic::resume_unwind(p),
        None => None,
    }
}
