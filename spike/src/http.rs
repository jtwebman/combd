//! The HTTP/1.1 front for the race against workerd, celld, Bun, and Node.
//!
//! One event loop per thread (epoll on Linux, kqueue on macOS) and one warm
//! cell per loop, run on the same thread as the I/O, so there's no context
//! switch per request. `COMBD_THREADS` sets the loop count (default: cores).
//! `GET /api/users/42` becomes the cell request `api GET /api/users/42`, and the
//! cell's answer `200 {...}` becomes the status line and body.

use crate::engine::Engine;
use crate::host::HostState;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use std::io::{ErrorKind, Read, Write};

const LISTENER: Token = Token(usize::MAX);
const MAX_REQUEST: usize = 1 << 20;

struct Conn {
    s: TcpStream,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    wpos: usize,
    writing: bool,
    close_after: bool,
}

/// One warm cell, reused across requests like a Worker instance.
struct Cell<'a, E: Engine> {
    eng: &'a E,
    prog: &'a E::Program,
    vm: E::Vm,
    st: HostState,
}

impl<'a, E: Engine> Cell<'a, E> {
    fn new(eng: &'a E, prog: &'a E::Program) -> std::io::Result<Self> {
        let mut vm = eng.spawn(prog).map_err(std::io::Error::other)?;
        let mut st = HostState::default();
        crate::init(&mut vm, &mut st).map_err(std::io::Error::other)?;
        Ok(Cell { eng, prog, vm, st })
    }

    fn call(&mut self, req: &str) -> (u16, String) {
        match crate::request(&mut self.vm, req, &mut self.st) {
            Ok(answer) => {
                let (code, rest) = answer.split_once(' ').unwrap_or(("500", ""));
                (code.parse().unwrap_or(500), rest.to_string())
            }
            Err(trap) => {
                // Let it crash: the next request gets a fresh cell.
                if let Ok(fresh) = Cell::new(self.eng, self.prog) {
                    *self = fresh;
                }
                (500, format!("{{\"error\":{trap:?}}}"))
            }
        }
    }
}

pub fn serve<E>(eng: &E, prog: &E::Program, port: u16)
where
    E: Engine + Sync,
    E::Program: Sync,
{
    let threads = std::env::var("COMBD_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let listener = std::net::TcpListener::bind(("0.0.0.0", port)).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    println!("combd     listening on :{port} with {threads} event loop(s)");
    std::thread::scope(|sc| {
        for _ in 0..threads {
            let l = listener.try_clone().expect("clone listener");
            sc.spawn(move || {
                if let Err(e) = event_loop(eng, prog, TcpListener::from_std(l)) {
                    eprintln!("event loop stopped: {e}");
                }
            });
        }
    });
}

fn event_loop<E: Engine>(eng: &E, prog: &E::Program, mut listener: TcpListener) -> std::io::Result<()> {
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);
    poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
    let mut conns: Vec<Option<Conn>> = vec![];
    let mut free: Vec<usize> = vec![];
    let mut cell = Cell::new(eng, prog)?;
    loop {
        poll.poll(&mut events, None)?;
        for ev in events.iter() {
            if ev.token() == LISTENER {
                loop {
                    match listener.accept() {
                        Ok((mut s, _)) => {
                            s.set_nodelay(true)?;
                            let id = free.pop().unwrap_or_else(|| {
                                conns.push(None);
                                conns.len() - 1
                            });
                            poll.registry().register(&mut s, Token(id), Interest::READABLE)?;
                            conns[id] = Some(Conn { s, rbuf: vec![], wbuf: vec![], wpos: 0, writing: false, close_after: false });
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => break, // another loop took it, or a transient error
                    }
                }
                continue;
            }
            let id = ev.token().0;
            let Some(c) = conns.get_mut(id).and_then(Option::as_mut) else { continue };
            let mut closed = ev.is_readable() && read_all(c);
            closed |= process(c, &mut cell);
            closed |= flush(c);
            closed |= c.close_after && c.wpos == c.wbuf.len();
            if closed {
                let _ = poll.registry().deregister(&mut c.s);
                conns[id] = None;
                free.push(id);
            } else {
                let want = c.wpos < c.wbuf.len();
                if want != c.writing {
                    let interest = if want { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
                    poll.registry().reregister(&mut c.s, Token(id), interest)?;
                    c.writing = want;
                }
            }
        }
    }
}

/// Read until the socket would block. Returns true if the peer closed.
fn read_all(c: &mut Conn) -> bool {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match c.s.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                c.rbuf.extend_from_slice(&buf[..n]);
                if c.rbuf.len() > MAX_REQUEST {
                    return true;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
}

/// Answer every complete request in the read buffer. Returns true on a
/// request we can't parse (the connection closes after a 400).
fn process<E: Engine>(c: &mut Conn, cell: &mut Cell<E>) -> bool {
    let mut used = 0;
    let mut bad = false;
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut headers);
        let hlen = match req.parse(&c.rbuf[used..]) {
            Ok(httparse::Status::Complete(n)) => n,
            Ok(httparse::Status::Partial) => break,
            Err(_) => {
                bad = true;
                break;
            }
        };
        let mut clen = 0usize;
        for h in req.headers.iter() {
            if h.name.eq_ignore_ascii_case("content-length") {
                clen = std::str::from_utf8(h.value).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            } else if h.name.eq_ignore_ascii_case("connection") && h.value.eq_ignore_ascii_case(b"close") {
                c.close_after = true;
            }
        }
        if c.rbuf.len() - used < hlen + clen {
            break; // the body hasn't all arrived yet
        }
        let body = String::from_utf8_lossy(&c.rbuf[used + hlen..used + hlen + clen]);
        let (method, path) = (req.method.unwrap_or("GET"), req.path.unwrap_or("/"));
        let r = if body.is_empty() { format!("api {method} {path}") } else { format!("api {method} {path} {body}") };
        let (status, payload) = cell.call(&r);
        let _ = write!(c.wbuf, "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", payload.len());
        c.wbuf.extend_from_slice(payload.as_bytes());
        used += hlen + clen;
    }
    c.rbuf.drain(..used);
    if bad {
        c.wbuf.extend_from_slice(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        c.close_after = true;
    }
    false
}

/// Write until done or the socket would block. Returns true on a write error.
fn flush(c: &mut Conn) -> bool {
    while c.wpos < c.wbuf.len() {
        match c.s.write(&c.wbuf[c.wpos..]) {
            Ok(0) => return true,
            Ok(n) => c.wpos += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
    c.wbuf.clear();
    c.wpos = 0;
    false
}
