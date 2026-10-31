//! The cells' lockdown, on Linux: after their process starts and before it
//! loads any module, two walls go up, for good (nothing undoes them, for
//! this process or any thread or process it starts):
//!
//! - landlock: files only beneath the folders given, read and write or
//!   read only. No TCP (Linux 6.7 and up), no signals to processes outside
//!   the wall and no abstract Unix sockets (6.12 and up).
//! - seccomp: an allowlist of system calls. A call off the list kills the
//!   process (`SIGSYS`), so a break-in is loud, not a quiet error. No
//!   `socket`, `connect`, `execve`, `ptrace`, `process_vm_*`, or new
//!   process (`clone` makes threads only), and, unless asked, no memory
//!   made executable.
//!
//! Written from the kernel's own headers (`linux/landlock.h`,
//! `linux/seccomp.h`, `linux/filter.h`) and libc's call numbers, for the
//! small part of the `landlock` and `seccompiler` crates combd would use.
//!
//! This is the `unsafe` code for it. What it relies on: each call gets
//! pointers to structs that live across it, of the size the call is told.

use std::io;
use std::path::Path;

/// What the walls let through.
pub struct Walls<'a> {
    /// Folders read and written (the cells' data).
    pub rw: Vec<&'a Path>,
    /// Folders or files only read (compiled builds, `/dev/urandom`).
    pub ro: Vec<&'a Path>,
    /// Memory may be made executable: an engine that loads machine code
    /// (compiled builds, a JIT). The interpreter needs none.
    pub exec: bool,
}

/// What went up: landlock's version on this kernel (its ABI, which says
/// which rights it can hold back), and seccomp.
#[derive(Debug)]
pub struct Locked {
    pub landlock: u32,
}

/// Lock this process down. It must have one thread: landlock holds the
/// thread that asks and the ones it starts after. Missing walls are an
/// error: the caller decides whether to run without.
#[cfg(target_os = "linux")]
pub fn lock(w: &Walls) -> io::Result<Locked> {
    let threads = std::fs::read_dir("/proc/self/task")?.count();
    if threads != 1 {
        return Err(io::Error::other(format!("locking down a process with {threads} threads: lock before any starts")));
    }
    // Both walls need it, and it keeps a program run later (there's none)
    // from gaining rights.
    // SAFETY: a plain call, no pointers.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let landlock = landlock::lock(&w.rw, &w.ro)?;
    seccomp::lock(&seccomp::filter(w.exec, std::process::id()))?;
    Ok(Locked { landlock })
}

#[cfg(not(target_os = "linux"))]
pub fn lock(_: &Walls) -> io::Result<Locked> {
    Err(io::Error::other("landlock and seccomp are Linux's"))
}

