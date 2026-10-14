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
//! Durable Objects: `/do/<name>/<path>` goes to the object `<name>`.
//! - **One owner.** Each object lives on exactly one loop, picked by a hash of
//!   its name. A request that arrives on another loop is forwarded there, and
//!   the answer comes back the same way.
//! - **One request at a time,** in a warm cell with its own SQLite file (under
//!   `COMBD_DATA`). Each request is a transaction, committed (fsynced) before
//!   its answer is written: the output gate. A crash rolls it back and drops
//!   the cell; the next request gets a fresh one, which loads from storage.
//! - **Alarms.** A cell sets one with `alarm_set`; it commits with the request.
//!   The owner runs `ALARM` when it's due. A failed alarm is retried with
//!   backoff, up to `ALARM_TRIES` times.
//! - **Eviction.** Objects idle for `COMBD_IDLE_MS`, or past `COMBD_MAX_OBJECTS`
//!   per loop, leave memory. They come back from storage when needed.
//!
//! `GET /_combd/stats` shows what the node is doing.

use crate::engine::{CellVm, Engine, Outcome};
use crate::host::{now_ms, HostState};
use crate::store::{self, AlarmIndex, Db};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LISTENER: Token = Token(usize::MAX);
const WAKER: Token = Token(usize::MAX - 1);
const MAX_REQUEST: usize = 1 << 20;
const MAX_NAME: usize = 256;
/// An alarm that keeps failing is tried this many times, then dropped.
const ALARM_TRIES: u32 = 6;
/// The first retry waits this long; each next one waits twice as long.
const ALARM_BACKOFF_MS: i64 = 500;
/// The loop wakes at least this often (alarms, eviction, stop).
const TICK: Duration = Duration::from_millis(100);

pub struct Config {
    pub threads: usize,
    pub data: PathBuf,
    /// Objects idle this long leave memory.
    pub idle: Duration,
    /// The most objects in memory, per loop.
    pub max_objects: usize,
    /// Set it to stop the server.
    pub stop: Arc<AtomicBool>,
}

impl Config {
    pub fn from_env() -> Config {
        let num = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Config {
            threads: num("COMBD_THREADS", std::thread::available_parallelism().map_or(1, |n| n.get())),
            data: PathBuf::from(std::env::var("COMBD_DATA").unwrap_or_else(|_| "data".into())),
            idle: Duration::from_millis(num("COMBD_IDLE_MS", 60_000) as u64),
            max_objects: num("COMBD_MAX_OBJECTS", 10_000),
            stop: Arc::default(),
        }
    }
}

/// Counters for `/_combd/stats`, for the whole node.
#[derive(Default)]
struct Stats {
    objects_live: AtomicU64,
    objects_opened: AtomicU64,
    evictions: AtomicU64,
    alarms_fired: AtomicU64,
    alarms_failed: AtomicU64,
    forwarded: AtomicU64,
}

/// What the loops share.
struct Shared {
    cfg: Config,
    peers: Vec<(Sender<Msg>, Arc<Waker>)>,
    index: Mutex<AlarmIndex>,
    stats: Stats,
}

/// Which connection a request answers: (slot, generation).
type Who = (usize, u64);

/// Where a request came from, so its answer can go back.
#[derive(Clone, Copy)]
enum Origin {
    /// A connection on this loop.
    Local(Who),
    /// A connection on another loop.
    Remote(usize, Who),
    /// An alarm firing (due at `at`), on its `tries`-th retry.
    Alarm { at: i64, tries: u32 },
}

/// Between loops.
enum Msg {
    /// Run this request on an object you own.
    Run { origin: Origin, obj: String, req: String },
    /// Here is the answer for one of your connections.
    Reply { who: Who, reply: (u16, String) },
}

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

impl<E: Engine> Cell<E> {
    /// The host state host calls see: the engine's own, if it keeps one.
    fn host(&mut self) -> &mut HostState {
        if self.vm.host_state().is_some() {
            self.vm.host_state().unwrap()
        } else {
            &mut self.st
        }
    }
}

fn fresh<E: Engine>(eng: &E, prog: &E::Program) -> std::io::Result<Cell<E>> {
    let mut vm = eng.spawn(prog).map_err(std::io::Error::other)?;
    let mut st = HostState::default();
    crate::init(&mut vm, &mut st).map_err(std::io::Error::other)?;
    Ok(Cell { vm, st })
}

/// A request in flight.
struct Task<E: Engine> {
    origin: Origin,
    cell: Cell<E>,
    /// The Durable Object it runs in, if any.
    obj: Option<String>,
}

