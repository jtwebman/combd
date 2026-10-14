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
//!
//! `/do/<name>/<path>` goes to the Durable Object `<name>`: one warm cell with
//! its own SQLite file (under `COMBD_DATA`), running one request at a time.
//! Each request is a transaction, committed (fsynced) before its answer is
//! written: the output gate. A crash rolls it back and drops the cell; the
//! next request gets a fresh one, which loads its state from storage.

use crate::engine::{CellVm, Engine, Outcome};
use crate::host::HostState;
use crate::store::{self, Db};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LISTENER: Token = Token(usize::MAX);
const MAX_REQUEST: usize = 1 << 20;
const MAX_NAME: usize = 256;

struct Conn {
    s: TcpStream,
    /// Bumped each time this slot gets a new connection, so an answer for a
    /// closed connection never reaches the next one in the same slot.
    gen: u64,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    wpos: usize,
    writing: bool,
    close_after: bool,
    /// A request from this connection is running or waiting; the next one waits.
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

/// Which connection a request answers: (slot, generation).
type Who = (usize, u64);

/// A request in flight.
struct Task<E: Engine> {
    who: Who,
    cell: Cell<E>,
    /// The Durable Object it runs in, if any.
    obj: Option<String>,
}

/// A Durable Object on this loop.
struct Object<E: Engine> {
    /// The warm cell, when no request is using it. `None` after a crash.
    cell: Option<Cell<E>>,
    db: Arc<Mutex<Db>>,
    busy: bool,
    /// Requests waiting their turn.
    waiting: VecDeque<(Who, String)>,
}

/// A parsed request: the cell request, and the object it's for.
struct Req {
    obj: Option<String>,
    cell_req: String,
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
    let data = PathBuf::from(std::env::var("COMBD_DATA").unwrap_or_else(|_| "data".into()));
    serve_with(eng, prog, port, threads, data)
}

pub fn serve_with<E>(eng: &E, prog: &E::Program, port: u16, threads: usize, data: PathBuf)
where
    E: Engine + Sync,
    E::Program: Sync,
{
    std::fs::create_dir_all(&data).expect("create the data directory");
    let listener = std::net::TcpListener::bind(("0.0.0.0", port)).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    println!("combd     listening on :{port} with {threads} event loop(s), data in {}", data.display());
    std::thread::scope(|sc| {
        for _ in 0..threads {
            let l = listener.try_clone().expect("clone listener");
            let data = data.clone();
            sc.spawn(move || {
                let mut lp = Loop {
                    eng,
                    prog,
                    pool: vec![],
                    tasks: VecDeque::new(),
                    objects: HashMap::new(),
                    data,
                    threads,
                    conns: vec![],
                    free: vec![],
                    next_gen: 0,
                };
                if let Err(e) = lp.run(TcpListener::from_std(l)) {
                    eprintln!("event loop stopped: {e}");
                }
            });
        }
    });
}

/// Everything one event loop owns.
struct Loop<'a, E: Engine> {
    eng: &'a E,
    prog: &'a E::Program,
    pool: Vec<Cell<E>>,
    tasks: VecDeque<Task<E>>,
    objects: HashMap<String, Object<E>>,
    data: PathBuf,
    threads: usize,
    conns: Vec<Option<Conn>>,
    free: Vec<usize>,
    next_gen: u64,
}