#[cfg(target_os = "linux")]
mod landlock {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `landlock_ruleset_attr`: what the ruleset handles (holds back unless
    /// a rule allows it). It grew a field at ABI 4 and at ABI 6; a kernel
    /// takes a longer struct than its own if the rest is zero.
    #[repr(C)]
    struct RulesetAttr {
        fs: u64,
        net: u64,
        scoped: u64,
    }

    /// `landlock_path_beneath_attr`, packed as the kernel's is.
    #[repr(C, packed)]
    struct PathBeneath {
        allowed: u64,
        parent_fd: i32,
    }

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;

    // `LANDLOCK_ACCESS_FS_*`.
    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    /// Every right of ABI 1, the 13 first bits (`EXECUTE` to `MAKE_SYM`).
    const ABI1: u64 = (1 << 13) - 1;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;
    const IOCTL_DEV: u64 = 1 << 15;
    // `LANDLOCK_ACCESS_NET_*`, ABI 4.
    const BIND_TCP: u64 = 1 << 0;
    const CONNECT_TCP: u64 = 1 << 1;
    // `LANDLOCK_SCOPE_*`, ABI 6.
    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const SCOPE_SIGNAL: u64 = 1 << 1;

    /// The kernel's landlock version, or why there's none.
    pub fn abi() -> io::Result<u32> {
        // SAFETY: a null attr and size 0 with this flag asks for the version.
        let r = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
        if r < 0 {
            let e = io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::ENOSYS) => io::Error::other("this kernel has no landlock (Linux 5.13 and up)"),
                Some(libc::EOPNOTSUPP) => io::Error::other("landlock is off in this kernel (its `lsm=` boot setting must list it)"),
                _ => e,
            });
        }
        Ok(r as u32)
    }

    /// Hold back every right this kernel knows, then give `rw` and `ro`
    /// theirs, for this thread and the ones it starts after.
    pub fn lock(rw: &[&Path], ro: &[&Path]) -> io::Result<u32> {
        let abi = abi()?;
        let mut fs = ABI1;
        if abi >= 2 {
            fs |= REFER;
        }
        if abi >= 3 {
            fs |= TRUNCATE;
        }
        if abi >= 5 {
            fs |= IOCTL_DEV;
        }
        let attr = RulesetAttr {
            fs,
            net: if abi >= 4 { BIND_TCP | CONNECT_TCP } else { 0 },
            scoped: if abi >= 6 { SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL } else { 0 },
        };
        // SAFETY: `attr` lives across the call, and its size is the one given.
        let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, std::mem::size_of::<RulesetAttr>(), 0u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new fd the call gave us, closed only here.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        // Files: read, write, make, remove, truncate. Never run one, move
        // one, nor make a device, socket, pipe, or link.
        let write = READ_FILE | READ_DIR | WRITE_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_DIR | MAKE_REG | TRUNCATE;
        for (dirs, allowed) in [(rw, write), (ro, READ_FILE | READ_DIR)] {
            for d in dirs {
                allow(&ruleset, d, allowed & fs & !EXECUTE)?;
            }
        }
        // SAFETY: a plain call on the fd we hold.
        if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(abi)
    }

    /// Rights for a file or a folder.
    fn allow(ruleset: &OwnedFd, path: &Path, allowed: u64) -> io::Result<()> {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other("a path with a nul in it"))?;
        // SAFETY: a nul-terminated path.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
        }
        // SAFETY: a new fd, closed only here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // A file takes only a file's rights (the kernel refuses the rest).
        let allowed = if std::fs::metadata(path)?.is_dir() { allowed } else { allowed & (READ_FILE | WRITE_FILE | TRUNCATE) };
        let rule = PathBeneath { allowed, parent_fd: fd.as_raw_fd() };
        // SAFETY: `rule` lives across the call; its type is the one given.
        if unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset.as_raw_fd(), RULE_PATH_BENEATH, &rule as *const PathBeneath, 0u32) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub mod seccomp {
    use libc::{sock_filter, SECCOMP_RET_ALLOW as ALLOW, SECCOMP_RET_ERRNO, SECCOMP_RET_KILL_PROCESS as KILL};
    use std::io;

    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64


    // `struct seccomp_data`: the call's number, its arch, and its six
    // arguments from byte 16, 8 bytes each (the low half first: both
    // arches here are little-endian).
    const NR: u32 = 0;
    const ARCH_AT: u32 = 4;
    const fn arg(i: u32) -> u32 {
        16 + 8 * i
    }

    const LD: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    #[cfg(target_arch = "x86_64")]
    const JGE: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
    const JSET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
    const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

    fn op(code: u16, k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter { code, jt, jf, k }
    }

    /// What a call on the list may do. Each makes a block that always
    /// returns.
    enum Rule {
        Allow,
        /// Refused with this error, for a call code tries and falls back from.
        Errno(i32),
        /// Allowed only if argument `0` has a bit of `1`.
        Has(u32, u32),
        /// Allowed only if argument `0` has no bit of `1`.
        Lacks(u32, u32),
        /// Allowed only if argument `0` (its low 32 bits) is one of these.
        OneOf(u32, Vec<u32>),
    }

    impl Rule {
        fn block(&self) -> Vec<sock_filter> {
            match self {
                Rule::Allow => vec![op(RET, ALLOW, 0, 0)],
                Rule::Errno(e) => vec![op(RET, SECCOMP_RET_ERRNO | *e as u32, 0, 0)],
                Rule::Has(a, bits) => vec![op(LD, arg(*a), 0, 0), op(JSET, *bits, 0, 1), op(RET, ALLOW, 0, 0), op(RET, KILL, 0, 0)],
                Rule::Lacks(a, bits) => vec![op(LD, arg(*a), 0, 0), op(JSET, *bits, 0, 1), op(RET, KILL, 0, 0), op(RET, ALLOW, 0, 0)],
                Rule::OneOf(a, vs) => {
                    let mut b = vec![op(LD, arg(*a), 0, 0)];
                    for v in vs {
                        b.extend([op(JEQ, *v, 0, 1), op(RET, ALLOW, 0, 0)]);
                    }
                    b.push(op(RET, KILL, 0, 0));
                    b
                }
            }
        }
    }

    /// The cells' allowlist: what they're seen to call (`strace -f` on
    /// the arena, million compiled, and plain objects coming and going),
    /// and what a rare path needs (a crash's `abort`, a guard page's signal
    /// handler, SQLite's busy wait and checks, a big `realloc`), each a
    /// call on what the process already holds or on a path landlock
    /// decides. `exec`: memory may be made executable. `pid`: the process,
    /// the only one a signal may go to.
    pub fn filter(exec: bool, pid: u32) -> Vec<sock_filter> {
        use libc::*;
        use Rule::*;
        let prot = || if exec { Allow } else { Lacks(2, PROT_EXEC as u32) };
        #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
        let mut rules: Vec<(c_long, Rule)> = vec![
            // Memory.
            (SYS_brk, Allow),
            (SYS_mmap, prot()),
            (SYS_mprotect, prot()),
            (SYS_munmap, Allow),
            (SYS_mremap, Allow),
            (SYS_madvise, Allow),
            (SYS_membarrier, Allow),
            (SYS_memfd_create, Allow),
            // Threads, and only threads: a new process is a way out. Naming
            // a thread or (glibc) a mapping is all `prctl` may do.
            (SYS_clone, Has(0, CLONE_THREAD as u32)),
            // Its flags are behind a pointer the filter can't read; glibc
            // falls back to `clone`.
            (SYS_clone3, Errno(ENOSYS)),
            (SYS_futex, Allow),
            (SYS_set_robust_list, Allow),
            (SYS_rseq, Allow),
            (SYS_gettid, Allow),
            (SYS_getpid, Allow),
            (SYS_sched_yield, Allow),
            (SYS_sched_getaffinity, Allow),
            (SYS_exit, Allow),
            (SYS_exit_group, Allow),
            (SYS_prctl, OneOf(0, vec![PR_SET_NAME as u32, PR_GET_NAME as u32, PR_SET_VMA as u32])),
            // Signals: its own handlers (guard pages), and to itself only
            // (`abort`).
            (SYS_rt_sigaction, Allow),
            (SYS_rt_sigprocmask, Allow),
            (SYS_rt_sigreturn, Allow),
            (SYS_sigaltstack, Allow),
            (SYS_tgkill, OneOf(0, vec![pid])),
            (SYS_restart_syscall, Allow),
            // Time and chance.
            (SYS_clock_gettime, Allow),
            (SYS_clock_getres, Allow),
            (SYS_gettimeofday, Allow),
            (SYS_nanosleep, Allow),
            (SYS_clock_nanosleep, Allow),
            (SYS_getrandom, Allow),
            // Waiting on its sockets.
            (SYS_epoll_create1, Allow),
            (SYS_epoll_ctl, Allow),
            (SYS_epoll_pwait, Allow),
            (SYS_epoll_pwait2, Allow),
            (SYS_eventfd2, Allow),
            // Its open files and sockets: no new socket, only those it got.
            (SYS_read, Allow),
            (SYS_write, Allow),
            (SYS_writev, Allow),
            (SYS_pread64, Allow),
            (SYS_pwrite64, Allow),
            (SYS_lseek, Allow),
            (SYS_close, Allow),
            (SYS_fcntl, Allow),
            (SYS_fstat, Allow),
            (SYS_newfstatat, Allow),
            (SYS_statx, Allow),
            (SYS_fsync, Allow),
            (SYS_fdatasync, Allow),
            (SYS_ftruncate, Allow),
            (SYS_fchmod, Allow),
            // SQLite, as root (in a container), gives a new `-wal` or
            // `-shm` its database's owner: on a file it has open.
            (SYS_fchown, Allow),
            (SYS_sendto, Allow),
            (SYS_recvfrom, Allow),
            (SYS_ioctl, OneOf(1, vec![FIONBIO as u32, FIOCLEX as u32, FIONCLEX as u32])),
            // Files by name: landlock says which.
            (SYS_openat, Allow),
            (SYS_unlinkat, Allow),
            (SYS_mkdirat, Allow),
            (SYS_readlinkat, Allow),
            (SYS_faccessat, Allow),
            (SYS_faccessat2, Allow),
            (SYS_getcwd, Allow),
            // SQLite asks, to know whether to hand a new file to its owner.
            (SYS_geteuid, Allow),
        ];
        // x86-64's older calls, which aarch64 never had.
        #[cfg(target_arch = "x86_64")]
        rules.extend([
            (SYS_open, Allow),
            (SYS_stat, Allow),
            (SYS_lstat, Allow),
            (SYS_access, Allow),
            (SYS_readlink, Allow),
            (SYS_unlink, Allow),
            (SYS_mkdir, Allow),
            (SYS_rmdir, Allow),
            (SYS_epoll_wait, Allow),
        ]);
        let mut f = vec![op(LD, ARCH_AT, 0, 0), op(JEQ, ARCH, 1, 0), op(RET, KILL, 0, 0), op(LD, NR, 0, 0)];
        // x32's calls are x86-64's numbers with this bit: none of ours.
        #[cfg(target_arch = "x86_64")]
        f.extend([op(JGE, 0x4000_0000, 0, 1), op(RET, KILL, 0, 0)]);
        for (nr, rule) in rules {
            let block = rule.block();
            f.push(op(JEQ, nr as u32, 0, block.len() as u8));
            f.extend(block);
        }
        f.push(op(RET, KILL, 0, 0));
        f
    }

    /// Put `filter` on every thread of this process, for good.
    pub fn lock(filter: &[sock_filter]) -> io::Result<()> {
        let prog = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr() as *mut sock_filter };
        // SAFETY: `prog` and the filter it points at live across the call.
        let r = unsafe { libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, libc::SECCOMP_FILTER_FLAG_TSYNC, &prog as *const libc::sock_fprog) };
        match r {
            0 => Ok(()),
            r if r > 0 => Err(io::Error::other(format!("thread {r} couldn't take the filter"))),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