/// A Durable Object in memory.
struct Object<E: Engine> {
    /// The warm cell, when no request is using it. `None` after a crash.
    cell: Option<Cell<E>>,
    db: Arc<Mutex<Db>>,
    busy: bool,
    /// Requests waiting their turn.
    waiting: VecDeque<(Origin, String)>,
    last_used: Instant,
}

/// A parsed request.
enum Req {
    Cell(String),
    Object(String, String),
    Stats,
}

/// The loop that owns an object.
fn owner(name: &str, threads: usize) -> usize {
    let h = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    (h % threads as u64) as usize
}

pub fn serve<E>(eng: &E, prog: &E::Program, port: u16)
where
    E: Engine + Sync,
    E::Program: Sync,
{
    serve_with(eng, prog, port, Config::from_env())
}

pub fn serve_with<E>(eng: &E, prog: &E::Program, port: u16, cfg: Config)
where
    E: Engine + Sync,
    E::Program: Sync,
{
    std::fs::create_dir_all(&cfg.data).expect("create the data directory");
    let index = Mutex::new(AlarmIndex::open(&cfg.data).expect("open the alarm index"));
    let listener = std::net::TcpListener::bind(("0.0.0.0", port)).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    println!("combd     listening on :{port} with {} event loop(s), data in {}", cfg.threads, cfg.data.display());
    let mut polls = vec![];
    let mut peers = vec![];
    let mut inboxes = vec![];
    for _ in 0..cfg.threads {
        let poll = Poll::new().expect("poll");
        let waker = Arc::new(Waker::new(poll.registry(), WAKER).expect("waker"));
        let (tx, rx) = channel();
        polls.push(poll);
        peers.push((tx, waker));
        inboxes.push(rx);
    }
    let sh = Shared { cfg, peers, index, stats: Stats::default() };
    let sh = &sh;
    std::thread::scope(|sc| {
        for (me, (poll, inbox)) in polls.into_iter().zip(inboxes).enumerate() {
            let l = listener.try_clone().expect("clone listener");
            sc.spawn(move || {
                let mut lp = Loop {
                    eng,
                    prog,
                    sh,
                    me,
                    inbox,
                    poll,
                    pool: vec![],
                    tasks: VecDeque::new(),
                    objects: HashMap::new(),
                    alarms: BinaryHeap::new(),
                    tries: HashMap::new(),
                    conns: vec![],
                    free: vec![],
                    next_gen: 0,
                    last_sweep: Instant::now(),
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
    sh: &'a Shared,
    me: usize,
    inbox: Receiver<Msg>,
    poll: Poll,
    pool: Vec<Cell<E>>,
    tasks: VecDeque<Task<E>>,
    objects: HashMap<String, Object<E>>,
    /// Alarms of objects this loop owns: (due, name). Entries can be stale;
    /// the object's storage is checked when one fires.
    alarms: BinaryHeap<Reverse<(i64, String)>>,
    /// Retries so far, for alarms that failed.
    tries: HashMap<String, u32>,
    conns: Vec<Option<Conn>>,
    free: Vec<usize>,
    next_gen: u64,
    last_sweep: Instant,
}

impl<E: Engine> Loop<'_, E> {
    fn run(&mut self, mut listener: TcpListener) -> std::io::Result<()> {
        let mut events = Events::with_capacity(1024);
        self.poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
        self.pool.push(fresh(self.eng, self.prog)?);
        // Alarms from before a restart.
        let threads = self.sh.cfg.threads;
        for (name, at) in self.sh.index.lock().unwrap().all().unwrap_or_default() {
            if owner(&name, threads) == self.me {
                self.alarms.push(Reverse((at, name)));
            }
        }
        let mut touched: Vec<usize> = vec![];
        while !self.sh.cfg.stop.load(Relaxed) {
            // Never block while requests are waiting to run.
            let timeout = if self.tasks.is_empty() {
                let next = self.alarms.peek().map(|Reverse((at, _))| Duration::from_millis((at - now_ms()).max(0) as u64));
                next.map_or(TICK, |n| n.min(TICK))
            } else {
                Duration::ZERO
            };
            self.poll.poll(&mut events, Some(timeout))?;
            touched.clear();
            for ev in events.iter() {
                match ev.token() {
                    LISTENER => self.accept(&mut listener)?,
                    WAKER => self.read_inbox()?,
                    Token(id) => {
                        let Some(c) = self.conns.get_mut(id).and_then(Option::as_mut) else { continue };
                        if ev.is_readable() && read_all(c) {
                            // The peer is done sending: answer what's buffered, then close.
                            c.close_after = true;
                        }
                        touched.push(id);
                    }
                }
            }
            // Start the next request on every connection that has one ready.
            for &id in &touched {
                self.start_next(id)?;
                self.settle(id)?;
            }
            self.fire_alarms();
            if self.last_sweep.elapsed() >= TICK {
                self.sweep();
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
                let origin = t.origin;
                let reply = self.finish_task(t, answer);
                self.deliver(origin, reply)?;
            }
        }
        Ok(())
    }

    fn accept(&mut self, listener: &mut TcpListener) -> std::io::Result<()> {
        loop {
            match listener.accept() {
                Ok((mut s, _)) => {
                    s.set_nodelay(true)?;
                    let id = self.free.pop().unwrap_or_else(|| {
                        self.conns.push(None);
                        self.conns.len() - 1
                    });
                    self.poll.registry().register(&mut s, Token(id), Interest::READABLE)?;
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

    fn read_inbox(&mut self) -> std::io::Result<()> {
        while let Ok(m) = self.inbox.try_recv() {
            match m {
                Msg::Run { origin, obj, req } => self.submit(origin, obj, req),
                Msg::Reply { who, reply } => self.deliver(Origin::Local(who), reply)?,
            }
        }
        Ok(())
    }

    /// Send an answer back where its request came from.
    fn deliver(&mut self, origin: Origin, reply: (u16, String)) -> std::io::Result<()> {
        match origin {
            Origin::Local(who) => {
                if self.conn(who).is_none() {
                    return Ok(());
                }
                self.queue_reply(who, reply);
                self.start_next(who.0)?;
                self.settle(who.0)
            }
            Origin::Remote(l, who) => {
                let (tx, waker) = &self.sh.peers[l];
                let _ = tx.send(Msg::Reply { who, reply });
                waker.wake()
            }
            Origin::Alarm { .. } => Ok(()), // `finish_task` already handled it
        }
    }

    /// A request is done: put its cell back (or drop it after a crash), close
    /// its object's transaction, and say what to answer.
    fn finish_task(&mut self, t: Task<E>, answer: Result<String, String>) -> (u16, String) {
        let Task { mut cell, obj, origin } = t;
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
        o.last_used = Instant::now();
        let dirty = std::mem::take(&mut cell.host().alarm_dirty) || matches!(origin, Origin::Alarm { .. });
        let mut ok = false;
        let mut new_alarm = None;
        let reply = {
            let db = o.db.lock().unwrap();
            match answer {
                Ok(a) => {
                    let alarm = if dirty { db.alarm() } else { None };
                    // A new alarm goes in the index before the commit, so it can't be lost.
                    if let Some(at) = alarm {
                        if let Err(e) = self.sh.index.lock().unwrap().set(&name, Some(at)) {
                            eprintln!("object {name:?}: alarm index: {e}");
                        }
                    }
                    // The output gate: the answer goes out only once its writes are on disk.
                    match db.commit() {
                        Ok(()) => {
                            o.cell = Some(cell);
                            ok = true;
                            new_alarm = alarm;
                            split_answer(&a)
                        }
                        Err(e) => {
                            // Memory and disk no longer agree, so the cell goes too.
                            eprintln!("object {name:?}: commit failed: {e}");
                            db.rollback();
                            (500, "{\"error\":\"storage\"}".into())
                        }
                    }
                }
                Err(trap) => {
                    eprintln!("object {name:?} trapped: {trap}");
                    db.rollback();
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            }
        };
        if ok && dirty {
            match new_alarm {
                Some(at) => self.alarms.push(Reverse((at, name.clone()))),
                // A cleared alarm leaves the index only after the commit.
                None => {
                    let _ = self.sh.index.lock().unwrap().set(&name, None);
                }
            }
        }
        if let Origin::Alarm { tries, .. } = origin {
            self.alarm_done(&name, ok, tries);
        }
        self.next_waiting(&name);
        reply
    }

    /// After an alarm ran: forget its retries, or schedule the next one.
    fn alarm_done(&mut self, name: &str, ok: bool, tries: u32) {
        if ok {
            self.sh.stats.alarms_fired.fetch_add(1, Relaxed);
            self.tries.remove(name);
            return;
        }
        self.sh.stats.alarms_failed.fetch_add(1, Relaxed);
        if tries + 1 < ALARM_TRIES {
            self.tries.insert(name.to_string(), tries + 1);
            let at = now_ms() + (ALARM_BACKOFF_MS << tries);
            self.alarms.push(Reverse((at, name.to_string())));
            return;
        }
        // Out of tries: drop the alarm, as Cloudflare does.
        eprintln!("object {name:?}: alarm failed {ALARM_TRIES} times, dropping it");
        self.tries.remove(name);
        if let Some(o) = self.objects.get(name) {
            let db = o.db.lock().unwrap();
            if db.begin().is_ok() && db.set_alarm(None).is_ok() && db.commit().is_ok() {
                let _ = self.sh.index.lock().unwrap().set(name, None);
            } else {
                db.rollback();
            }
        }
    }

    /// Run every alarm that's due.
    fn fire_alarms(&mut self) {
        let now = now_ms();
        while self.alarms.peek().is_some_and(|Reverse((at, _))| *at <= now) {
            let Reverse((at, name)) = self.alarms.pop().unwrap();
            let tries = self.tries.get(&name).copied().unwrap_or(0);
            self.submit(Origin::Alarm { at, tries }, name, format!("do ALARM / {tries}"));
        }
    }

    /// Drop objects from memory that have been idle too long, or the least
    /// recently used ones past the limit.
    fn sweep(&mut self) {
        self.last_sweep = Instant::now();
        let idle = self.sh.cfg.idle;
        let free = |o: &Object<E>| !o.busy && o.waiting.is_empty();
        let mut gone: Vec<String> = self.objects.iter().filter(|(_, o)| free(o) && o.last_used.elapsed() >= idle).map(|(n, _)| n.clone()).collect();
        let over = self.objects.len().saturating_sub(gone.len()).saturating_sub(self.sh.cfg.max_objects);
        if over > 0 {
            let mut lru: Vec<(&String, Instant)> =
                self.objects.iter().filter(|(n, o)| free(o) && !gone.contains(n)).map(|(n, o)| (n, o.last_used)).collect();
            lru.sort_by_key(|(_, t)| *t);
            gone.extend(lru.into_iter().take(over).map(|(n, _)| n.clone()));
        }
        for name in gone {
            self.objects.remove(&name);
            self.sh.stats.objects_live.fetch_sub(1, Relaxed);
            self.sh.stats.evictions.fetch_add(1, Relaxed);
        }
    }

    /// Start the next request queued on an object, skipping closed connections.
    fn next_waiting(&mut self, name: &str) {
        while let Some((origin, req)) = self.objects.get_mut(name).and_then(|o| o.waiting.pop_front()) {
            if let Origin::Local(who) = origin {
                if self.conn(who).is_none() {
                    continue;
                }
            }
            match self.run_object(name, origin, req) {
                Ok(true) => return,
                Ok(false) => continue, // a stale alarm: nothing ran
                Err(reply) => {
                    let _ = self.deliver(origin, reply);
                }
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

    /// Flush what's buffered, then close or update write interest.
    fn settle(&mut self, id: usize) -> std::io::Result<()> {
        let Some(c) = self.conns.get_mut(id).and_then(Option::as_mut) else { return Ok(()) };
        let closed = flush(c) || (c.close_after && !c.busy && c.wpos == c.wbuf.len());
        if closed {
            if let Some(mut c) = self.conns.get_mut(id).and_then(Option::take) {
                let _ = self.poll.registry().deregister(&mut c.s);
                self.free.push(id);
            }
            return Ok(());
        }
        let want = c.wpos < c.wbuf.len();
        if want != c.writing {
            let interest = if want { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
            self.poll.registry().reregister(&mut c.s, Token(id), interest)?;
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
        match req {
            Req::Stats => {
                let s = &self.sh.stats;
                let body = format!(
                    "{{\"objects_live\":{},\"objects_opened\":{},\"evictions\":{},\"alarms_fired\":{},\"alarms_failed\":{},\"forwarded\":{}}}",
                    s.objects_live.load(Relaxed),
                    s.objects_opened.load(Relaxed),
                    s.evictions.load(Relaxed),
                    s.alarms_fired.load(Relaxed),
                    s.alarms_failed.load(Relaxed),
                    s.forwarded.load(Relaxed)
                );
                self.queue_reply(who, (200, body));
            }
            Req::Cell(cell_req) => {
                let mut cell = match self.pool.pop() {
                    Some(cell) => cell,
                    None => fresh(self.eng, self.prog)?,
                };
                if let Err(trap) = crate::start(&mut cell.vm, &cell_req, &mut cell.st) {
                    eprintln!("cell trapped starting a request: {trap}");
                    self.queue_reply(who, (500, String::new()));
                    return Ok(());
                }
                self.tasks.push_back(Task { origin: Origin::Local(who), cell, obj: None });
            }
            Req::Object(name, cell_req) => {
                let o = owner(&name, self.sh.cfg.threads);
                if o == self.me {
                    self.submit(Origin::Local(who), name, cell_req);
                } else {
                    // Another loop owns it: hand the request over.
                    self.sh.stats.forwarded.fetch_add(1, Relaxed);
                    let (tx, waker) = &self.sh.peers[o];
                    let _ = tx.send(Msg::Run { origin: Origin::Remote(self.me, who), obj: name, req: cell_req });
                    waker.wake()?;
                }
            }
        }
        Ok(())
    }

    /// Run a request on an object this loop owns, now or after the ones ahead of it.
    fn submit(&mut self, origin: Origin, name: String, req: String) {
        if !self.objects.contains_key(&name) {
            let db = match Db::open(&store::path_for(&self.sh.cfg.data, &name)) {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("object {name:?}: can't open storage: {e}");
                    let _ = self.deliver(origin, (500, "{\"error\":\"storage\"}".into()));
                    return;
                }
            };
            self.sh.stats.objects_live.fetch_add(1, Relaxed);
            self.sh.stats.objects_opened.fetch_add(1, Relaxed);
            let o = Object { cell: None, db: Arc::new(Mutex::new(db)), busy: false, waiting: VecDeque::new(), last_used: Instant::now() };
            self.objects.insert(name.clone(), o);
        }
        let o = self.objects.get_mut(&name).unwrap();
        if o.busy {
            o.waiting.push_back((origin, req));
            return;
        }
        if let Err(reply) = self.run_object(&name, origin, req) {
            let _ = self.deliver(origin, reply);
        }
    }

    /// Begin one request on an object: its warm cell (or a fresh one), inside
    /// a transaction. `Ok(false)`: nothing to run (a stale alarm).
    fn run_object(&mut self, name: &str, origin: Origin, req: String) -> Result<bool, (u16, String)> {
        let (eng, prog) = (self.eng, self.prog);
        let o = self.objects.get_mut(name).unwrap();
        o.last_used = Instant::now();
        {
            let db = o.db.lock().unwrap();
            if let Err(e) = db.begin() {
                eprintln!("object {name:?}: begin failed: {e}");
                return Err((500, "{\"error\":\"storage\"}".into()));
            }
            if let Origin::Alarm { at, .. } = origin {
                // The index and the heap are hints: storage says if it's still due.
                match db.alarm() {
                    Some(t) if t <= now_ms() => {
                        let _ = db.set_alarm(None); // the handler can set a new one
                    }
                    t => {
                        db.rollback();
                        if let Some(t) = t.filter(|t| *t != at) {
                            self.alarms.push(Reverse((t, name.to_string())));
                        }
                        return Ok(false);
                    }
                }
            }
        }
        let mut cell = match o.cell.take() {
            Some(c) => c,
            None => match fresh(eng, prog) {
                Ok(mut c) => {
                    c.host().db = Some(o.db.clone());
                    c
                }
                Err(e) => {
                    o.db.lock().unwrap().rollback();
                    return Err((500, format!("{{\"error\":{:?}}}", e.to_string())));
                }
            },
        };
        if let Err(trap) = crate::start(&mut cell.vm, &req, &mut cell.st) {
            eprintln!("object {name:?} trapped starting a request: {trap}");
            o.db.lock().unwrap().rollback();
            return Err((500, String::new()));
        }
        o.busy = true;
        self.tasks.push_back(Task { origin, cell, obj: Some(name.to_string()) });
        Ok(true)
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
    let line = |kind: &str, path: &str| if body.is_empty() { format!("{kind} {method} {path}") } else { format!("{kind} {method} {path} {body}") };
    let r = if path == "/_combd/stats" {
        Req::Stats
    } else if let Some(rest) = path.strip_prefix("/do/") {
        // `/do/<name>/<rest>`: a Durable Object request.
        let (name, sub) = rest.split_once('/').map_or((rest, "/".to_string()), |(n, s)| (n, format!("/{s}")));
        if name.is_empty() || name.len() > MAX_NAME {
            return Err(());
        }
        Req::Object(name.to_string(), line("do", &sub))
    } else {
        Req::Cell(line("api", path))
    };
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
