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

/// The memory this process costs the machine, in bytes (`None` where we
/// can't tell). On Linux, its cgroup's (a container's, or a service's): its
/// own memory and what the kernel holds for it (sockets' buffers, about 4
/// KB a connection, which RSS leaves out), less file cache it would give
/// back. Elsewhere, or with no cgroup to read, its resident memory.
pub fn resident() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Some(n) = cgroup_dir().and_then(|d| cgroup_used(&std::fs::read_to_string(d.join("memory.current")).ok()?, &std::fs::read_to_string(d.join("memory.stat")).ok()?)) {
            return Some(n);
        }
        let pages: u64 = std::fs::read_to_string("/proc/self/statm").ok()?.split_whitespace().nth(1)?.parse().ok()?;
        // SAFETY: a plain query.
        Some(pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
        // SAFETY: our own pid, a buffer of the size we say.
        let n = unsafe { libc::proc_pidinfo(std::process::id() as i32, libc::PROC_PIDTASKINFO, 0, &mut info as *mut _ as *mut c_void, size) };
        (n == size).then_some(info.pti_resident_size)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    None
}

/// Raise this process's open-file limit to its ceiling, and say what it is
/// now. The soft limit is often 1024, and every socket is a file: a node
/// that holds connections needs the hard limit (Go's runtime raises it the
/// same way). `None`: it can't tell.
pub fn raise_fd_limit() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: a struct of ours, the size the call wants.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 {
            return None;
        }
        let mut want = if r.rlim_max == libc::RLIM_INFINITY { 1 << 20 } else { r.rlim_max };
        // macOS refuses a soft limit past OPEN_MAX, whatever the hard one says.
        if cfg!(target_os = "macos") {
            want = want.min(10240);
        }
        if want > r.rlim_cur {
            let up = libc::rlimit { rlim_cur: want, rlim_max: r.rlim_max };
            // SAFETY: as above.
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &up) } == 0 {
                r.rlim_cur = want;
            }
        }
        Some(r.rlim_cur as u64)
    }
    #[cfg(not(unix))]
    None
}

/// This process's cgroup (v2): `0::<path>` in /proc/self/cgroup, under
/// /sys/fs/cgroup (in a container, its own namespace's root).
#[cfg(target_os = "linux")]
fn cgroup_dir() -> Option<std::path::PathBuf> {
    let me = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = me.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    Some(std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// What a cgroup uses that it can't give back: `memory.current` less the
/// file cache nobody's using (`inactive_file`), as `docker stats` counts.
pub fn cgroup_used(current: &str, stat: &str) -> Option<u64> {
    let total: u64 = current.trim().parse().ok()?;
    let inactive = stat.lines().find_map(|l| l.strip_prefix("inactive_file ")).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
    Some(total.saturating_sub(inactive))
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

    /// Random images, writes, grows (past the reservation too), clones, and
    /// drops, against a plain Vec as the model: every byte must match.
    #[test]
    fn random_operations_match_a_plain_vec() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut rand = move |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n.max(1) as u64) as usize
        };
        for _ in 0..60 {
            // An image of up to 3 pages and a bit, mostly zeros.
            let mut start = vec![0u8; rand(3) * PAGE + rand(5000)];
            for _ in 0..rand(80) {
                if !start.is_empty() {
                    let i = rand(start.len());
                    start[i] = rand(256) as u8;
                }
            }
            let img = Image::new(&start).unwrap();
            let len = start.len().div_ceil(PAGE).max(img.as_ref().map_or(0, |i| i.len() / PAGE)) * PAGE;
            let mut model = start.clone();
            model.resize(len, 0);
            let mut cells = vec![(Mem::new(img.as_ref(), len).unwrap(), model)];
            for _ in 0..200 {
                let k = rand(cells.len());
                match rand(10) {
                    0..=5 => {
                        let (m, v) = &mut cells[k];
                        if !v.is_empty() {
                            let (i, b) = (rand(v.len()), rand(256) as u8);
                            m.bytes_mut()[i] = b;
                            v[i] = b;
                        }
                    }
                    6 => {
                        let (m, v) = &mut cells[k];
                        // Now and then past the reservation, so it moves.
                        let to = if rand(4) == 0 { (MIN_RESERVE / PAGE + rand(3)) * PAGE } else { v.len() + rand(3) * PAGE };
                        if to >= v.len() {
                            m.grow(to).unwrap();
                            v.resize(to, 0);
                        }
                    }
                    7 => {
                        let c = (cells[k].0.clone(), cells[k].1.clone());
                        cells.push(c);
                    }
                    8 if cells.len() > 1 => {
                        cells.swap_remove(k);
                    }
                    _ => cells.push((Mem::new(img.as_ref(), len).unwrap(), { let mut v = start.clone(); v.resize(len, 0); v })),
                }
                // A few bytes every step, all of them now and then.
                let (m, v) = &cells[rand(cells.len())];
                assert_eq!(m.len(), v.len());
                for _ in 0..8 {
                    if !v.is_empty() {
                        let i = rand(v.len());
                        assert_eq!(m.bytes()[i], v[i], "byte {i}");
                    }
                }
            }
            for (m, v) in &cells {
                assert!(m.bytes() == &v[..], "a cell's memory differs from its model");
            }
        }
    }

    #[test]
    fn the_open_file_limit_goes_up_to_its_ceiling() {
        let n = raise_fd_limit().unwrap();
        assert!(n >= 1024, "{n}");
        assert_eq!(raise_fd_limit(), Some(n), "and stays");
    }

    #[test]
    fn a_cgroup_counts_all_but_idle_file_cache() {
        let stat = "anon 600000000\nfile 90000000\nkernel 150000000\nsock 2000000\ninactive_file 70000000\nactive_file 20000000\n";
        assert_eq!(cgroup_used("800000000\n", stat), Some(730_000_000));
        assert_eq!(cgroup_used("max", stat), None);
        assert_eq!(cgroup_used("1000", ""), Some(1000), "no stat: all of it");
        #[cfg(target_os = "linux")]
        assert!(resident().is_some_and(|n| n > 0));
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
