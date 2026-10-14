//! A size-class allocator for JS cells. JS makes lots of small, short-lived
//! objects, so malloc and free should be a pointer pop and push.
//!
//! Small blocks come from per-size free lists, carved out of 64 KiB chunks.
//! Big blocks go to the system allocator. ponytail: freed small blocks are
//! reused but never handed back to the system, so a cell's memory stays at
//! its high-water mark. Cells are recycled by the pool, so that's fine for now.
//!
//! Single-threaded only: Wasm cells have one thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::ptr::null_mut;

const CLASSES: [usize; 16] = [16, 32, 48, 64, 80, 96, 128, 160, 192, 256, 384, 512, 768, 1024, 1536, 2048];
const MAX_SMALL: usize = 2048;
const CHUNK: usize = 64 * 1024;

/// Class index for each 16-byte step up to MAX_SMALL.
const LOOKUP: [u8; MAX_SMALL / 16 + 1] = {
    let mut t = [0u8; MAX_SMALL / 16 + 1];
    let mut i = 0;
    while i < t.len() {
        let size = i * 16;
        let mut c = 0;
        while CLASSES[c] < size {
            c += 1;
        }
        t[i] = c as u8;
        i += 1;
    }
    t
};

struct State {
    free: [*mut u8; CLASSES.len()],
    bump: *mut u8,
    end: *mut u8,
}

pub struct Pool(UnsafeCell<State>);

// Safety: cells are single-threaded; see the module docs.
unsafe impl Sync for Pool {}

impl Pool {
    pub const fn new() -> Pool {
        Pool(UnsafeCell::new(State { free: [null_mut(); CLASSES.len()], bump: null_mut(), end: null_mut() }))
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

fn class(l: &Layout) -> Option<usize> {
    // Every class is a multiple of 16, so blocks are 16-aligned.
    (l.size() <= MAX_SMALL && l.align() <= 16).then(|| LOOKUP[l.size().div_ceil(16)] as usize)
}

unsafe impl GlobalAlloc for Pool {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let Some(c) = class(&l) else { return System.alloc(l) };
        let s = &mut *self.0.get();
        let p = s.free[c];
        if !p.is_null() {
            s.free[c] = *(p as *mut *mut u8);
            return p;
        }
        let size = CLASSES[c];
        if (s.end as usize) - (s.bump as usize) < size {
            let chunk = System.alloc(Layout::from_size_align_unchecked(CHUNK, 16));
            if chunk.is_null() {
                return null_mut();
            }
            s.bump = chunk;
            s.end = chunk.add(CHUNK);
        }
        let p = s.bump;
        s.bump = p.add(size);
        p
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let Some(c) = class(&l) else { return System.dealloc(p, l) };
        let s = &mut *self.0.get();
        *(p as *mut *mut u8) = s.free[c];
        s.free[c] = p;
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let nl = Layout::from_size_align_unchecked(new, l.align());
        match (class(&l), class(&nl)) {
            (Some(a), Some(b)) if a == b => p,
            (None, None) => System.realloc(p, l, new),
            _ => {
                let q = self.alloc(nl);
                if !q.is_null() {
                    std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                    self.dealloc(p, l);
                }
                q
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_fit_and_blocks_are_reused() {
        for size in 1..=MAX_SMALL {
            let c = class(&Layout::from_size_align(size, 8).unwrap()).unwrap();
            assert!(CLASSES[c] >= size && (c == 0 || CLASSES[c - 1] < size), "{size}");
        }
        let pool = Pool::new();
        unsafe {
            let l = Layout::from_size_align(40, 8).unwrap();
            let a = pool.alloc(l);
            let b = pool.alloc(l);
            assert_ne!(a, b);
            assert_eq!(a as usize % 16, 0);
            pool.dealloc(a, l);
            assert_eq!(pool.alloc(l), a);
            let r = pool.realloc(b, l, 3000);
            assert!(!r.is_null());
            pool.dealloc(r, Layout::from_size_align(3000, 8).unwrap());
        }
    }
}
