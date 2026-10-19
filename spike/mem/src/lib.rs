//! A cell's linear memory, mapped copy-on-write from its module's starting
//! image. A module's static data is written once, into an `Image` (an
//! anonymous in-memory file); every cell maps it privately, so pages a cell
//! only reads stay shared, and its first write to a page copies just that
//! page. Past the image it's ordinary zeroed memory. Growth stays inside a
//! reservation (address space, not RAM); past that, the memory moves.
//!
//! This crate is the interpreter's only `unsafe` code. What it relies on:
//! `base` is a mapping of `reserved` bytes that we made and only we change,
//! and its first `len` bytes are readable and writable.

use std::ffi::c_void;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

/// A Wasm page. Every length here is a multiple of it, and it's a multiple
/// of the OS page on every machine we run on (4 KB, 16 KB).
pub const PAGE: usize = 65536;
/// The address space a memory reserves at least.
const MIN_RESERVE: usize = 8 << 20;

/// A module's starting memory: what its data segments put there, up to the
/// last page that isn't all zero.
pub struct Image {
    fd: OwnedFd,
    len: usize,
}

impl Image {
    /// `None`: nothing but zeros, so there's nothing to share.
    pub fn new(bytes: &[u8]) -> io::Result<Option<Arc<Image>>> {
        let Some(last) = bytes.iter().rposition(|&b| b != 0) else { return Ok(None) };
        let len = (last + 1).div_ceil(PAGE) * PAGE;
        let mut f = std::fs::File::from(anon_file()?);
        f.write_all(&bytes[..len.min(bytes.len())])?;
        f.set_len(len as u64)?;
        Ok(Some(Arc::new(Image { fd: f.into(), len })))
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

/// A file that lives only in memory and in our descriptor.
#[cfg(target_os = "linux")]
fn anon_file() -> io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: a C string and flags; the result is checked.
    let fd = unsafe { libc::memfd_create(c"combd-image".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// macOS has no memfd: a temporary file, unlinked at once.
#[cfg(not(target_os = "linux"))]
fn anon_file() -> io::Result<OwnedFd> {
    let path = std::env::temp_dir().join(format!("combd-image-{}-{}", std::process::id(), next_id()));
    let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(f.into())
}

#[cfg(not(target_os = "linux"))]
fn next_id() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub struct Mem {
    base: *mut u8,
    len: usize,
    reserved: usize,
}

// A cell's memory has one owner at a time.
unsafe impl Send for Mem {}

impl Mem {
    /// `len` bytes: the image's (if any) first, zeros after. `len` is a
    /// multiple of `PAGE`, at least the image's length.
    pub fn new(image: Option<&Arc<Image>>, len: usize) -> io::Result<Mem> {
        assert!(len % PAGE == 0 && image.is_none_or(|i| i.len <= len));
        let reserved = len.max(MIN_RESERVE);
        let mut m = Mem { base: reserve(reserved)?, len: 0, reserved };
        if let Some(i) = image {
            // SAFETY: inside our reservation, a whole number of pages; the
            // file is the image's, at least `i.len` long.
            let p = unsafe { libc::mmap(m.base as *mut c_void, i.len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_FIXED, i.fd.as_raw_fd(), 0) };
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
        }
        m.commit(image.map_or(0, |i| i.len), len)?;
        m.len = len;
        Ok(m)
    }

    /// Make [from, to) read-write: fresh pages from the kernel are zero.
    fn commit(&self, from: usize, to: usize) -> io::Result<()> {
        if to > from {
            assert!(to <= self.reserved);
            // SAFETY: inside our reservation, a whole number of pages.
            if unsafe { libc::mprotect(self.base.add(from) as *mut c_void, to - from, libc::PROT_READ | libc::PROT_WRITE) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the first `len` bytes are mapped read-write and ours.
        unsafe { std::slice::from_raw_parts(self.base, self.len) }
    }

    #[inline(always)]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes it the only reference.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.len) }
    }

    /// Grow to `new_len` (a multiple of `PAGE`). Inside the reservation the
    /// new pages are just made writable; past it, everything moves to a
    /// reservation twice as big (and stops sharing the image).
    pub fn grow(&mut self, new_len: usize) -> io::Result<()> {
        assert!(new_len % PAGE == 0 && new_len >= self.len);
        if new_len > self.reserved {
            let mut bigger = Mem { base: reserve(new_len.max(self.reserved * 2))?, len: 0, reserved: new_len.max(self.reserved * 2) };
            bigger.commit(0, new_len)?;
            bigger.len = new_len;
            bigger.bytes_mut()[..self.len].copy_from_slice(self.bytes());
            *self = bigger;
            return Ok(());
        }
        self.commit(self.len, new_len)?;
        self.len = new_len;
        Ok(())
    }
}

impl Clone for Mem {
    /// A copy of every byte, sharing nothing.
    fn clone(&self) -> Mem {
        let mut m = Mem::new(None, self.len).expect("can't map memory for a copy");
        m.bytes_mut().copy_from_slice(self.bytes());
        m
    }
}

impl Drop for Mem {
    fn drop(&mut self) {
        // SAFETY: the whole reservation is ours, and nothing refers to it now.
        unsafe { libc::munmap(self.base as *mut c_void, self.reserved) };
    }
}

/// Address space only: nothing readable, nothing committed.
fn reserve(n: usize) -> io::Result<*mut u8> {
    // SAFETY: a new private anonymous mapping; the result is checked.
    let p = unsafe { libc::mmap(std::ptr::null_mut(), n, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE, -1, 0) };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(p as *mut u8)
}

/// Bytes the allocator has handed out and not had back (glibc only: 0
/// elsewhere). Unlike RSS, it doesn't count memory freed but kept.
pub fn heap_in_use() -> u64 {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: a plain query with no arguments.
        let m = unsafe { libc::mallinfo2() };
        (m.uordblks + m.hblkhd) as u64
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Arc<Image> {
        let mut b = vec![0u8; 3 * PAGE];
        b[10] = 1;
        b[PAGE + 5] = 2;
        Image::new(&b).unwrap().unwrap()
    }

    #[test]
    fn cells_share_an_image_and_never_see_each_others_writes() {
        let img = image();
        assert_eq!(img.len(), 2 * PAGE, "up to the last page with data");
        let (mut a, b) = (Mem::new(Some(&img), 3 * PAGE).unwrap(), Mem::new(Some(&img), 3 * PAGE).unwrap());
        assert_eq!((a.bytes()[10], a.bytes()[PAGE + 5], a.bytes()[2 * PAGE + 1]), (1, 2, 0));
        a.bytes_mut()[10] = 9;
        a.bytes_mut()[2 * PAGE + 1] = 7;
        assert_eq!((a.bytes()[10], b.bytes()[10], b.bytes()[2 * PAGE + 1]), (9, 1, 0), "a's writes are a's");
        let c = Mem::new(Some(&img), 3 * PAGE).unwrap();
        assert_eq!(c.bytes()[10], 1, "and never reach the image");
        assert!(Image::new(&[0; PAGE]).unwrap().is_none(), "all zeros: nothing to share");
    }

    #[test]
    fn growing_keeps_what_was_there() {
        let img = image();
        let mut m = Mem::new(Some(&img), 2 * PAGE).unwrap();
        m.bytes_mut()[PAGE] = 3;
        m.grow(4 * PAGE).unwrap();
        assert_eq!((m.len(), m.bytes()[10], m.bytes()[PAGE], m.bytes()[4 * PAGE - 1]), (4 * PAGE, 1, 3, 0));
        // Past the reservation: it moves, and keeps everything.
        m.grow(MIN_RESERVE + PAGE).unwrap();
        assert_eq!((m.bytes()[10], m.bytes()[PAGE], m.bytes()[PAGE + 5]), (1, 3, 2));
        m.bytes_mut()[MIN_RESERVE] = 4;
        let c = m.clone();
        m.bytes_mut()[MIN_RESERVE] = 5;
        assert_eq!((c.bytes()[MIN_RESERVE], c.bytes()[10]), (4, 1), "a clone is a copy");
    }
}
