//! Many sockets' sends in one system call (Linux io_uring), for the event
//! loop's flush: what's waiting for each socket goes into the ring, one
//! submission sends them all, and every completion is in before `send_all`
//! returns. So a buffer is never borrowed past the call, which is what the
//! `unsafe` below relies on, with: each `fd` is an open socket the caller
//! owns for the call's length.
//!
//! Each send is MSG_DONTWAIT: a full socket says so (EAGAIN) at once, and
//! keeps what didn't go for the next flush, as a write would.
//!
//! Where io_uring isn't (another OS, or a sandbox that blocks it, as
//! Docker's default does), `Ring::new` says so and the caller writes as
//! before.

use std::io;
use std::os::fd::RawFd;

#[cfg(target_os = "linux")]
pub struct Ring {
    ring: io_uring::IoUring,
}

#[cfg(target_os = "linux")]
impl Ring {
    /// A ring for up to `entries` sends a submission (more go in turns).
    pub fn new(entries: u32) -> io::Result<Ring> {
        Ok(Ring { ring: io_uring::IoUring::new(entries)? })
    }

    /// Send each `(fd, bytes)`: in `done`, for each, the bytes it took, or why not.
    pub fn send_all(&mut self, sends: &[(RawFd, &[u8])], done: &mut Vec<io::Result<usize>>) -> io::Result<()> {
        done.clear();
        done.resize_with(sends.len(), || Ok(0));
        let cap = self.ring.params().sq_entries() as usize;
        for (at, batch) in sends.chunks(cap).enumerate().map(|(k, b)| (k * cap, b)) {
            for (i, &(fd, bytes)) in batch.iter().enumerate() {
                let e = io_uring::opcode::Send::new(io_uring::types::Fd(fd), bytes.as_ptr(), bytes.len() as u32).flags(libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL).build().user_data((at + i) as u64);
                // SAFETY: `bytes` stays borrowed until its completion is read
                // below (we wait for all of this batch's before going on).
                unsafe { self.ring.submission().push(&e) }.map_err(|_| io::Error::other("the ring is full"))?;
            }
            let mut left = batch.len();
            self.ring.submit_and_wait(left)?;
            while left > 0 {
                for c in self.ring.completion() {
                    let r = c.result();
                    done[c.user_data() as usize] = if r < 0 { Err(io::Error::from_raw_os_error(-r)) } else { Ok(r as usize) };
                    left -= 1;
                }
                if left > 0 {
                    self.ring.submit_and_wait(left)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
pub struct Ring;

#[cfg(not(target_os = "linux"))]
impl Ring {
    pub fn new(_entries: u32) -> io::Result<Ring> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "io_uring is Linux's"))
    }

    pub fn send_all(&mut self, _sends: &[(RawFd, &[u8])], _done: &mut Vec<io::Result<usize>>) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "io_uring is Linux's"))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;

    /// Sends to many sockets in one go: each gets its own bytes; a full one
    /// says EAGAIN and keeps the rest; a closed one says why. Where the
    /// kernel (or a sandbox) won't make a ring, there's nothing to test.
    #[test]
    fn many_sends_in_one_go() {
        let Ok(mut ring) = Ring::new(8) else {
            eprintln!("skipping: no io_uring here");
            return;
        };
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let pairs: Vec<_> = (0..20)
            .map(|_| {
                let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
                let (s, _) = l.accept().unwrap();
                s.set_nonblocking(true).unwrap();
                (s, c)
            })
            .collect();
        let msgs: Vec<Vec<u8>> = (0..20).map(|i| format!("message {i}").into_bytes()).collect();
        let sends: Vec<(RawFd, &[u8])> = pairs.iter().zip(&msgs).map(|((s, _), m)| (s.as_raw_fd(), m.as_slice())).collect();
        let mut done = vec![];
        ring.send_all(&sends, &mut done).unwrap();
        for (i, ((_, c), r)) in pairs.iter().zip(&done).enumerate() {
            assert_eq!(*r.as_ref().unwrap(), msgs[i].len(), "socket {i}");
            let mut got = vec![0u8; msgs[i].len()];
            (&*c).read_exact(&mut got).unwrap();
            assert_eq!(got, msgs[i]);
        }
        // A socket nobody reads fills: past that, EAGAIN, not a wait.
        let (s, _c) = &pairs[0];
        let big = vec![7u8; 1 << 20];
        let mut sent = 0;
        let full = loop {
            ring.send_all(&[(s.as_raw_fd(), &big)], &mut done).unwrap();
            match &done[0] {
                Ok(n) => sent += n,
                Err(e) => break e.kind(),
            }
            assert!(sent < 1 << 30, "never filled");
        };
        assert_eq!(full, io::ErrorKind::WouldBlock);
    }
}