impl<E: Engine> Loop<'_, E> {
    fn run(&mut self, mut listener: TcpListener) -> std::io::Result<()> {
        let mut poll = Poll::new()?;
        let mut events = Events::with_capacity(1024);
        poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
        self.pool.push(fresh(self.eng, self.prog)?);
        let mut touched: Vec<usize> = vec![];
        loop {
            // Never block while requests are waiting to run.
            let timeout = if self.tasks.is_empty() { None } else { Some(Duration::ZERO) };
            poll.poll(&mut events, timeout)?;
            touched.clear();
            for ev in events.iter() {
                if ev.token() == LISTENER {
                    self.accept(&mut poll, &mut listener)?;
                    continue;
                }
                let id = ev.token().0;
                let Some(c) = self.conns.get_mut(id).and_then(Option::as_mut) else { continue };
                if ev.is_readable() && read_all(c) {
                    // The peer is done sending: answer what's buffered, then close.
                    c.close_after = true;
                }
                touched.push(id);
            }
            // Start the next request on every connection that has one ready.
            for &id in &touched {
                self.start_next(id)?;
                self.settle(&mut poll, id)?;
            }
            // One round of the run queue: every request gets one slice.
            for _ in 0..self.tasks.len() {
                let mut t = self.tasks.pop_front().unwrap();
                let answer = match t.cell.vm.run(crate::SLICE) {
                    Outcome::Yield => {
                        self.tasks.push_back(t);
                        continue;
                    }
                    Outcome::HostCall { import, args } => match crate::serve(&mut t.cell.vm, import, &args, &mut t.cell.st) {
                        Ok(()) => {
                            self.tasks.push_back(t);
                            continue;
                        }
                        Err(trap) => Err(trap),
                    },
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
                let who = t.who;
                let reply = self.finish_task(t, answer);
                self.reply(&mut poll, who, reply)?;
            }
        }
    }

    fn accept(&mut self, poll: &mut Poll, listener: &mut TcpListener) -> std::io::Result<()> {
        loop {
            match listener.accept() {
                Ok((mut s, _)) => {
                    s.set_nodelay(true)?;
                    let id = self.free.pop().unwrap_or_else(|| {
                        self.conns.push(None);
                        self.conns.len() - 1
                    });
                    poll.registry().register(&mut s, Token(id), Interest::READABLE)?;
                    self.next_gen += 1;
                    self.conns[id] = Some(Conn {
                        s,
                        gen: self.next_gen,
                        rbuf: vec![],
                        wbuf: vec![],
                        wpos: 0,
                        writing: false,
                        close_after: false,
                        busy: false,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(_) => return Ok(()), // another loop took it, or a transient error
            }
        }
    }

    /// A request is done: put its cell back (or drop it after a crash), close
    /// its object's transaction, and say what to answer.
    fn finish_task(&mut self, t: Task<E>, answer: Result<String, String>) -> (u16, String) {
        let Task { cell, obj, .. } = t;
        let Some(name) = obj else {
            return match answer {
                Ok(a) => {
                    self.pool.push(cell);
                    split_answer(&a)
                }
                Err(trap) => {
                    // Let it crash: this cell is dropped; the pool makes a fresh one when needed.
                    eprintln!("cell trapped: {trap}");
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            };
        };
        let o = self.objects.get_mut(&name).expect("object of a running task");
        o.busy = false;
        let reply = {
            let db = o.db.lock().unwrap();
            match answer {
                // The output gate: the answer goes out only once its writes are on disk.
                Ok(a) => match db.commit() {
                    Ok(()) => {
                        o.cell = Some(cell);
                        split_answer(&a)
                    }
                    Err(e) => {
                        // Memory and disk no longer agree, so the cell goes too.
                        eprintln!("object {name:?}: commit failed: {e}");
                        db.rollback();
                        (500, "{\"error\":\"storage\"}".into())
                    }
                },
                Err(trap) => {
                    eprintln!("object {name:?} trapped: {trap}");
                    db.rollback();
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            }
        };
        self.next_waiting(&name);
        reply
    }

    /// Start the next request queued on an object, skipping closed connections.
    fn next_waiting(&mut self, name: &str) {
        while let Some((who, req)) = self.objects.get_mut(name).and_then(|o| o.waiting.pop_front()) {
            if self.conn(who).is_some() {
                if let Err(reply) = self.run_object(name, who, req) {
                    self.queue_reply(who, reply);
                }
                return;
            }
        }
    }

    fn conn(&mut self, (id, gen): Who) -> Option<&mut Conn> {
        self.conns.get_mut(id).and_then(Option::as_mut).filter(|c| c.gen == gen)
    }

    /// Buffer an answer for `who`, if it's still connected.
    fn queue_reply(&mut self, who: Who, (status, payload): (u16, String)) {
        if let Some(c) = self.conn(who) {
            let _ = write!(c.wbuf, "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", payload.len());
            c.wbuf.extend_from_slice(payload.as_bytes());
            c.busy = false;
        }
    }

    fn reply(&mut self, poll: &mut Poll, who: Who, reply: (u16, String)) -> std::io::Result<()> {
        if self.conn(who).is_none() {
            return Ok(());
        }
        self.queue_reply(who, reply);
        self.start_next(who.0)?;
        self.settle(poll, who.0)
    }

    /// Flush what's buffered, then close or update write interest.
    fn settle(&mut self, poll: &mut Poll, id: usize) -> std::io::Result<()> {
        let Some(c) = self.conns.get_mut(id).and_then(Option::as_mut) else { return Ok(()) };
        let closed = flush(c) || (c.close_after && !c.busy && c.wpos == c.wbuf.len());
        if closed {
            if let Some(mut c) = self.conns.get_mut(id).and_then(Option::take) {
                let _ = poll.registry().deregister(&mut c.s);
                self.free.push(id);
            }
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

    /// If the connection is idle and has a whole request buffered, start it.
    /// A bad request gets a 400 and the connection closes.
    fn start_next(&mut self, id: usize) -> std::io::Result<()> {
        let Some(c) = self.conns.get_mut(id).and_then(Option::as_mut) else { return Ok(()) };
        if c.busy {
            return Ok(());
        }
        let who = (id, c.gen);
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
        c.busy = true;
        let Some(name) = req.obj else {
            let mut cell = match self.pool.pop() {
                Some(cell) => cell,
                None => fresh(self.eng, self.prog)?,
            };
            if let Err(trap) = crate::start(&mut cell.vm, &req.cell_req, &mut cell.st) {
                eprintln!("cell trapped starting a request: {trap}");
                self.queue_reply(who, (500, String::new()));
                return Ok(());
            }
            self.tasks.push_back(Task { who, cell, obj: None });
            return Ok(());
        };
        if self.threads > 1 {
            // ponytail: each loop has its own objects, so two loops could both
            // run one object. Route by name across loops before lifting this.
            self.queue_reply(who, (503, "{\"error\":\"durable objects need COMBD_THREADS=1 for now\"}".into()));
            return Ok(());
        }
        if !self.objects.contains_key(&name) {
            let db = match Db::open(&store::path_for(&self.data, &name)) {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("object {name:?}: can't open storage: {e}");
                    self.queue_reply(who, (500, "{\"error\":\"storage\"}".into()));
                    return Ok(());
                }
            };
            let o = Object { cell: None, db: Arc::new(Mutex::new(db)), busy: false, waiting: VecDeque::new() };
            self.objects.insert(name.clone(), o);
        }
        let o = self.objects.get_mut(&name).unwrap();
        if o.busy {
            o.waiting.push_back((who, req.cell_req));
            return Ok(());
        }
        if let Err(reply) = self.run_object(&name, who, req.cell_req) {
            self.queue_reply(who, reply);
        }
        Ok(())
    }

    /// Begin one request on an object: its warm cell (or a fresh one), inside
    /// a transaction.
    fn run_object(&mut self, name: &str, who: Who, req: String) -> Result<(), (u16, String)> {
        let (eng, prog) = (self.eng, self.prog);
        let o = self.objects.get_mut(name).unwrap();
        let mut cell = match o.cell.take() {
            Some(c) => c,
            None => {
                let mut c = fresh(eng, prog).map_err(|e| (500, format!("{{\"error\":{:?}}}", e.to_string())))?;
                match c.vm.host_state() {
                    Some(st) => st.db = Some(o.db.clone()),
                    None => c.st.db = Some(o.db.clone()),
                }
                c
            }
        };
        if let Err(e) = o.db.lock().unwrap().begin() {
            eprintln!("object {name:?}: begin failed: {e}");
            o.cell = Some(cell);
            return Err((500, "{\"error\":\"storage\"}".into()));
        }
        if let Err(trap) = crate::start(&mut cell.vm, &req, &mut cell.st) {
            eprintln!("object {name:?} trapped starting a request: {trap}");
            o.db.lock().unwrap().rollback();
            return Err((500, String::new()));
        }
        o.busy = true;
        self.tasks.push_back(Task { who, cell, obj: Some(name.to_string()) });
        Ok(())
    }
}

fn split_answer(a: &str) -> (u16, String) {
    let (code, rest) = a.split_once(' ').unwrap_or(("500", ""));
    (code.parse::<u16>().unwrap_or(500), rest.to_string())
}

/// Take one whole request off the front of the read buffer.
fn parse(c: &mut Conn) -> Result<Option<Req>, ()> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let hlen = match req.parse(&c.rbuf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut clen = 0usize;
    let mut close = false;
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("content-length") {
            clen = std::str::from_utf8(h.value).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        } else if h.name.eq_ignore_ascii_case("connection") && h.value.eq_ignore_ascii_case(b"close") {
            close = true;
        }
    }
    if c.rbuf.len() < hlen + clen {
        // The body hasn't all arrived yet. Don't act on `connection: close`
        // until it has, or the idle connection gets closed under the request.
        return Ok(None);
    }
    c.close_after |= close;
    let body = String::from_utf8_lossy(&c.rbuf[hlen..hlen + clen]);
    let (method, path) = (req.method.unwrap_or("GET"), req.path.unwrap_or("/"));
    // `/do/<name>/<rest>`: a Durable Object request.
    let (obj, kind, path) = match path.strip_prefix("/do/") {
        Some(rest) => {
            let (name, sub) = rest.split_once('/').map_or((rest, "/".to_string()), |(n, s)| (n, format!("/{s}")));
            if name.is_empty() || name.len() > MAX_NAME {
                return Err(());
            }
            (Some(name.to_string()), "do", sub)
        }
        None => (None, "api", path.to_string()),
    };
    let cell_req = if body.is_empty() { format!("{kind} {method} {path}") } else { format!("{kind} {method} {path} {body}") };
    c.rbuf.drain(..hlen + clen);
    Ok(Some(Req { obj, cell_req }))
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
