//! What gets through the walls. Each case runs in a process of its own,
//! locked as the cells are, and tries one thing: what a cell that broke out
//! of its engine would try first. Each is also seen to get through with no
//! walls, so a case that can't fail doesn't pass for one that holds.
//!
//! No test harness: `lock` wants a process with one thread, so this is its
//! own main, and runs itself once per case.

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("walls: landlock and seccomp are Linux's, nothing to test here");
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read, Write};
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    #[derive(Debug, PartialEq, Clone, Copy)]
    enum Want {
        Ok,
        /// Refused by landlock (`EACCES`).
        Denied,
        /// Killed by seccomp (`SIGSYS`).
        Killed,
    }
    use Want::*;

    /// Each case: its name, whether the walls let memory be executable,
    /// and what the walls do to it.
    const CASES: &[(&str, bool, Want)] = &[
        ("read_data", false, Ok),
        ("write_data", false, Ok),
        ("read_builds", false, Ok),
        ("read_a_file_given", false, Ok),
        ("write_a_file_given", false, Denied),
        ("memory_image", false, Ok),
        ("thread", false, Ok),
        ("write_builds", false, Denied),
        ("read_outside", false, Denied),
        ("write_outside", false, Denied),
        ("read_etc", false, Denied),
        ("read_parent_environ", false, Denied),
        ("thread_reads_outside", false, Denied),
        ("tcp_connect", false, Killed),
        ("unix_socket", false, Killed),
        ("run_a_program", false, Killed),
        ("fork", false, Killed),
        ("ptrace", false, Killed),
        ("read_memory_by_pid", false, Killed),
        ("signal_parent", false, Killed),
        ("exec_memory", false, Killed),
        ("exec_memory_later", false, Killed),
        ("exec_memory", true, Ok),
        ("lock_again", false, Killed),
        // Moving a file is off the list too: nothing the cells do needs it.
        ("move_a_file", false, Killed),
    ];

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("try") {
            let (case, exec, locked, dir) = (&args[2], args[3] == "exec", args[4] == "locked", PathBuf::from(&args[5]));
            // Before the walls: `getppid` isn't a call the cells make.
            let ppid = std::os::unix::process::parent_id();
            if locked {
                let (data, builds) = (dir.join("data"), dir.join("builds"));
                let random = std::path::Path::new("/dev/urandom");
                if let Err(e) = combd_wall::lock(&combd_wall::Walls { rw: vec![&data], ro: vec![&builds, random], exec }) {
                    println!("lock: {e}");
                    std::process::exit(3);
                }
            }
            match attempt(case, &dir, ppid) {
                Result::Ok(()) => println!("ok"),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => println!("denied"),
                Err(e) => {
                    println!("{case}: {e}");
                    std::process::exit(2);
                }
            }
            return;
        }
        let dir = std::env::temp_dir().join(format!("combd-walls-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::create_dir_all(dir.join("builds")).unwrap();
        std::fs::write(dir.join("data/a"), "a").unwrap();
        std::fs::write(dir.join("builds/b"), "b").unwrap();
        std::fs::write(dir.join("outside"), "the master key").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::fs::write(dir.join("data/port"), listener.local_addr().unwrap().port().to_string()).unwrap();
        let mut failed = 0;
        for &(case, exec, want) in CASES {
            for locked in [false, true] {
                let got = run(case, exec, locked, &dir);
                let want = if locked { want } else { Ok };
                let walls = if locked { "walls" } else { "no walls" };
                if got == Result::Ok(want) {
                    println!("walls: {case}{} with {walls}: {want:?}", if exec { " (exec)" } else { "" });
                } else {
                    println!("walls: FAILED {case} with {walls}: want {want:?}, got {got:?}");
                    failed += 1;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(failed, 0, "{failed} cases failed");
    }

    fn run(case: &str, exec: bool, locked: bool, dir: &Path) -> Result<Want, String> {
        let me = std::env::current_exe().unwrap();
        let out = Command::new(me)
            .args(["try", case, if exec { "exec" } else { "-" }, if locked { "locked" } else { "-" }, dir.to_str().unwrap()])
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match (out.status.signal(), said.as_str()) {
            (Some(libc::SIGSYS), _) => Result::Ok(Killed),
            (None, "ok") => Result::Ok(Ok),
            (None, "denied") => Result::Ok(Denied),
            (sig, _) => Err(format!("{said} (signal {sig:?}) {}", String::from_utf8_lossy(&out.stderr))),
        }
    }

    fn errno<T: Default + PartialOrd>(r: T) -> std::io::Result<()> {
        if r < T::default() {
            Err(std::io::Error::last_os_error())
        } else {
            Result::Ok(())
        }
    }

    fn map(prot: i32) -> std::io::Result<*mut libc::c_void> {
        // SAFETY: a new anonymous mapping.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), 4096, prot, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Result::Ok(p)
    }

    fn attempt(case: &str, dir: &Path, ppid: u32) -> std::io::Result<()> {
        match case {
            "read_data" => std::fs::read(dir.join("data/a")).map(drop),
            "write_data" => {
                let d = dir.join("data/sub");
                std::fs::create_dir_all(&d)?;
                std::fs::write(d.join("x"), "x")?;
                std::fs::OpenOptions::new().write(true).open(d.join("x"))?.set_len(0)?;
                std::fs::remove_file(d.join("x"))?;
                std::fs::remove_dir(&d)
            }
            "read_builds" => std::fs::read(dir.join("builds/b")).map(drop),
            "move_a_file" => {
                std::fs::write(dir.join("data/m"), "m")?;
                std::fs::rename(dir.join("data/m"), dir.join("data/n"))
            }
            "read_a_file_given" => std::fs::File::open("/dev/urandom")?.read_exact(&mut [0; 16]),
            "write_a_file_given" => std::fs::OpenOptions::new().write(true).open("/dev/urandom").map(drop),
            // What combd-mem does for a module's starting memory.
            "memory_image" => {
                // SAFETY: a plain call, a nul-terminated name.
                let fd = unsafe { libc::memfd_create(c"image".as_ptr(), libc::MFD_CLOEXEC) };
                errno(fd)?;
                let mut f = std::fs::File::from(unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
                f.write_all(&[1; 4096])?;
                f.set_len(8192)?;
                // SAFETY: a private mapping of the file we hold.
                let p = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE, fd, 0) };
                if p == libc::MAP_FAILED {
                    return Err(std::io::Error::last_os_error());
                }
                Result::Ok(())
            }
            "thread" => {
                let d = dir.to_path_buf();
                std::thread::spawn(move || std::fs::read(d.join("data/a")).map(drop)).join().unwrap()
            }
            "write_builds" => std::fs::write(dir.join("builds/c"), "c"),
            "read_outside" => std::fs::read(dir.join("outside")).map(drop),
            "write_outside" => std::fs::write(dir.join("planted"), "x"),
            "read_etc" => std::fs::read("/etc/passwd").map(drop),
            "read_parent_environ" => std::fs::read(format!("/proc/{ppid}/environ")).map(drop),
            "thread_reads_outside" => {
                let d = dir.to_path_buf();
                std::thread::spawn(move || std::fs::read(d.join("outside")).map(drop)).join().unwrap()
            }
            "tcp_connect" => {
                let port: u16 = std::fs::read_to_string(dir.join("data/port"))?.parse().unwrap();
                std::net::TcpStream::connect(("127.0.0.1", port)).map(drop)
            }
            "unix_socket" => std::os::unix::net::UnixStream::pair().map(drop),
            "run_a_program" => Command::new("/bin/true").status().map(drop),
            "fork" => {
                // SAFETY: the child exits at once.
                let pid = unsafe { libc::fork() };
                if pid == 0 {
                    unsafe { libc::_exit(0) };
                }
                errno(pid)
            }
            // SAFETY: plain calls.
            "ptrace" => errno(unsafe { libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) }),
            "read_memory_by_pid" => {
                let mut buf = [0u8; 8];
                let x = 7u64;
                let local = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: 8 };
                let remote = libc::iovec { iov_base: &x as *const u64 as *mut libc::c_void, iov_len: 8 };
                // SAFETY: both buffers live across the call; it reads our own memory.
                errno(unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) })
            }
            "signal_parent" => errno(unsafe { libc::kill(ppid as i32, 0) }),
            "exec_memory" => map(libc::PROT_READ | libc::PROT_EXEC).map(drop),
            "exec_memory_later" => {
                let p = map(libc::PROT_READ | libc::PROT_WRITE)?;
                // SAFETY: our own new mapping.
                errno(unsafe { libc::mprotect(p, 4096, libc::PROT_READ | libc::PROT_EXEC) })
            }
            "lock_again" => {
                // Asking landlock's version: the first call of any new wall.
                let r = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, 1u32) };
                errno(r)
            }
            _ => Err(std::io::Error::other(format!("no case {case}"))),
        }
    }
}
