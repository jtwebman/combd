//! The HTTP/1.1 front for the races against workerd, celld, Bun, and Node.
//!
//! One event loop per thread (epoll on Linux, kqueue on macOS). Each loop is a
//! small BEAM-style scheduler: a request takes a warm cell from the loop's pool
//! and runs one slice (about 50 us) at a time. Between slices the loop checks
//! for I/O and gives every other request its turn, so a CPU-heavy request
//! can't hold the core. `COMBD_THREADS` sets the loop count (default: cores).
//!
//! `GET /api/users/42` becomes the cell request `api GET /api/users/42`, and the
//! cell's answer `200 {...}` becomes the status line and body.

use crate::engine::{CellVm, Engine, Outcome};
use crate::host::HostState;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::time::Duration;

const LISTENER: Token = Token(usize::MAX);
const MAX_REQUEST: usize = 1 << 20;

struct Conn {
    s: TcpStream,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    wpos: usize,
    writing: bool,
    close_after: bool,
    /// A request from this connection is running; the next one waits.
    busy: bool,
}

/// A warm cell, reused across requests like a Worker instance.
struct Cell<E: Engine> {
    vm: E::Vm,
    st: HostState,
}

fn fresh<E: Engine>(eng: &E, prog: &E::Program) -> std::io::Result<Cell<E>> {
    let mut vm = eng.spawn(prog).map_err(std::io::Error::other)?;
    let mut st = HostState::default();
    crate::init(&mut vm, &mut st).map_err(std::io::Error::other)?;
    Ok(Cell { vm, st })
}

/// A request in flight.
struct Task<E: Engine> {
    conn: usize,
    cell: Cell<E>,
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
    let mut pool: Vec<Cell<E>> = vec![fresh(eng, prog)?];
    let mut tasks: VecDeque<Task<E>> = VecDeque::new();
    let mut touched: Vec<usize> = vec![];
    loop {
        // Never block while requests are waiting to run.
        let timeout = if tasks.is_empty() { None } else { Some(Duration::ZERO) };
        poll.poll(&mut events, timeout)?;
        touched.clear();
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
                            conns[id] = Some(Conn {
                                s,
                                rbuf: vec![],
                                wbuf: vec![],
                                wpos: 0,
                                writing: false,
                                close_after: false,
                                busy: false,
                            });
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => break, // another loop took it, or a transient error
                    }
                }
                continue;
            }
            let id = ev.token().0;
            let Some(c) = conns.get_mut(id).and_then(Option::as_mut) else { continue };
            if ev.is_readable() && read_all(c) {
                // The peer is done sending: answer what's buffered, then close.
                c.close_after = true;
            }
            touched.push(id);
        }
        // Start the next request on every connection that has one ready.
        for &id in &touched {
            start_next(eng, prog, &mut conns, id, &mut pool, &mut tasks)?;
            settle(&mut poll, &mut conns, &mut free, id)?;
        }
        // One round of the run queue: every request gets one slice.
        for _ in 0..tasks.len() {
            let mut t = tasks.pop_front().unwrap();
            let answer = match t.cell.vm.run(crate::SLICE) {
                Outcome::Yield => {
                    tasks.push_back(t);
                    continue;
                }
                Outcome::HostCall { import, args } => {
                    match crate::serve(&mut t.cell.vm, import, &args, &mut t.cell.st) {
                        Ok(()) => {
                            tasks.push_back(t);
                            continue;
                        }
                        Err(trap) => Err(trap),
                    }
                }
                Outcome::Done(r) => {
                    let a = crate::response(&t.cell.vm, &r);
                    // Cells that export `release` free the answer once we've read it.
                    if t.cell.vm.begin("release", &[r[0] >> 32, r[0] & 0xffff_ffff]).is_ok() {
                        let _ = crate::finish(&mut t.cell.vm, u32::MAX, &mut t.cell.st);
                    }
                    Ok(a)
                }
                Outcome::Trap(trap) => Err(trap),
            };
            let (status, payload) = match answer {
                Ok(a) => {
                    pool.push(t.cell);
                    let (code, rest) = a.split_once(' ').unwrap_or(("500", ""));
                    (code.parse::<u16>().unwrap_or(500), rest.to_string())
                }
                Err(trap) => {
                    // Let it crash: this cell is dropped; the pool makes a fresh one when needed.
                    eprintln!("cell trapped: {trap}");
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            };
            let id = t.conn;
            if let Some(c) = conns.get_mut(id).and_then(Option::as_mut) {
                let _ = write!(c.wbuf, "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", payload.len());
                c.wbuf.extend_from_slice(payload.as_bytes());
                c.busy = false;
                start_next(eng, prog, &mut conns, id, &mut pool, &mut tasks)?;
                settle(&mut poll, &mut conns, &mut free, id)?;
            }
        }
    }
}

/// Flush what's buffered, then close or update write interest.
fn settle(poll: &mut Poll, conns: &mut [Option<Conn>], free: &mut Vec<usize>, id: usize) -> std::io::Result<()> {
    let Some(c) = conns.get_mut(id).and_then(Option::as_mut) else { return Ok(()) };
    let closed = flush(c) || (c.close_after && !c.busy && c.wpos == c.wbuf.len());
    if closed {
        close(poll, conns, free, id);
        return Ok(());
    }
    let want = c.wpos < c.wbuf.len();
    if want != c.writing {
        let interest = if want { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
        poll.registry().reregister(&mut c.s, Token(id), interest)?;
        c.writing = want;
    }
    Ok(())
}

fn close(poll: &mut Poll, conns: &mut [Option<Conn>], free: &mut Vec<usize>, id: usize) {
    if let Some(mut c) = conns.get_mut(id).and_then(Option::take) {
        let _ = poll.registry().deregister(&mut c.s);
        free.push(id);
    }
}

/// If the connection is idle and has a whole request buffered, give it a cell
/// and queue it. A bad request gets a 400 and the connection closes.
fn start_next<E: Engine>(
    eng: &E,
    prog: &E::Program,
    conns: &mut [Option<Conn>],
    id: usize,
    pool: &mut Vec<Cell<E>>,
    tasks: &mut VecDeque<Task<E>>,
) -> std::io::Result<()> {
    let Some(c) = conns.get_mut(id).and_then(Option::as_mut) else { return Ok(()) };
    if c.busy {
        return Ok(());
    }
    let req = match parse(c) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(()) => {
            eprintln!("bad request: {:?}", String::from_utf8_lossy(&c.rbuf[..c.rbuf.len().min(200)]));
            c.rbuf.clear();
            c.wbuf.extend_from_slice(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            c.close_after = true;
            return Ok(());
        }
    };
    let mut cell = match pool.pop() {
        Some(cell) => cell,
        None => fresh(eng, prog)?,
    };
    if let Err(trap) = crate::start(&mut cell.vm, &req, &mut cell.st) {
        eprintln!("cell trapped starting a request: {trap}");
        c.wbuf.extend_from_slice(b"HTTP/1.1 500 OK\r\ncontent-length: 0\r\n\r\n");
        return Ok(());
    }
    c.busy = true;
    tasks.push_back(Task { conn: id, cell });
    Ok(())
}

/// Take one whole request off the front of the read buffer, as a cell request.
fn parse(c: &mut Conn) -> Result<Option<String>, ()> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let hlen = match req.parse(&c.rbuf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut clen = 0usize;
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("content-length") {
            clen = std::str::from_utf8(h.value).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        } else if h.name.eq_ignore_ascii_case("connection") && h.value.eq_ignore_ascii_case(b"close") {
            c.close_after = true;
        }
    }
    if c.rbuf.len() < hlen + clen {
        return Ok(None); // the body hasn't all arrived yet
    }
    let body = String::from_utf8_lossy(&c.rbuf[hlen..hlen + clen]);
    let (method, path) = (req.method.unwrap_or("GET"), req.path.unwrap_or("/"));
    let r = if body.is_empty() { format!("api {method} {path}") } else { format!("api {method} {path} {body}") };
    c.rbuf.drain(..hlen + clen);
    Ok(Some(r))
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
