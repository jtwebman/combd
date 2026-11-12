//! What a node's cell loops share beside the loops themselves: the fsync
//! pool, which makes objects' commits durable (the local output gate), the
//! local alarm index, and the cells' counters. Both stay with the cells' SQLite files, so when
//! the cells move to a child process (plan.md, the second wall), this is
//! the child's. Settings stay in `http::Config` until then: the child will
//! get its own over the socket.

use crate::app::{App, ForCells, Sleep};
use crate::engine::{CellVm, Engine, Outcome};
use crate::host::{now_ms, AsyncOp, EvAction, HostState, WsOut};
use crate::http::{frame, json_err, owner, session_sock, split_answer, unframe, ws_accepted, Origin, ALARM_BACKOFF_MS, ALARM_TRIES, MAX_EV_TICKETS, ROUND, SLICE_TIME, SQUEEZED_IDLE, TICK};
use crate::line::{Asked, Back, CallTo, DbAsk, Down, End, Up};
use mio::{Events, Interest, Poll, Token};
use std::sync::mpsc::Receiver;
use crate::own::{self, Act};
use crate::store::{self, AlarmIndex, Db};
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, VecDeque};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The cells' loop's tokens: the line, and the waker fsyncs ring.
const LINE: Token = Token(0);
pub const WAKE: Token = Token(1);
/// A round's slices between sending what they answered (`round_some`).
const LINE_BATCH: usize = 32;

/// Threads that run fsyncs, so the event loops never wait on the disk.
const SYNC_THREADS: usize = 16;

/// The cells' counts, as numbers, for the node's stats page (`Up::Stats`):
/// each as `Stats` names it, and in this order on the line.
macro_rules! counts {
    ($($f:ident),* $(,)?) => {
        #[derive(Clone, Debug, Default, PartialEq)]
        pub struct Counts {
            $(pub $f: u64,)*
            /// Per process: the time its cells ran (us) and their slices.
            pub procs: Vec<(String, u64, u64, crate::engine::Kind)>,
        }

        impl Stats {
            pub fn counts(&self) -> Counts {
                let procs = self.procs.lock().unwrap().iter().map(|(k, &(us, slices, e))| (k.clone(), us, slices, e)).collect();
                Counts { $($f: self.$f.load(Relaxed),)* procs }
            }
        }

        impl Counts {
            pub fn numbers(&self) -> Vec<u64> {
                vec![$(self.$f),*]
            }

            /// `None` unless there's one number for each count.
            pub fn from_numbers(n: &[u64], procs: Vec<(String, u64, u64, crate::engine::Kind)>) -> Option<Counts> {
                let mut n = n.iter().copied();
                let c = Counts { $($f: n.next()?,)* procs };
                n.next().is_none().then_some(c)
            }
        }
    };
}

counts!(objects_live, objects_opened, evictions, alarms_fired, alarms_failed, commits, fsyncs, checkpoints, fsyncs_slow, fsync_max_us, sessions_live, sessions_hibernated, sessions_woken, sessions_ready, sessions_waited, mailbox_refused, batched, ev_instances, ev_requests);

/// Make an object's commits up to `gen` durable, then tell loop `back`.
pub struct SyncJob {
    pub file: Arc<std::fs::File>,
    pub db_path: PathBuf,
    /// Checkpoint after the fsync. The object is paused (no request runs on
    /// it), so the checkpoint copies the whole WAL and the WAL can restart.
    pub ckpt: bool,
    pub obj: String,
    pub gen: u64,
    pub back: usize,
    /// A new object's folders: fsynced too, or a power cut could lose its file.
    pub dirs: Vec<PathBuf>,
}

/// What the fsync pool tells a loop.
#[derive(Debug, PartialEq)]
pub enum Done {
    /// The object's commits up to `gen` are on disk (or, `!ok`, failed to be).
    Synced { obj: String, gen: u64, ok: bool },
    /// Its checkpoint finished; it can run requests again.
    Checkpointed { obj: String },
}

pub struct Cells {
    pub sync: Mutex<Sender<SyncJob>>,
    pub index: Mutex<AlarmIndex>,
    pub stats: Arc<Stats>,
}

/// The cells' counters for `/_combd/stats` (the node keeps the rest).
#[derive(Default)]
pub struct Stats {
    pub objects_live: AtomicU64,
    pub objects_opened: AtomicU64,
    pub evictions: AtomicU64,
    pub alarms_fired: AtomicU64,
    pub alarms_failed: AtomicU64,
    /// Commits that wrote something, and the fsyncs that made them durable.
    pub commits: AtomicU64,
    pub fsyncs: AtomicU64,
    pub checkpoints: AtomicU64,
    /// fsync times: slow ones (10 ms or more), and the slowest, in us.
    pub fsyncs_slow: AtomicU64,
    pub fsync_max_us: AtomicU64,
    /// Socket processes' sessions, how many of them are hibernating, and
    /// how many times one has woken.
    pub sessions_live: AtomicU64,
    pub sessions_hibernated: AtomicU64,
    pub sessions_woken: AtomicU64,
    /// Sleeping sessions with work, waiting for a cell under `max_awake`;
    /// and how many times one has had to.
    pub sessions_ready: AtomicU64,
    pub sessions_waited: AtomicU64,
    /// Requests refused because their object's or session's mailbox was full.
    pub mailbox_refused: AtomicU64,
    /// Requests that ran in a batch (`handle_batch`).
    pub batched: AtomicU64,
    /// Event cells: instances started, and requests they took.
    pub ev_instances: AtomicU64,
    pub ev_requests: AtomicU64,
    /// Per process (`org/app/proc`): the time its cells ran, in us, host
    /// calls included, and the slices they ran in. Each loop adds its own
    /// every 250 ms.
    pub procs: Mutex<BTreeMap<String, (u64, u64, crate::engine::Kind)>>,
}

impl Cells {
    /// The alarm index in `data`, and the fsync pool: each fsync waits
    /// `delay` first, and its result goes to `tell` (and the loop it names).
    pub fn start(data: &Path, delay: Duration, tell: impl Fn(usize, Done) + Clone + Send + 'static) -> Cells {
        let index = Mutex::new(AlarmIndex::open(data).expect("open the alarm index"));
        let (sync_tx, sync_rx) = channel::<SyncJob>();
        let sync_rx = Arc::new(Mutex::new(sync_rx));
        let no_fsync = std::env::var_os("COMBD_EXPERIMENT_NO_FSYNC").is_some();
        let stats = Arc::new(Stats::default());
        for _ in 0..SYNC_THREADS {
            let (rx, tell, st) = (sync_rx.clone(), tell.clone(), stats.clone());
            std::thread::Builder::new().name("fsync".into()).spawn(move || loop {
                let Ok(job) = rx.lock().unwrap().recv() else { return };
                // EXPERIMENT ONLY, never ship: COMBD_EXPERIMENT_NO_FSYNC measures
                // what the fsync costs. It breaks the output gate's promise.
                std::thread::sleep(delay);
                let t = Instant::now();
                let ok = no_fsync || (job.file.sync_data().is_ok() && job.dirs.iter().all(|d| std::fs::File::open(d).and_then(|f| f.sync_all()).is_ok()));
                let us = t.elapsed().as_micros() as u64;
                st.fsync_max_us.fetch_max(us, Relaxed);
                if us >= 10_000 {
                    st.fsyncs_slow.fetch_add(1, Relaxed);
                }
                // Release the answers first; the checkpoint doesn't hold them up.
                tell(job.back, Done::Synced { obj: job.obj.clone(), gen: job.gen, ok });
                // Keep the WAL small, here instead of on the event loop.
                if job.ckpt {
                    if ok {
                        match store::checkpoint(&job.db_path) {
                            Ok(()) => {
                                st.checkpoints.fetch_add(1, Relaxed);
                            }
                            Err(e) => eprintln!("object {:?}: checkpoint: {e}", job.obj),
                        }
                    }
                    tell(job.back, Done::Checkpointed { obj: job.obj });
                }
            }).expect("fsync thread");
        }
        Cells { sync: Mutex::new(sync_tx), index, stats }
    }
}

/// What the cells know of the node's settings (`http::Config`), given when
/// they start.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub data: PathBuf,
    /// The node's event loops (an object's loop: `http::owner`).
    pub threads: usize,
    pub idle: Duration,
    pub max_awake: usize,
    pub max_objects: usize,
    /// With no manifest: the node's own env vars, for its one program.
    pub environ: Arc<Vec<Vec<u8>>>,
    /// Fleet mode: this node's id.
    pub node: Option<u32>,
}

/// What the cells run: each of the `App`'s programs, loaded (only this side
/// loads programs), and each app's env vars, opened secrets in. Made from
/// the `App` at each deploy, and swapped in with it.
pub struct Code<P> {
    /// By `App::progs` index.
    pub progs: Vec<Arc<P>>,
    /// By `App::tenants` index.
    pub environ: Vec<Arc<Vec<Vec<u8>>>>,
}

impl<P> Code<P> {
    pub fn empty() -> Code<P> {
        Code { progs: vec![], environ: vec![] }
    }

    /// For `app`, a later `App` than this was made for: its new programs
    /// (an `App` only grows, so the old ones are still right), and every
    /// app's env vars.
    pub fn next(&self, app: &crate::app::App, get: impl Fn(&crate::app::Prog) -> Result<Arc<P>, String>) -> Result<Code<P>, String> {
        let mut progs = self.progs.clone();
        for p in &app.progs[progs.len().min(app.progs.len())..] {
            progs.push(get(p).map_err(|e| format!("{}: {e}", p.what))?);
        }
        Ok(Code { progs, environ: app.tenants.iter().map(|t| t.environ.clone()).collect() })
    }
}

/// The programs the cells have loaded, by their bytes and how they run:
/// each loaded once, whichever loop's deploy (or check) needs it first.
/// ponytail: kept until the cells restart, as `App` keeps retired programs.
pub struct Progs<P>(Mutex<HashMap<([u8; 32], Option<crate::engine::Kind>), Arc<P>>>);

impl<P> Progs<P> {
    pub fn new() -> Progs<P> {
        Progs(Mutex::new(HashMap::new()))
    }

    /// `prog`, loaded by `eng` if it isn't yet.
    pub fn get<E: Engine<Program = P>>(&self, eng: &E, prog: &crate::app::Prog) -> Result<Arc<P>, String> {
        use sha2::Digest;
        let key = (sha2::Sha256::digest(&prog.wasm).into(), prog.kind);
        if let Some(p) = self.0.lock().unwrap().get(&key) {
            return Ok(p.clone());
        }
        // Loaded outside the lock: another loop may load it too, once.
        let p = Arc::new(prog.kind.map_or_else(|| eng.load(&prog.wasm), |k| eng.load_as(&prog.wasm, k))?);
        Ok(self.0.lock().unwrap().entry(key).or_insert(p).clone())
    }
}

/// What a cell wrote (stdout, stderr) in the request that trapped, for the
/// log: a panic's message, from a guest that prints one (combd-sdk does).
fn said<E: Engine>(cell: &mut Cell<E>) -> String {
    let out = &cell.host().out;
    if out.is_empty() {
        return String::new();
    }
    let tail = String::from_utf8_lossy(&out[out.len().saturating_sub(500)..]).trim().replace('\n', " | ");
    format!(" ({tail})")
}

/// An event cell: one instance serving many requests at once, like a Node
/// process. It runs one callback at a time; requests interleave only where
/// the cell waits (an async call), which it chose.
struct EvInst<E: Engine> {
    gen: u64,
    /// Its process.
    proc: usize,
    /// `None` while a callback runs.
    cell: Option<Cell<E>>,
    reqs: HashMap<u64, EvReq>,
    /// Async calls out: ticket -> request.
    tickets: HashMap<u64, u64>,
    queue: VecDeque<EvCall>,
    /// The request whose event the running callback handles, if any.
    running: Option<u64>,
}

struct EvReq {
    origin: Origin,
    responded: bool,
    /// Async calls started and events not yet handled.
    open: u32,
    handles: Vec<u64>,
    start: Instant,
}

enum EvCall {
    Request(u64, String),
    Event { ticket: u64, req: u64, payload: Vec<u8> },
}

/// An event cell request gets this long to answer, and this long in all
/// (work after answering included: waitUntil).
const EV_ANSWER: Duration = Duration::from_secs(30);

const EV_TOTAL: Duration = Duration::from_secs(60);

/// A warm cell, reused across requests like a Worker instance.
struct Cell<E: Engine> {
    vm: E::Vm,
    st: HostState,
    /// Its process (`App::procs`).
    proc: usize,
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

fn fresh<E: Engine>(eng: &E, app: &App, code: &Code<E::Program>, proc: usize, environ: &Arc<Vec<Vec<u8>>>) -> std::io::Result<Cell<E>> {
    let mut vm = eng.spawn(&code.progs[app.procs[proc].prog]).map_err(std::io::Error::other)?;
    if let Some(pages) = app.procs[proc].memory_pages {
        vm.cap_memory(pages).map_err(std::io::Error::other)?;
    }
    let mut st = HostState { environ: environ.clone(), ..HostState::default() };
    // An engine that runs host calls itself (compiled) has its own state:
    // its env vars go there too, or it sees none.
    if let Some(h) = vm.host_state() {
        h.environ = environ.clone();
    }
    crate::init(&mut vm, &mut st).map_err(std::io::Error::other)?;
    Ok(Cell { vm, st, proc })
}

/// A cell again, from what `CellVm::freeze` kept. It doesn't run the
/// module's initializer: that would undo the state it just got back.
fn thawed<E: Engine>(eng: &E, app: &App, code: &Code<E::Program>, proc: usize, environ: &Arc<Vec<Vec<u8>>>, frozen: &[u8]) -> std::io::Result<Cell<E>> {
    let mut vm = eng.thaw(&code.progs[app.procs[proc].prog], frozen).map_err(std::io::Error::other)?;
    if let Some(pages) = app.procs[proc].memory_pages {
        vm.cap_memory(pages).map_err(std::io::Error::other)?;
    }
    if let Some(h) = vm.host_state() {
        h.environ = environ.clone();
    }
    Ok(Cell { vm, st: HostState { environ: environ.clone(), ..HostState::default() }, proc })
}

/// A request in flight.
struct Task<E: Engine> {
    origin: Origin,
    cell: Cell<E>,
    /// The Durable Object it runs in, if any.
    obj: Option<String>,
    /// A WebSocket handshake (it joins the object's list if the answer is
    /// 101), and gone sockets this request takes off the list.
    ws_open: Option<u64>,
    ws_drop: Vec<u64>,
    /// Postgres handles a stateless request opened (they close with it).
    pg: Vec<u64>,
    /// An event cell's callback: its instance.
    ev: Option<usize>,
    /// A batch (`handle_batch`): the requests after the first, in order.
    more: Vec<Origin>,
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
    /// Commits that wrote (numbered), and how many of them are on disk.
    committed: u64,
    synced: u64,
    syncing: bool,
    /// A new object's folders, fsynced with its first commit.
    new_dirs: Vec<PathBuf>,
    /// Answers waiting for an fsync: (the commit they need, origin, answer,
    /// and the WebSocket sends that wait with it).
    held: Vec<(u64, Origin, (u16, String), Vec<WsOut>)>,
    /// Paused for a checkpoint: requests wait until it's done.
    checkpointing: bool,
    /// Its WebSocket list (`_combd_sockets`), read once it's needed.
    socks: Option<BTreeSet<u64>>,
    /// Postgres handles it holds (pinned connections), and when one was last used.
    pg: Vec<u64>,
    pg_used: Instant,
}

impl<E: Engine> Object<E> {
    fn new(db: Db) -> Object<E> {
        Object {
            new_dirs: vec![],
            cell: None,
            db: Arc::new(Mutex::new(db)),
            busy: false,
            waiting: VecDeque::new(),
            last_used: Instant::now(),
            committed: 0,
            synced: 0,
            syncing: false,
            held: vec![],
            checkpointing: false,
            socks: None,
            pg: vec![],
            pg_used: Instant::now(),
        }
    }
}

/// A socket process's instance: one per WebSocket, on the loop that holds
/// the connection. In memory only: no bucket, no storage, no output gate.
struct Session<E: Engine> {
    sock: u64,
    proc: usize,
    /// `None`: not started, or hibernating: then `frozen` is what its cell
    /// changed (`hibernate = "auto"`), or `state` what the app kept.
    cell: Option<Cell<E>>,
    frozen: Option<Vec<u8>>,
    state: Option<Vec<u8>>,
    busy: bool,
    /// The event it's running (`WSMSG`, `HIBERNATE`, ...).
    running: String,
    waiting: VecDeque<(Origin, String)>,
    last_used: Instant,
    /// In the loop's `ready` line.
    ready: bool,
}

/// A session's mailbox while its loop is behind (`max_awake`).
const BEHIND_MAILBOX: usize = 8;

/// What the host asks a session that's idle past `hibernate_ms`.
const HIBERNATE: &str = "do HIBERNATE /";

/// A burst leaves a pool as big as it was: keep only what's been used in
/// the last `idle` (the newest stays, warm for the next request).
fn trim_idle<T>(pool: &mut Vec<(T, Instant)>, idle: Duration) {
    let stale = pool.iter().take(pool.len().saturating_sub(1)).take_while(|(_, at)| at.elapsed() >= idle).count();
    pool.drain(..stale);
}

/// One event loop's cells: objects, sessions, workers, and their storage.
/// They see the node only through the line (`Down` in, `Up` out).
pub(crate) struct CellLoop<'a, E: Engine> {
    /// The run queue's round: slices left in it, and each org's share
    /// (`None`: one org, no limit), and what each org has used of it.
    round: (usize, Option<usize>),
    round_used: HashMap<String, usize>,
    /// The node's memory pressure and disk, as told (`Down::Health`).
    pressure: u64,
    disk_low: bool,
    eng: &'a E,
    /// What they know of the node's settings.
    set: &'a Settings,
    /// What this node's cells share across loops (`Cells`), and the
    /// programs they've loaded.
    cells: &'a Cells,
    progs: &'a Progs<E::Program>,
    /// The app as of the last deploy (`Down::Deploy`).
    app: Arc<App>,
    /// `app`'s programs, loaded, and its apps' env vars.
    code: Arc<Code<E::Program>>,
    me: usize,
    /// Warm worker cells per process, and when each was put back (the
    /// newest last). The sweep trims ones idle too long (`trim_idle`).
    pools: Vec<Vec<(Cell<E>, Instant)>>,
    tasks: VecDeque<Task<E>>,
    objects: HashMap<String, Object<E>>,
    /// Alarms of objects this loop owns: (due, name). Entries can be stale;
    /// the object's storage is checked when one fires.
    alarms: BinaryHeap<Reverse<(i64, String)>>,
    /// Retries so far, for alarms that failed.
    tries: HashMap<String, u32>,
    last_sweep: Instant,
    /// This loop is behind (`note_behind`).
    behind: bool,
    /// The objects the node's been told hold a database session or a beat
    /// (`Up::Anchor`).
    anchors: std::collections::HashSet<String>,
    /// The sockets this loop holds (`Down::SockOpen`).
    socks_open: std::collections::HashSet<u64>,
    /// Socket processes' sessions on this loop, by name.
    sessions: HashMap<String, Session<E>>,
    /// When this loop last added its cells' time to the stats.
    last_publish: Instant,
    /// Per process (by index): the ns its cells ran and the slices, since
    /// they last went into `Stats::procs`.
    ran: Vec<(u64, u64)>,
    /// Sleeping sessions with work, in order, for when there's room
    /// (`max_awake`).
    ready: VecDeque<String>,
    /// Sockets that are gone, per object, until a committed request has
    /// taken them off the object's list (`_combd_sockets`).
    ws_gone: HashMap<String, BTreeSet<u64>>,
    /// Lost sockets we've told their object about already.
    ws_lost: std::collections::HashSet<u64>,
    /// Cells waiting on the database, by park number; and the Postgres
    /// handles cells on this loop hold.
    parked: HashMap<u64, (Task<E>, bool)>,
    next_park: u64,
    /// The cells': each handle's object (it outlives requests), or `None`
    /// for a stateless request's (it closes when the request ends), and
    /// how long it may sit unused.
    pg_of: HashMap<u64, (Option<String>, Duration)>,
    /// Event cells: which processes are (their module exports
    /// `on_request`), their instances, and their timers (due, instance,
    /// generation, ticket).
    ev_mode: Vec<bool>,
    ev: Vec<Option<EvInst<E>>>,
    ev_gen: u64,
    next_ev_req: u64,
    ev_timers: BinaryHeap<Reverse<(Instant, usize, u64, u64)>>,
    /// The cells': tokens of waiting requests whose connection is gone.
    cancelled: std::collections::HashSet<u64>,
    ups: VecDeque<Up>,
    deferred: VecDeque<(CallTo, Vec<u8>)>,
    /// Objects' beats: name -> (every, next, count).
    ticks: HashMap<String, (Duration, Instant, u64)>,
}

impl<'a, E: Engine> CellLoop<'a, E> {
    /// A loop's cells, with nothing deployed yet (`Down::Deploy`).
    pub(crate) fn new(eng: &'a E, set: &'a Settings, cells: &'a Cells, progs: &'a Progs<E::Program>, me: usize) -> CellLoop<'a, E> {
        CellLoop {
            round: (0, None),
            round_used: HashMap::new(),
            pressure: 0,
            disk_low: false,
            eng,
            set,
            cells,
            progs,
            app: Arc::new(App::empty()),
            code: Arc::new(Code::empty()),
            me,
            pools: vec![],
            tasks: VecDeque::new(),
            objects: HashMap::new(),
            alarms: BinaryHeap::new(),
            tries: HashMap::new(),
            last_sweep: Instant::now(),
            anchors: Default::default(),
            socks_open: Default::default(),
            sessions: HashMap::new(),
            last_publish: Instant::now(),
            ran: vec![],
            ready: VecDeque::new(),
            ws_gone: HashMap::new(),
            ws_lost: Default::default(),
            parked: HashMap::new(),
            next_park: 0,
            pg_of: HashMap::new(),
            ev_mode: vec![],
            ev: vec![],
            ev_gen: 0,
            next_ev_req: 0,
            ev_timers: BinaryHeap::new(),
            cancelled: Default::default(),
            ups: VecDeque::new(),
            deferred: VecDeque::new(),
            ticks: HashMap::new(),
            behind: false,
        }
    }
}

impl<E: Engine> CellLoop<'_, E> {
    /// The cells send to the node.
    fn up(&mut self, m: Up) -> std::io::Result<()> {
        self.ups.push_back(m);
        Ok(())
    }

    /// The cells' own loop: the node's messages in from `line`, fsyncs done
    /// from `fsyncs` (`waker`'s token), their turn, their messages out. It
    /// ends when the node closes its end.
    pub(crate) fn run(&mut self, mut line: End, mut poll: Poll, fsyncs: Receiver<Done>) -> std::io::Result<()> {
        poll.registry().register(&mut line.s, LINE, Interest::READABLE)?;
        let mut events = Events::with_capacity(64);
        let mut writable = false;
        self.start_cells()?;
        loop {
            let timeout = if self.busy() { Duration::ZERO } else { self.next_due().map_or(TICK, |n| n.min(TICK)) };
            match poll.poll(&mut events, Some(timeout)) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                r => r?,
            }
            while let Ok(done) = fsyncs.try_recv() {
                self.fsynced(done);
            }
            if events.iter().any(|e| e.token() == LINE && (e.is_readable() || e.is_read_closed())) {
                if !line.fill(events.iter().any(|e| e.token() == LINE && e.is_read_closed()))? {
                    return Ok(());
                }
                while let Some(m) = line.next(Down::decode)? {
                    self.on_down(m);
                }
            }
            self.chores();
            self.round_begin();
            while self.round_some(LINE_BATCH)? {
                self.send_ups(&mut line)?;
            }
            self.round_end();
            self.send_ups(&mut line)?;
            // What the socket didn't take goes when it can.
            if line.pending() != writable {
                writable = line.pending();
                let interest = if writable { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
                poll.registry().reregister(&mut line.s, LINE, interest)?;
            }
        }
    }

    /// What the cells have queued for the node, onto the line.
    fn send_ups(&mut self, line: &mut End) -> std::io::Result<()> {
        for m in self.take_ups() {
            line.send(|b| m.encode(b));
        }
        line.flush().map(|_| ())
    }

    /// An fsync (or the checkpoint after it) is done.
    fn fsynced(&mut self, done: Done) {
        match done {
            Done::Synced { obj, gen, ok } => {
                let _ = self.synced(&obj, gen, ok);
            }
            Done::Checkpointed { obj } => {
                if let Some(o) = self.objects.get_mut(&obj) {
                    o.checkpointing = false;
                    o.db.lock().unwrap().checkpointed();
                }
                self.next_waiting(&obj);
                self.kick_sync(&obj);
            }
        }
    }

    /// The cells start: a warm cell for each process, and the alarms from
    /// before a restart.
    fn start_cells(&mut self) -> std::io::Result<()> {
        self.warm(0)?;
        let threads = self.set.threads;
        for (name, at) in self.cells.index.lock().unwrap().all().unwrap_or_default() {
            if owner(&name, threads) == self.me {
                self.alarms.push(Reverse((at, name)));
            }
        }
        Ok(())
    }

    /// When the cells next have something to do: an alarm, a timer, a beat.
    fn next_due(&self) -> Option<Duration> {
        let next = self.alarms.peek().map(|Reverse((at, _))| Duration::from_millis((at - now_ms()).max(0) as u64));
        let timer = self.ev_timers.peek().map(|Reverse((at, ..))| *at).into_iter().chain(self.ticks.values().map(|t| t.1)).min().map(|at| at.saturating_duration_since(Instant::now()));
        match (next, timer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Work waiting: requests to run, or answers deferred to the next turn.
    fn busy(&self) -> bool {
        !self.tasks.is_empty() || !self.deferred.is_empty()
    }

    fn take_ups(&mut self) -> VecDeque<Up> {
        std::mem::take(&mut self.ups)
    }

    /// The cells' timers: local alarms (one node; a fleet's come from its
    /// index, as requests), and the sweep.
    fn chores(&mut self) {
        let fleet = self.set.node.is_some();
        if !fleet {
            self.fire_alarms();
        }
        if self.last_sweep.elapsed() >= TICK {
            if fleet {
                self.fleet_sweep();
            } else {
                self.sweep();
            }
        }
    }

    /// An event cell's answer for after its callback (`round` hands it on).
    fn defer(&mut self, to: CallTo, payload: Vec<u8>) {
        self.deferred.push_back((to, payload));
    }

    /// The cells' turn: answers deferred to now, event cells' timers,
    /// objects' beats, one round of the run queue, and sessions that may wake.
    fn round_begin(&mut self) {
        for (to, payload) in std::mem::take(&mut self.deferred) {
            self.on_down(Down::Back { to, back: Back::Answer(payload) });
        }
        self.ev_tick();
        self.fire_ticks();
        // One round of the run queue: every request gets one slice, but
        // each org gets an equal share of the round, so one busy tenant
        // can't crowd out the rest. An org that needs less than its share
        // gets all it needs; what it leaves goes unspent this round, and
        // the round is shorter (the poll comes sooner).
        self.round = (self.tasks.len(), self.round_share());
        self.round_used.clear();
    }

    /// Up to `n` more of this round's slices; `true` while it has more.
    /// The loop hands their answers over in between, so they're written
    /// while still in cache: all of a round's at once (1000 pages of 8 KB)
    /// cost million's page 16% at 1000 connections.
    fn round_some(&mut self, n: usize) -> std::io::Result<bool> {
        let (left, share) = self.round;
        let app = self.app.clone();
        for _ in 0..n.min(left) {
            self.round.0 -= 1;
            let mut t = self.tasks.pop_front().unwrap();
            if let Some(share) = share {
                let org = &app.tenants[app.procs[t.cell.proc].tenant].org;
                let n = match self.round_used.get_mut(org) {
                    Some(n) => n,
                    None => self.round_used.entry(org.clone()).or_default(),
                };
                if *n >= share {
                    self.tasks.push_back(t);
                    continue;
                }
                *n += 1;
            }
            // A host call that's done at once (a send, the clock, a query
            // that didn't wait) costs the cell no round: it goes on, in
            // its slice's time. Before, each was a trip round the loop
            // (14 us on an M4), so a chunk telling 160 watchers took 160.
            let (proc, started) = (t.cell.proc, Instant::now());
            let outcome = loop {
                let o = t.cell.vm.run(crate::SLICE);
                let Outcome::HostCall { import, args } = o else { break Ok(o) };
                let r = crate::serve(&mut t.cell.vm, import, &args, &mut t.cell.st);
                if r.is_err() || t.cell.st.pending.is_some() || started.elapsed() >= SLICE_TIME {
                    break Err(r);
                }
            };
            self.ran(proc, started, 1);
            // Out of time after a host call, or one that waits: as before.
            let outcome = match outcome {
                Ok(o) => o,
                Err(Ok(())) => {
                    match t.cell.st.pending.take() {
                        Some(op) => self.park(t, op, true),
                        None => self.tasks.push_back(t),
                    }
                    continue;
                }
                Err(Err(trap)) => Outcome::Trap(trap),
            };
            let answer = match outcome {
                Outcome::Yield => {
                    // A compiled cell paused inside a database call: park it.
                    match t.cell.vm.db_pending() {
                        Some(op) => self.park(t, op, false),
                        None => self.tasks.push_back(t),
                    }
                    continue;
                }
                Outcome::HostCall { .. } => unreachable!("served above"),
                // An event cell's callback answers by `respond`, not by return.
                Outcome::Done(_) if t.ev.is_some() => Ok(String::new()),
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
            if t.ev.is_some() {
                self.ev_finish(t, answer.is_ok());
                continue;
            }
            let (origin, name) = (t.origin, t.obj.clone());
            if let Some((reply, ws)) = self.finish_task(t, answer) {
                self.release(name.as_deref(), origin, reply, ws)?;
            }
        }
        Ok(self.round.0 > 0)
    }

    fn round_end(&mut self) {
        self.wake_ready();
    }

    /// An answer is ready (after the output gate): send it, then do the
    /// WebSocket sends that waited with it.
    fn release(&mut self, name: Option<&str>, origin: Origin, reply: (u16, String), ws: Vec<WsOut>) -> std::io::Result<()> {
        self.answer(origin, reply)?;
        match name {
            Some(n) if !ws.is_empty() => self.up(Up::Ws { name: n.to_string(), out: ws }),
            _ => Ok(()),
        }
    }

    /// The cells' answer to a request: a token's goes up to the node, and
    /// their own kinds (an alarm's, a beat's, a send's, an event cell's, the
    /// core's: `finish_task` told it) are done with here.
    fn answer(&mut self, origin: Origin, reply: (u16, String)) -> std::io::Result<()> {
        match origin {
            Origin::Token(token) => {
                self.cancelled.remove(&token);
                self.up(Up::Answer { token, reply })
            }
            Origin::Alarm { .. } | Origin::Fleet(_) | Origin::Ev | Origin::Discard | Origin::Tick => Ok(()),
            // Never: the node hands the cells tokens.
            _ => {
                #[cfg(test)]
                panic!("a node's origin reached the cells");
                #[cfg(not(test))]
                Ok(())
            }
        }
    }

    /// A request for an event cell: the least busy instance with room takes
    /// it, or a new one starts.
    fn ev_request(&mut self, origin: Origin, proc: usize, req: String) -> std::io::Result<()> {
        let cap = self.app.procs[proc].max_concurrent;
        let pick = self.ev.iter().enumerate().filter_map(|(i, e)| e.as_ref().filter(|e| e.proc == proc && e.reqs.len() < cap).map(|e| (e.reqs.len(), i))).min().map(|(_, i)| i);
        let i = match pick {
            Some(i) => i,
            None => {
                self.ev_gen += 1;
                let inst = EvInst { gen: self.ev_gen, proc, cell: Some(fresh(self.eng, &self.app, &self.code, proc, &self.environ_for(proc))?), reqs: HashMap::new(), tickets: HashMap::new(), queue: VecDeque::new(), running: None };
                self.cells.stats.ev_instances.fetch_add(1, Relaxed);
                match self.ev.iter().position(Option::is_none) {
                    Some(i) => {
                        self.ev[i] = Some(inst);
                        i
                    }
                    None => {
                        self.ev.push(Some(inst));
                        self.ev.len() - 1
                    }
                }
            }
        };
        self.next_ev_req += 1;
        let id = self.next_ev_req;
        self.cells.stats.ev_requests.fetch_add(1, Relaxed);
        let inst = self.ev[i].as_mut().unwrap();
        inst.reqs.insert(id, EvReq { origin, responded: false, open: 0, handles: vec![], start: Instant::now() });
        inst.queue.push_back(EvCall::Request(id, req));
        self.ev_kick(i);
        Ok(())
    }

    /// Start the instance's next callback, if it's free.
    fn ev_kick(&mut self, i: usize) {
        let Some(inst) = self.ev.get_mut(i).and_then(Option::as_mut) else { return };
        if inst.cell.is_none() {
            return;
        }
        let Some(call) = inst.queue.pop_front() else { return };
        let mut cell = inst.cell.take().unwrap();
        let ids: Vec<u64> = inst.reqs.keys().copied().collect();
        let h = cell.host();
        h.ev_reqs = ids;
        h.ev_out.clear();
        h.ev_bytes = 0;
        // A fresh host-time budget per callback, as a classic cell gets one
        // per request: an instance lives long, and it would run out.
        h.host_time = Default::default();
        cell.st.host_time = Default::default();
        let started = match call {
            EvCall::Request(id, req) => {
                inst.running = None;
                (|| -> Result<(), String> {
                    cell.vm.begin("alloc", &[req.len() as u64])?;
                    let ptr = crate::finish(&mut cell.vm, u32::MAX, &mut cell.st)?[0] as u32;
                    if !cell.vm.write(ptr, req.as_bytes()) {
                        return Err("bad alloc pointer".into());
                    }
                    cell.vm.begin("on_request", &[id, ptr as u64, req.len() as u64])
                })()
            }
            EvCall::Event { ticket, req, payload } => {
                inst.running = Some(req);
                let n = payload.len() as u64;
                cell.host().ev_payload = payload;
                cell.vm.begin("on_event", &[ticket, n])
            }
        };
        let t = Task { origin: Origin::Ev, cell, obj: None, ws_open: None, ws_drop: vec![], pg: vec![], ev: Some(i), more: vec![] };
        match started {
            Ok(()) => self.tasks.push_back(t),
            Err(e) => {
                eprintln!("event cell: can't start a callback: {e}");
                self.ev_finish(t, false);
            }
        }
    }

    /// A callback returned: do what it asked, finish requests that are done,
    /// and start the next callback. A trap takes the instance down.
    fn ev_finish(&mut self, t: Task<E>, ok: bool) {
        let i = t.ev.unwrap();
        // Handles from blocking pg_open last one callback.
        for h in t.pg {
            self.pg_close(h);
        }
        let mut cell = t.cell;
        let Some(inst) = self.ev.get_mut(i).and_then(Option::as_mut) else { return };
        if !ok {
            return self.ev_crash(i);
        }
        let actions = std::mem::take(&mut cell.host().ev_out);
        // Open tickets, capped per instance: each callback may ask for
        // 10,000 things, timers included, and each timer's callback for
        // 10,000 more. Past the cap it's broken, or an attack: it crashes.
        let asks = actions.iter().filter(|a| matches!(a, EvAction::Async { .. })).count();
        if inst.tickets.len() + asks > MAX_EV_TICKETS {
            eprintln!("event cell: over {MAX_EV_TICKETS} things in flight: crashed");
            return self.ev_crash(i);
        }
        inst.cell = Some(cell);
        if let Some(q) = inst.running.take().and_then(|r| inst.reqs.get_mut(&r)) {
            q.open = q.open.saturating_sub(1);
        }
        let (gen, mut replies, mut ops, mut closes, mut sends) = (inst.gen, vec![], vec![], vec![], vec![]);
        let from = inst.proc;
        for a in actions {
            match a {
                EvAction::Respond(req, text) => {
                    if let Some(q) = inst.reqs.get_mut(&req).filter(|q| !q.responded) {
                        q.responded = true;
                        replies.push((q.origin, split_answer(&text)));
                    }
                }
                EvAction::Async { req, ticket, op } => {
                    if let Some(q) = inst.reqs.get_mut(&req) {
                        q.open += 1;
                        inst.tickets.insert(ticket, req);
                        ops.push((req, ticket, op));
                    }
                }
                EvAction::Send(m) => sends.push(m),
                EvAction::PgClose(req, h) => {
                    if let Some(q) = inst.reqs.get_mut(&req) {
                        if q.handles.contains(&h) {
                            q.handles.retain(|&x| x != h);
                            closes.push(h);
                        }
                    }
                }
            }
        }
        for (origin, reply) in replies {
            let _ = self.answer(origin, reply);
        }
        for h in closes {
            self.pg_close(h);
        }
        for (req, ticket, op) in ops {
            self.ev_start(i, gen, req, ticket, op);
        }
        for msg in sends {
            let _ = self.up(Up::Call { to: None, from, msg });
        }
        self.ev_reap(i);
        self.ev_kick(i);
    }

    /// Start an async call for an event cell's request.
    fn ev_start(&mut self, i: usize, gen: u64, req: u64, ticket: u64, op: AsyncOp) {
        let to = CallTo::Ev(i, gen, ticket);
        let proc = self.ev[i].as_ref().map_or(0, |e| e.proc);
        let ask = match op {
            AsyncOp::Timer(ms) => return self.ev_timers.push(Reverse((Instant::now() + Duration::from_millis(ms), i, gen, ticket))),
            AsyncOp::Call(msg) => {
                let _ = self.up(Up::Call { to: Some(to), from: proc, msg });
                return;
            }
            AsyncOp::PgOpen(r) => DbAsk::Open(r),
            AsyncOp::PgQuery(0, r) => DbAsk::Query(r),
            AsyncOp::PgQuery(h, r) => {
                if !self.ev[i].as_ref().and_then(|e| e.reqs.get(&req)).is_some_and(|q| q.handles.contains(&h)) {
                    return self.defer(to, json_err(&format!("pg_query_async: no such handle {h}")));
                }
                DbAsk::QueryOn(h, r)
            }
        };
        let _ = self.up(Up::Db { to, proc, ask });
    }

    /// An async call is back: queue its event for the instance.
    fn ev_done(&mut self, i: usize, gen: u64, ticket: u64, done: Back) {
        let live = self.ev.get(i).and_then(Option::as_ref).is_some_and(|e| e.gen == gen);
        let req = if live { self.ev[i].as_mut().unwrap().tickets.remove(&ticket) } else { None };
        let alive_req = req.filter(|r| self.ev[i].as_ref().is_some_and(|e| e.reqs.contains_key(r)));
        let payload = match done {
            Back::Answer(a) => a,
            Back::Failed(e) => json_err(&e),
            Back::Handle(h, idle) => {
                let Some(r) = alive_req else {
                    // Its request (or instance) is gone: give it straight back.
                    let _ = self.up(Up::DbClose(h));
                    return;
                };
                self.pg_of.insert(h, (None, idle));
                self.ev[i].as_mut().unwrap().reqs.get_mut(&r).unwrap().handles.push(h);
                serde_json::json!({ "handle": h }).to_string().into_bytes()
            }
        };
        let Some(r) = alive_req else { return };
        self.ev[i].as_mut().unwrap().queue.push_back(EvCall::Event { ticket, req: r, payload });
        self.ev_kick(i);
    }

    /// Requests that are answered and have nothing open are done; their
    /// handles close.
    fn ev_reap(&mut self, i: usize) {
        let Some(inst) = self.ev.get_mut(i).and_then(Option::as_mut) else { return };
        let done: Vec<u64> = inst.reqs.iter().filter(|(_, q)| q.responded && q.open == 0).map(|(&id, _)| id).collect();
        let mut closes = vec![];
        for id in done {
            closes.extend(inst.reqs.remove(&id).unwrap().handles);
        }
        for h in closes {
            self.pg_close(h);
        }
        // An idle instance goes, unless it's the first: that one stays warm.
        // ponytail: drops at once; keep it for `idle` if start-up cost shows.
        let inst = self.ev[i].as_ref().unwrap();
        let first = self.ev.iter().position(|e| e.as_ref().is_some_and(|e| e.proc == inst.proc));
        let retired = self.app.procs[inst.proc].retired;
        if (first != Some(i) || retired) && inst.reqs.is_empty() && inst.queue.is_empty() && inst.cell.is_some() {
            self.ev[i] = None;
        }
    }

    /// An instance trapped: its unanswered requests fail, its handles close,
    /// and it's gone (a fresh one starts when needed). Let it crash.
    fn ev_crash(&mut self, i: usize) {
        let Some(inst) = self.ev.get_mut(i).and_then(Option::take) else { return };
        eprintln!("event cell crashed, with {} requests in flight", inst.reqs.len());
        for q in inst.reqs.into_values() {
            if !q.responded {
                let _ = self.answer(q.origin, (500, "{\"error\":\"the cell crashed\"}".into()));
            }
            for h in q.handles {
                self.pg_close(h);
            }
        }
    }

    /// Timers that are due, and requests past their time.
    fn ev_tick(&mut self) {
        let now = Instant::now();
        while self.ev_timers.peek().is_some_and(|Reverse((at, ..))| *at <= now) {
            let Reverse((_, i, gen, ticket)) = self.ev_timers.pop().unwrap();
            self.ev_done(i, gen, ticket, Back::Answer(vec![]));
        }
        for i in 0..self.ev.len() {
            let Some(inst) = self.ev[i].as_mut() else { continue };
            let mut late = vec![];
            let mut over = vec![];
            for (&id, q) in inst.reqs.iter_mut() {
                if !q.responded && q.start.elapsed() > EV_ANSWER {
                    q.responded = true;
                    late.push(q.origin);
                }
                if q.start.elapsed() > EV_TOTAL {
                    over.push(id);
                }
            }
            let mut closes = vec![];
            for id in over {
                closes.extend(inst.reqs.remove(&id).unwrap().handles);
                inst.tickets.retain(|_, r| *r != id);
            }
            for origin in late {
                let _ = self.answer(origin, (504, "{\"error\":\"the cell didn't answer in time\"}".into()));
            }
            for h in closes {
                self.pg_close(h);
            }
        }
    }

    /// An object leaves memory: its database handles close (rolling back and
    /// releasing locks; Postgres would too once the connection went).
    fn remove_object(&mut self, name: &str) -> Option<Object<E>> {
        self.ticks.remove(name);
        let o = self.objects.remove(name)?;
        for &h in &o.pg {
            self.pg_close(h);
        }
        self.anchor(name);
        Some(o)
    }

    /// Close a database handle: its connection is reset and goes back.
    fn pg_close(&mut self, h: u64) {
        let Some((obj, _)) = self.pg_of.remove(&h) else { return };
        if let Some(n) = obj {
            if let Some(o) = self.objects.get_mut(&n) {
                o.pg.retain(|&x| x != h);
            }
            self.anchor(&n);
        }
        let _ = self.up(Up::DbClose(h));
    }

    /// A cell waits on a database call: hand it to the pool and park the
    /// cell (paused, no CPU) until the answer is back. `interp`: it paused at
    /// a host call (the interpreter); else on its fiber (compiled).
    fn park(&mut self, t: Task<E>, op: crate::host::DbOp, interp: bool) {
        use crate::host::DbOp;
        // A call to another process: nothing to do with the database.
        if let DbOp::Call(msg) = op {
            self.next_park += 1;
            let park = self.next_park;
            let from = t.cell.proc;
            self.parked.insert(park, (t, interp));
            let _ = self.up(Up::Call { to: Some(CallTo::Park(park)), from, msg });
            return;
        }
        let fail = |m: String| json_err(&m);
        // May this cell use handle `h`? Its object's, or its request's.
        let owns = |me: &Self, t: &Task<E>, h: u64| match &t.obj {
            Some(n) => me.objects.get(n).is_some_and(|o| o.pg.contains(&h)),
            None => t.pg.contains(&h),
        };
        let ask = match op {
            DbOp::Call(_) => unreachable!("handled above"),
            DbOp::Open(_) if t.obj.as_ref().is_some_and(|n| self.sessions.contains_key(n)) => {
                return self.db_resume(t, interp, 0, fail("pg_open: a socket process uses pooled queries (pg_query with no handle)".into()));
            }
            DbOp::Open(req) => DbAsk::Open(req),
            DbOp::Query(0, req) => DbAsk::Query(req),
            DbOp::Query(h, req) => {
                if !owns(self, &t, h) {
                    return self.db_resume(t, interp, 0, fail(format!("pg_query: no such handle {h}")));
                }
                if let Some(o) = self.pg_of.get(&h).and_then(|(n, _)| n.clone()).and_then(|n| self.objects.get_mut(&n)) {
                    o.pg_used = Instant::now();
                }
                DbAsk::QueryOn(h, req)
            }
            DbOp::Close(h) => {
                let ok = owns(self, &t, h);
                if ok {
                    self.pg_close(h);
                }
                let mut t = t;
                t.pg.retain(|&x| x != h);
                return self.db_resume(t, interp, (!ok) as u64, vec![]);
            }
        };
        self.next_park += 1;
        let park = self.next_park;
        let proc = t.cell.proc;
        self.parked.insert(park, (t, interp));
        let _ = self.up(Up::Db { to: CallTo::Park(park), proc, ask });
    }

    /// A database call is back: hand the answer to its cell, and run it again.
    fn db_done(&mut self, park: u64, done: Back) {
        let Some((mut t, interp)) = self.parked.remove(&park) else {
            if let Back::Handle(h, _) = done {
                let _ = self.up(Up::DbClose(h));
            }
            return;
        };
        let (value, out) = match done {
            Back::Answer(a) => (a.len() as u64, a),
            Back::Failed(e) => (0, json_err(&e)),
            Back::Handle(h, idle) => {
                self.pg_of.insert(h, (t.obj.clone(), idle));
                match &t.obj {
                    Some(n) => {
                        if let Some(o) = self.objects.get_mut(n) {
                            o.pg.push(h);
                            o.pg_used = Instant::now();
                        }
                        let n = n.clone();
                        self.anchor(&n);
                    }
                    None => t.pg.push(h),
                }
                (h, vec![])
            }
        };
        self.db_resume(t, interp, value, out);
    }

    fn db_resume(&mut self, mut t: Task<E>, interp: bool, value: u64, out: Vec<u8>) {
        if interp {
            t.cell.st.pg_out = out;
            let r: &[u64] = if t.cell.st.pending_results == 0 { &[] } else { &[value] };
            t.cell.vm.resume(r);
        } else {
            t.cell.vm.db_done(value, out);
        }
        self.tasks.push_back(t);
    }

    /// A request committed: its handshake joined the socket list, and the
    /// gone sockets it took off are done with.
    fn sockets_committed(&mut self, name: &str, joined: Option<u64>, dropped: &[u64]) {
        let Some(socks) = self.objects.get_mut(name).and_then(|o| o.socks.as_mut()) else { return };
        socks.extend(joined);
        for id in dropped {
            socks.remove(id);
        }
        let socks = socks.clone();
        if let Some(g) = self.ws_gone.get_mut(name) {
            g.retain(|id| socks.contains(id));
            if g.is_empty() {
                self.ws_gone.remove(name);
            }
        }
    }

    /// The cells' side of the line (`line.rs`): what the node tells them.
    fn on_down(&mut self, m: Down) {
        match m {
            Down::SockOpen(sock) => {
                self.socks_open.insert(sock);
            }
            Down::SockGone(sock) => {
                self.socks_open.remove(&sock);
            }
            Down::SockLost { name, sock } => self.socket_lost(&name, sock),
            // Closed by the object: it hears nothing back, and the socket
            // leaves its list with the next request.
            Down::SockClosed { name, sock } => {
                self.ws_gone.entry(name).or_default().insert(sock);
            }
            Down::Back { to: CallTo::Park(park), back } => self.db_done(park, back),
            Down::Back { to: CallTo::Ev(i, gen, ticket), back } => self.ev_done(i, gen, ticket, back),
            Down::Cancel(t) => {
                self.cancelled.insert(t);
            }
            Down::Seal { name, act, image } => self.seal(name, act, image),
            Down::Restore { name, act, segs, image } => self.restore(name, act, segs, image),
            Down::Drop { name } => self.drop_object(&name),
            Down::Health { pressure, disk_low } => (self.pressure, self.disk_low) = (pressure, disk_low),
            Down::Run { origin, name, req, new } => {
                if session_sock(&name).is_some() && self.app.procs[self.app.object_proc(&name)].socket {
                    self.session_submit(origin, name, req);
                } else {
                    self.arrive(origin, name, req, new);
                }
            }
            Down::Stats => {
                let _ = self.up(Up::Stats(self.cells.stats.counts()));
            }
            Down::Deploy(d) => {
                if let Err(e) = self.deploy(d) {
                    eprintln!("combd     a deploy's first cells: {e}");
                }
            }
            Down::Worker { origin, proc, .. } if proc >= self.app.procs.len() => {
                let _ = self.answer(origin, (500, "{\"error\":\"not deployed here\"}".into()));
            }
            Down::Session { proc, .. } if proc >= self.app.procs.len() => {}
            Down::Worker { origin, proc, req } => {
                if let Err(e) = self.run_worker(origin, proc, req) {
                    eprintln!("can't start a cell: {e}");
                    let _ = self.answer(origin, (500, "{\"error\":\"can't start a cell\"}".into()));
                }
            }
            Down::Session { name, sock, proc } => {
                self.sessions.insert(name, Session { sock, proc, cell: None, frozen: None, state: None, busy: false, running: String::new(), waiting: VecDeque::new(), last_used: Instant::now(), ready: false });
                self.cells.stats.sessions_live.fetch_add(1, Relaxed);
            }
        }
    }

    /// A socket in an object's list is gone without a close: tell the
    /// object once (1006), wherever it runs.
    fn socket_lost(&mut self, name: &str, id: u64) {
        // A session ends with its socket: nothing to tell.
        if session_sock(name).is_some() {
            return;
        }
        // ponytail: forgotten wholesale past 64K, not per socket; a socket
        // lost after that may be told of twice (a second close: harmless).
        if self.ws_lost.len() >= 1 << 16 {
            self.ws_lost.clear();
        }
        if self.ws_gone.get(name).is_some_and(|g| g.contains(&id)) || !self.ws_lost.insert(id) {
            return;
        }
        let _ = self.up(Up::Submit { from: Asked::Lost, name: name.to_string(), req: format!("do WSCLOSE /{id} 1006") });
    }

    /// An event for a session: now, or after the ones ahead of it. One
    /// that's gone gets 410; only a socket starts one.
    fn session_submit(&mut self, origin: Origin, name: String, req: String) {
        let Some(s) = self.sessions.get_mut(&name) else {
            let _ = self.answer(origin, (410, "{\"error\":\"that session is gone\"}".into()));
            return;
        };
        // `on_close = "drop"`: the socket's gone, so the session is, unwoken.
        // Everyone leaving at once (a deploy, a network blip) is then cheap.
        if req.starts_with("do WSCLOSE ") && !self.app.procs[s.proc].wake_on_close {
            self.remove_session(&name);
            let _ = self.answer(origin, (200, "{}".into()));
            return;
        }
        // Behind (sessions waiting to wake): a short mailbox, so 40,000 of
        // them can't hold a node's memory in queued messages.
        let mailbox = if self.ready.is_empty() { self.app.procs[s.proc].mailbox } else { self.app.procs[s.proc].mailbox.min(BEHIND_MAILBOX) };
        if s.waiting.len() >= mailbox && !req.starts_with("do WSCLOSE ") {
            return self.overloaded(origin);
        }
        s.waiting.push_back((origin, req));
        self.session_next(&name);
    }

    /// Start a session's next event. A hibernating one wakes first, with
    /// what it kept (`do WAKE / <len>`, the bytes by `ws_msg`).
    fn session_next(&mut self, name: &str) {
        loop {
            let Some(s) = self.sessions.get_mut(name) else { return };
            if s.busy {
                return;
            }
            // Asleep, with work: it wakes only if there's room for one more
            // cell at work; else it waits its turn (`wake_ready`).
            if s.cell.is_none() && (s.frozen.is_some() || s.state.is_some()) && !s.waiting.is_empty() && self.tasks.len() + self.parked.len() >= self.set.max_awake {
                if !s.ready {
                    s.ready = true;
                    self.ready.push_back(name.to_string());
                    self.cells.stats.sessions_ready.fetch_add(1, Relaxed);
                    self.cells.stats.sessions_waited.fetch_add(1, Relaxed);
                    self.note_behind();
                }
                return;
            }
            let Some((origin, req)) = s.waiting.pop_front() else { return };
            let (origin, req, ws_in) = if req == HIBERNATE {
                if s.cell.is_none() || !s.waiting.is_empty() {
                    continue; // asleep already, or more to do first
                }
                (origin, req, vec![])
            } else if let Some(frozen) = s.frozen.take() {
                // Frozen by the host: the same cell again, where it was.
                self.cells.stats.sessions_hibernated.fetch_sub(1, Relaxed);
                self.cells.stats.sessions_woken.fetch_add(1, Relaxed);
                let proc = s.proc;
                match thawed(self.eng, &self.app, &self.code, proc, &self.environ_for(proc), &frozen) {
                    Ok(cell) => {
                        self.sessions.get_mut(name).unwrap().cell = Some(cell);
                        (origin, req, vec![])
                    }
                    Err(e) => {
                        eprintln!("session {name:?}: can't thaw: {e}");
                        let _ = self.answer(origin, (500, "{\"error\":\"the session was lost\"}".into()));
                        let sock = self.sessions.get(name).map(|s| s.sock);
                        self.remove_session(name);
                        if let Some(sock) = sock {
                            let _ = self.up(Up::Ws { name: name.to_string(), out: vec![WsOut::Close(sock, 1011)] });
                        }
                        return;
                    }
                }
            } else if let Some(state) = s.state.take() {
                s.waiting.push_front((origin, req));
                self.cells.stats.sessions_hibernated.fetch_sub(1, Relaxed);
                self.cells.stats.sessions_woken.fetch_add(1, Relaxed);
                (Origin::Discard, format!("do WAKE / {}", state.len()), state)
            } else {
                (origin, req, vec![])
            };
            if let Err(reply) = self.run_session(name, origin, req, ws_in) {
                // Let it crash: the session goes, and so does its socket.
                let _ = self.answer(origin, reply);
                let sock = self.sessions.get(name).map(|s| s.sock);
                self.remove_session(name);
                if let Some(sock) = sock {
                    let _ = self.up(Up::Ws { name: name.to_string(), out: vec![WsOut::Close(sock, 1011)] });
                }
                return;
            }
            return;
        }
    }

    /// Begin one event on a session: its warm cell, or a fresh one.
    fn run_session(&mut self, name: &str, origin: Origin, mut req: String, mut ws_in: Vec<u8>) -> Result<(), (u16, String)> {
        let (eng, app, code) = (self.eng, self.app.clone(), self.code.clone());
        let proc = app.object_proc(name);
        if let Some(rest) = req.strip_prefix("do WSBIN ") {
            let (sock, b64) = rest.split_once(' ').unwrap_or((rest, ""));
            ws_in = crate::ws::unbase64(b64).unwrap_or_default();
            req = format!("do WSBIN {sock} {}", ws_in.len());
        }
        let environ = self.environ_for(proc);
        let s = self.sessions.get_mut(name).unwrap();
        let open = self.socks_open.contains(&s.sock);
        let mut cell = match s.cell.take() {
            Some(c) => c,
            None => fresh(eng, &app, &code, proc, &environ).map_err(|e| (500, format!("{{\"error\":{:?}}}", e.to_string())))?,
        };
        cell.host().ws_ids = if open { vec![s.sock] } else { vec![] };
        cell.host().ws_out.clear();
        cell.host().out.clear();
        cell.host().ws_in = ws_in;
        if let Err(trap) = crate::start(&mut cell.vm, &req, &mut cell.st) {
            eprintln!("session {name:?} trapped starting an event: {trap}");
            return Err((500, String::new()));
        }
        s.busy = true;
        s.last_used = Instant::now();
        s.running = req.split(' ').nth(1).unwrap_or("").to_string();
        self.tasks.push_back(Task { origin, cell, obj: Some(name.to_string()), ws_open: None, ws_drop: vec![], pg: vec![], ev: None, more: vec![] });
        Ok(())
    }

    /// A session's event is done. No output gate: it has no storage, so its
    /// answer and sends go now. A `HIBERNATE` answered `200 <state>` puts it
    /// to sleep; a crash, a close, or a refused handshake ends it.
    fn session_finish(&mut self, name: &str, mut cell: Cell<E>, answer: Result<String, String>, mut ws: Vec<WsOut>) -> Option<((u16, String), Vec<WsOut>)> {
        let p = &self.app.procs[cell.proc];
        let (hibernate, sleep, state_max) = (p.hibernate, p.sleep, p.state_max);
        let s = self.sessions.get_mut(name)?;
        s.busy = false;
        s.last_used = Instant::now();
        let method = std::mem::take(&mut s.running);
        let (reply, ok) = match answer {
            Ok(a) => (split_answer(&a), true),
            Err(trap) => {
                eprintln!("session {name:?} trapped: {trap}{}", said(&mut cell));
                ((500, format!("{{\"error\":{trap:?}}}")), false)
            }
        };
        let gone = !ok || method == "WSCLOSE" || (method == "WSOPEN" && reply.0 != 101);
        let mut freeze = false;
        if ok && method == "HIBERNATE" && reply.0 == 200 && reply.1.len() <= state_max {
            // Its cell goes; what it kept is all that's left.
            s.state = Some(reply.1.clone().into_bytes());
            self.cells.stats.sessions_hibernated.fetch_add(1, Relaxed);
        } else if ok {
            if method == "HIBERNATE" && reply.0 == 200 {
                eprintln!("session {name:?}: {} bytes of state is over state_kb, so it stays warm", reply.1.len());
            }
            s.cell = Some(cell);
            // `hibernate_ms = 0`, or memory short: asleep after every event.
            // (A burst of new sockets otherwise keeps a cell awake for each
            // until its idle time, or the next sweep: thousands of them,
            // tens of KB each, past a 1 GB node's limit.)
            let short = self.pressure >= 1;
            if (hibernate.is_zero() || short) && !gone && method != "HIBERNATE" && s.waiting.is_empty() && !self.ticks.contains_key(name) {
                match sleep {
                    Sleep::App => s.waiting.push_back((Origin::Discard, HIBERNATE.into())),
                    Sleep::Auto => freeze = true,
                    Sleep::Off => {}
                }
            }
        }
        if !ok && self.socks_open.contains(&s.sock) {
            ws.push(WsOut::Close(s.sock, 1011));
        }
        if gone {
            self.remove_session(name);
        } else if freeze {
            self.freeze_session(name);
        } else {
            self.session_next(name);
        }
        Some((reply, ws))
    }

    /// The host puts an idle session to sleep: it keeps what the cell
    /// changed (up to `state_kb`), and the cell goes. Too big, or an engine
    /// that can't: it stays warm.
    fn freeze_session(&mut self, name: &str) {
        let max = self.sessions.get(name).map_or(0, |s| self.app.procs[s.proc].state_max);
        let Some(s) = self.sessions.get_mut(name).filter(|s| !s.busy && s.waiting.is_empty()) else { return };
        let Some(cell) = s.cell.take() else { return };
        match cell.vm.freeze() {
            Some(f) if f.len() <= max => {
                s.frozen = Some(f);
                self.cells.stats.sessions_hibernated.fetch_add(1, Relaxed);
            }
            _ => s.cell = Some(cell),
        }
    }

    /// A session ends: what waits for it gets 410.
    fn remove_session(&mut self, name: &str) {
        let Some(s) = self.sessions.remove(name) else { return };
        self.cells.stats.sessions_live.fetch_sub(1, Relaxed);
        if s.state.is_some() || s.frozen.is_some() {
            self.cells.stats.sessions_hibernated.fetch_sub(1, Relaxed);
        }
        self.ticks.remove(name);
        for (origin, _) in s.waiting {
            let _ = self.answer(origin, (410, "{\"error\":\"that session is gone\"}".into()));
        }
    }

    /// A request is done: put its cell back (or drop it after a crash), close
    /// its object's transaction, and say what to answer now. `None`: the
    /// answer is held until an fsync covers it.
    ///
    /// A cell that trapped is never used again. Its memory is half an
    /// operation, and the guard-page AOT tier doesn't promise it's exact
    /// at a trap either (LLVM may drop a store around a load that faults).
    fn finish_task(&mut self, t: Task<E>, answer: Result<String, String>) -> Option<((u16, String), Vec<WsOut>)> {
        let Task { mut cell, obj, origin, ws_open, ws_drop, pg, more, .. } = t;
        // A batch: an answer each, in order. A count that's off fails them all.
        let (answer, rest) = if more.is_empty() {
            (answer, vec![])
        } else {
            match answer.and_then(|a| unframe(&a, 1 + more.len())) {
                Ok(mut all) => {
                    let first = all.remove(0);
                    (Ok(first), more.into_iter().zip(all.into_iter().map(Ok)).collect())
                }
                Err(e) => (Err(e.clone()), more.into_iter().map(|o| (o, Err(e.clone()))).collect::<Vec<(Origin, Result<String, String>)>>()),
            }
        };
        // A beat the request asked for, if it succeeded.
        if let (Some(ms), Some(name), true) = (cell.host().tick.take(), &obj, answer.is_ok()) {
            if ms == 0 {
                self.ticks.remove(name);
            } else {
                let every = Duration::from_millis(ms);
                let count = self.ticks.get(name).map_or(0, |t| t.2);
                self.ticks.insert(name.clone(), (every, Instant::now() + every, count));
            }
            self.anchor(name);
        }
        // A stateless request's handles end with it; a crash ends an
        // object's (rolling back, releasing its locks).
        for h in pg {
            self.pg_close(h);
        }
        if answer.is_err() {
            if let Some(name) = &obj {
                let hs = self.objects.get(name).map(|o| o.pg.clone()).unwrap_or_default();
                for h in hs {
                    self.pg_close(h);
                }
            }
        }
        // WebSocket sends go out only if the request succeeds, and only
        // after its writes are on disk (with the answer).
        let ws = if answer.is_ok() { std::mem::take(&mut cell.host().ws_out) } else { vec![] };
        cell.host().ws_out.clear();
        let Some(name) = obj else {
            return Some((match answer {
                Ok(a) => {
                    for out in ws {
                        if let WsOut::Message(msg) = out {
                            let _ = self.up(Up::Call { to: None, from: cell.proc, msg });
                        }
                    }
                    // `fresh`: the next request starts from the module's image.
                    if !self.app.procs[cell.proc].retired && !self.app.procs[cell.proc].fresh {
                        self.pools[cell.proc].push((cell, Instant::now()));
                    }
                    split_answer(&a)
                }
                Err(trap) => {
                    // Let it crash: this cell is dropped; the pool makes a fresh one when needed.
                    eprintln!("cell trapped: {trap}{}", said(&mut cell));
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            }, vec![]));
        };
        // A session's, even if it's gone while this ran (its socket closed,
        // with `on_close = "drop"`): `session_finish` lets that go.
        if self.app.procs[cell.proc].socket {
            return self.session_finish(&name, cell, answer, ws);
        }
        if let Origin::Fleet(req) = origin {
            return self.fleet_finish(name, req, cell, answer, ws, ws_open, ws_drop, rest).map(|r| (r, vec![]));
        }
        let o = self.objects.get_mut(&name).expect("object of a running task");
        o.busy = false;
        o.last_used = Instant::now();
        let dirty = std::mem::take(&mut cell.host().alarm_dirty) || matches!(origin, Origin::Alarm { .. });
        let mut ok = false;
        let mut new_alarm = None;
        let mut wrote = false;
        let mut joined = None;
        let reply = {
            let db = o.db.lock().unwrap();
            match answer {
                Ok(a) if ws_accepted(&a, ws_open).is_some_and(|id| db.update_sockets(Some(id), &[]).is_err()) => {
                    eprintln!("object {name:?}: socket list: can't add");
                    db.rollback();
                    (500, "{\"error\":\"storage\"}".into())
                }
                Ok(a) => {
                    let alarm = if dirty { db.alarm() } else { None };
                    // A new alarm goes in the index before the commit, so it can't be lost.
                    if let Some(at) = alarm {
                        if let Err(e) = self.cells.index.lock().unwrap().set(&name, Some(at)) {
                            eprintln!("object {name:?}: alarm index: {e}");
                        }
                    }
                    match db.commit() {
                        Ok(w) => {
                            o.cell = Some(cell);
                            ok = true;
                            wrote = w;
                            new_alarm = alarm;
                            joined = ws_accepted(&a, ws_open);
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
                    eprintln!("object {name:?} trapped: {trap}{}", said(&mut cell));
                    db.rollback();
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            }
        };
        // The rest of a batch: their own answers if it committed, else the
        // first's failure.
        let rest: Vec<(Origin, (u16, String))> = rest.into_iter().map(|(o, a)| (o, match a { Ok(a) if ok => split_answer(&a), _ => reply.clone() })).collect();
        if ok {
            self.sockets_committed(&name, joined, &ws_drop);
        }
        if ok && dirty {
            match new_alarm {
                Some(at) => self.alarms.push(Reverse((at, name.clone()))),
                // A cleared alarm leaves the index only after the commit.
                None => {
                    let _ = self.cells.index.lock().unwrap().set(&name, None);
                }
            }
        }
        if let Origin::Alarm { tries, .. } = origin {
            self.alarm_done(&name, ok, tries);
        }
        // The output gate: an answer leaves only once every commit up to now
        // is on disk, its own included. Reads wait too, since they may have
        // seen a write that isn't on disk yet.
        let mut out = Some((reply, ws));
        if let Some(o) = self.objects.get_mut(&name) {
            if wrote {
                o.committed += 1;
                self.cells.stats.commits.fetch_add(1, Relaxed);
            }
            if ok && o.committed > o.synced {
                let (reply, ws) = out.take().unwrap();
                o.held.push((o.committed, origin, reply, ws));
                let gen = o.committed;
                o.held.extend(rest.iter().map(|(origin, reply)| (gen, *origin, reply.clone(), vec![])));
                self.kick_sync(&name);
            }
        }
        if out.is_some() {
            for (origin, reply) in rest {
                let _ = self.answer(origin, reply);
            }
        }
        self.next_waiting(&name);
        out
    }

    /// Start an fsync for an object if it has commits that aren't on disk
    /// and none is running.
    fn kick_sync(&mut self, name: &str) {
        let Some(o) = self.objects.get_mut(name) else { return };
        if o.syncing || o.checkpointing {
            return;
        }
        let (file, db_path, wants) = {
            let db = o.db.lock().unwrap();
            if o.committed <= o.synced && !db.wants_checkpoint() {
                return;
            }
            (db.wal_file(), db.path().to_path_buf(), db.wants_checkpoint())
        };
        // Pause the object for a checkpoint only between requests.
        let ckpt = wants && !o.busy;
        if o.committed <= o.synced && !ckpt {
            return;
        }
        let gen = o.committed;
        match file {
            Ok(file) => {
                o.syncing = true;
                o.checkpointing = ckpt;
                self.cells.stats.fsyncs.fetch_add(1, Relaxed);
                let job = SyncJob { file, db_path, ckpt, obj: name.to_string(), gen, back: self.me, dirs: std::mem::take(&mut o.new_dirs) };
                let _ = self.cells.sync.lock().unwrap().send(job);
            }
            Err(e) => {
                eprintln!("object {name:?}: can't open its WAL to fsync: {e}");
                let _ = self.synced(name, gen, false);
            }
        }
    }

    /// An fsync finished: release the answers it covers.
    fn synced(&mut self, name: &str, gen: u64, ok: bool) -> std::io::Result<()> {
        let Some(o) = self.objects.get_mut(name) else { return Ok(()) };
        o.syncing = false;
        let ready: Vec<(Origin, (u16, String), Vec<WsOut>)> = if ok {
            o.synced = o.synced.max(gen);
            let synced = o.synced;
            let (done, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut o.held).into_iter().partition(|(g, ..)| *g <= synced);
            o.held = rest;
            done.into_iter().map(|(_, origin, reply, ws)| (origin, reply, ws)).collect()
        } else {
            // After a failed fsync, nothing in memory can be trusted to be on
            // disk (the "fsyncgate" lesson). Fail what's held, and drop the
            // object so the next request reads what's really there.
            eprintln!("object {name:?}: fsync failed, dropping it");
            let o = self.remove_object(name).unwrap();
            self.cells.stats.objects_live.fetch_sub(1, Relaxed);
            let mut failed: Vec<(Origin, (u16, String), Vec<WsOut>)> =
                o.held.into_iter().map(|(_, origin, ..)| (origin, (500, "{\"error\":\"storage\"}".to_string()), vec![])).collect();
            failed.extend(o.waiting.into_iter().map(|(origin, _)| (origin, (503, "{\"error\":\"storage\"}".to_string()), vec![])));
            failed
        };
        for (origin, reply, ws) in ready {
            self.release(Some(name), origin, reply, ws)?;
        }
        self.kick_sync(name);
        Ok(())
    }

    /// After an alarm ran: forget its retries, or schedule the next one.
    fn alarm_done(&mut self, name: &str, ok: bool, tries: u32) {
        if ok {
            self.cells.stats.alarms_fired.fetch_add(1, Relaxed);
            self.tries.remove(name);
            return;
        }
        self.cells.stats.alarms_failed.fetch_add(1, Relaxed);
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
                let _ = self.cells.index.lock().unwrap().set(name, None);
            } else {
                db.rollback();
            }
            drop(db);
            if let Some(o) = self.objects.get_mut(name) {
                o.committed += 1;
            }
            self.kick_sync(name);
        }
    }

    /// Run every alarm that's due.
    /// Objects' beats that are due. A beat is skipped, not queued, while the
    /// object is busy, so a slow tick never piles up; an object that's left
    /// memory (or this node) has no beat.
    fn fire_ticks(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self.ticks.iter().filter(|(_, t)| t.1 <= now).map(|(n, _)| n.clone()).collect();
        for name in due {
            let Some(o) = self.objects.get(&name) else {
                self.ticks.remove(&name);
                continue;
            };
            let idle = !o.busy && o.waiting.is_empty();
            let t = self.ticks.get_mut(&name).unwrap();
            t.1 = now + t.0;
            if idle {
                t.2 += 1;
                let n = t.2;
                let _ = self.up(Up::Submit { from: Asked::Tick, name, req: format!("do TICK / {n}") });
            }
        }
    }

    fn fire_alarms(&mut self) {
        let now = now_ms();
        while self.alarms.peek().is_some_and(|Reverse((at, _))| *at <= now) {
            let Reverse((at, name)) = self.alarms.pop().unwrap();
            let tries = self.tries.get(&name).copied().unwrap_or(0);
            let _ = self.up(Up::Submit { from: Asked::Alarm { at, tries }, name, req: format!("do ALARM / {tries}") });
        }
    }

    /// Drop objects from memory that have been idle too long, or the least
    /// recently used ones past the limit.
    fn sweep(&mut self) {
        self.last_sweep = Instant::now();
        self.retire_cells();
        let idle = self.idle_limit();
        self.sweep_cells();
        // Database handles keep an object here until they've been idle for
        // their pool's `session_idle_ms`; then they close, and it can leave.
        let handles = &self.pg_of;
        let idle_for = |o: &Object<E>| o.pg.iter().filter_map(|h| handles.get(h)).map(|(_, idle)| *idle).min();
        let stale: Vec<u64> = self.objects.values().filter(|o| !o.busy && idle_for(o).is_some_and(|d| o.pg_used.elapsed() >= d)).flat_map(|o| o.pg.clone()).collect();
        for h in stale {
            self.pg_close(h);
        }
        let free = |o: &Object<E>| !o.busy && o.waiting.is_empty() && !o.syncing && o.held.is_empty() && !o.checkpointing && o.pg.is_empty();
        // A ticking object stays: its beat is its life.
        let ticks = &self.ticks;
        let mut gone: Vec<String> = self.objects.iter().filter(|(n, o)| free(o) && !ticks.contains_key(*n) && o.last_used.elapsed() >= idle).map(|(n, _)| n.clone()).collect();
        let over = self.objects.len().saturating_sub(gone.len()).saturating_sub(self.set.max_objects);
        if over > 0 {
            let mut lru: Vec<(&String, Instant)> =
                self.objects.iter().filter(|(n, o)| free(o) && !gone.contains(n)).map(|(n, o)| (n, o.last_used)).collect();
            lru.sort_by_key(|(_, t)| *t);
            gone.extend(lru.into_iter().take(over).map(|(n, _)| n.clone()));
        }
        for name in gone {
            self.remove_object(&name);
            self.cells.stats.objects_live.fetch_sub(1, Relaxed);
            self.cells.stats.evictions.fetch_add(1, Relaxed);
        }
    }

    /// The cells' side of `full`: tell the node when this loop's sessions
    /// waiting to wake reach `max_awake`, or drop under it.
    fn note_behind(&mut self) {
        let behind = self.ready.len() >= self.set.max_awake;
        if behind != self.behind {
            self.behind = behind;
            let _ = self.up(Up::Behind(behind));
        }
    }

    /// Wake sleeping sessions with work, in order, while there's room.
    fn wake_ready(&mut self) {
        while self.tasks.len() + self.parked.len() < self.set.max_awake {
            let Some(name) = self.ready.pop_front() else { break };
            self.cells.stats.sessions_ready.fetch_sub(1, Relaxed);
            if let Some(s) = self.sessions.get_mut(&name) {
                s.ready = false;
                self.session_next(&name);
            }
        }
        self.note_behind();
    }

    /// How long idle is too long: shorter while memory is short.
    /// How long an idle object stays: shorter while memory is short, but
    /// not a second (letting an object go and bringing it back costs more
    /// than keeping it: on a node, every chunk went and came back in turn).
    fn idle_limit(&self) -> Duration {
        if self.pressure >= 1 { SQUEEZED_IDLE.min(self.set.idle) } else { self.set.idle }
    }

    fn sweep_cells(&mut self) {
        if self.last_publish.elapsed() >= Duration::from_millis(250) {
            self.last_publish = Instant::now();
            self.publish_ran();
        }
        let squeeze = self.pressure >= 1;
        // Warm worker cells are cheap to make again: they go after a second.
        let idle = if squeeze { Duration::from_secs(1).min(self.set.idle) } else { self.set.idle };
        for pool in &mut self.pools {
            trim_idle(pool, idle);
        }
        // Those whose socket is gone (they closed it) go.
        let (procs, ticks, socks) = (&self.app.procs, &self.ticks, &self.socks_open);
        let mut sleepy = vec![];
        let mut ended = vec![];
        for (n, s) in self.sessions.iter().filter(|(_, s)| !s.busy && s.waiting.is_empty()) {
            if !socks.contains(&s.sock) {
                ended.push(n.clone());
            } else if s.cell.is_some() && !ticks.contains_key(n) && (squeeze || s.last_used.elapsed() >= procs[s.proc].hibernate) {
                sleepy.push((n.clone(), procs[s.proc].sleep));
            }
        }
        for n in ended {
            self.remove_session(&n);
        }
        for (n, sleep) in sleepy {
            match sleep {
                Sleep::App => self.session_submit(Origin::Discard, n, HIBERNATE.into()),
                Sleep::Auto => self.freeze_session(&n),
                Sleep::Off => {}
            }
        }
    }

    /// Start the next request queued on an object, skipping closed connections.
    fn next_waiting(&mut self, name: &str) {
        if self.objects.get(name).is_some_and(|o| o.checkpointing || o.busy) {
            return;
        }
        while let Some((origin, req)) = self.objects.get_mut(name).and_then(|o| o.waiting.pop_front()) {
            if let Origin::Token(t) = origin {
                if self.cancelled.remove(&t) {
                    continue;
                }
            }
            // Waiting requests are a batch, if the object takes them so: one
            // run, one transaction, an answer each.
            let (req, more) = self.batch_after(name, origin, req);
            let n = 1 + more.len() as u64;
            match self.run_object(name, origin, req, more.clone()) {
                Ok(true) => {
                    if n > 1 {
                        self.cells.stats.batched.fetch_add(n, Relaxed);
                    }
                    return;
                }
                Ok(false) => {
                    self.fleet_skip(name, origin); // a stale alarm: nothing ran
                    continue;
                }
                Err(reply) => {
                    for o in std::iter::once(origin).chain(more) {
                        let _ = self.answer(o, reply.clone());
                    }
                }
            }
        }
    }

    /// After `first`, the plain requests waiting behind it, up to the
    /// process's `batch`, if its cell takes batches (exports `handle_batch`):
    /// all of them framed, and the others' origins. Else `first` alone.
    /// WebSocket events, alarms, and beats go one at a time.
    fn batch_after(&mut self, name: &str, origin: Origin, first: String) -> (String, Vec<Origin>) {
        let max = self.app.procs[self.app.object_proc(name)].batch;
        let plain = |o: &Origin, r: &str| !matches!(o, Origin::Alarm { .. } | Origin::Tick) && matches!(r.split(' ').nth(1), Some("GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"));
        let Some(o) = self.objects.get_mut(name) else { return (first, vec![]) };
        let takes = max > 1 && plain(&origin, &first) && o.waiting.front().is_some_and(|(o, r)| plain(o, r)) && o.cell.as_mut().is_some_and(|c| c.vm.has_export("handle_batch"));
        if !takes {
            return (first, vec![]);
        }
        let mut reqs = vec![first];
        let mut more = vec![];
        while reqs.len() < max && o.waiting.front().is_some_and(|(o, r)| plain(o, r)) {
            let (o, r) = o.waiting.pop_front().unwrap();
            more.push(o);
            reqs.push(r);
        }
        (frame(&reqs), more)
    }

    /// Connections past their time (`Limits`): a request still arriving
    /// past the read timeout gets 408; a new one that sent nothing, a kept
    /// one idle past keep-alive, or a TLS handshake that drags, just closes.
    /// WebSockets and requests at work are left alone.
    fn ran(&mut self, proc: usize, at: Instant, slices: u64) {
        if self.ran.len() <= proc {
            self.ran.resize(proc + 1, (0, 0));
        }
        let r = &mut self.ran[proc];
        r.0 += at.elapsed().as_nanos() as u64;
        r.1 += slices;
    }

    /// This loop's time per process, into what the node shows (by name, as
    /// a deploy renumbers them).
    fn publish_ran(&mut self) {
        if self.ran.iter().all(|r| r.1 == 0 && r.0 == 0) {
            return;
        }
        let mut procs = self.cells.stats.procs.lock().unwrap();
        for (i, r) in self.ran.iter_mut().enumerate().filter(|(_, r)| r.0 > 0) {
            let Some(p) = self.app.procs.get(i) else { continue };
            let t = &self.app.tenants[p.tenant];
            let e = procs.entry(format!("{}/{}/{}", t.org, t.app, p.name)).or_insert((0, 0, p.engine));
            e.0 += r.0 / 1000;
            e.1 += r.1;
            e.2 = p.engine;
            *r = (0, 0);
        }
    }

    /// The cells' side of `run_cell`.
    fn run_worker(&mut self, origin: Origin, p: usize, req: String) -> std::io::Result<()> {
        if self.ev_mode[p] {
            return self.ev_request(origin, p, req);
        }
        let mut cell = match self.pools[p].pop().map(|(c, _)| c) {
            Some(cell) => cell,
            None => fresh(self.eng, &self.app, &self.code, p, &self.environ_for(p))?,
        };
        cell.host().out.clear();
        if let Err(trap) = crate::start(&mut cell.vm, &req, &mut cell.st) {
            eprintln!("cell trapped starting a request: {trap}");
            return self.answer(origin, (500, String::new()));
        }
        self.tasks.push_back(Task { origin, cell, obj: None, ws_open: None, ws_drop: vec![], pg: vec![], ev: None, more: vec![] });
        Ok(())
    }

    /// A deploy: load its new programs, and take it. Processes it retired
    /// keep what's running and get nothing new; their warm cells go, and an
    /// object's old cell gives way to a fresh one (its storage stays). The
    /// node loaded it before it went live, so one that won't load here is
    /// a bug: said, and left out (its processes' requests get 500).
    fn deploy(&mut self, d: ForCells) -> std::io::Result<()> {
        let Some(app) = App::from_node(&self.app, d) else {
            eprintln!("combd     bug: a deploy that doesn't follow on from the cells' last");
            return Ok(());
        };
        let (eng, progs) = (self.eng, self.progs);
        let code = match self.code.next(&app, |p| progs.get(eng, p)) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("combd     bug: a deploy the node loaded doesn't load in its cells: {e}");
                return Ok(());
            }
        };
        let from = self.app.procs.len();
        (self.app, self.code) = (Arc::new(app), Arc::new(code));
        let app = self.app.clone();
        for (p, pool) in self.pools.iter_mut().enumerate() {
            if app.procs[p].retired {
                pool.clear();
            }
        }
        self.retire_cells();
        self.warm(from)
    }

    /// Objects still on an older version's code move to the new one (their
    /// cell goes; the next request starts one) once they're not in the
    /// middle of something: no request running, no open sockets, no beat.
    /// So a deploy never ends a game in play. ponytail: an object that keeps
    /// sockets open forever stays on its old version until they close.
    fn retire_cells(&mut self) {
        let (app, ticks) = (&self.app, &self.ticks);
        for (name, o) in self.objects.iter_mut() {
            let old = o.cell.as_ref().is_some_and(|c| app.procs[c.proc].retired);
            let mid = o.busy || ticks.contains_key(name) || o.socks.as_ref().is_some_and(|s| !s.is_empty());
            if old && !mid {
                o.cell = None;
            }
        }
    }

    /// New processes (from `from` on): a warm cell each, and whether it's an
    /// event cell.
    fn warm(&mut self, from: usize) -> std::io::Result<()> {
        self.pools.resize_with(self.app.procs.len(), Vec::new);
        self.ev_mode.resize(self.app.procs.len(), false);
        for p in from..self.app.procs.len() {
            if self.app.procs[p].retired {
                continue;
            }
            // A ws process's sessions start their own.
            if self.app.procs[p].socket {
                continue;
            }
            let mut first = fresh(self.eng, &self.app, &self.code, p, &self.environ_for(p))?;
            self.ev_mode[p] = first.vm.has_export("on_request");
            self.pools[p].push((first, Instant::now()));
        }
        Ok(())
    }

    /// Each org's slices this round (`None`: one org, no limit).
    fn round_share(&self) -> Option<usize> {
        if self.app.router.legacy || self.tasks.len() <= ROUND {
            return None;
        }
        let mut orgs: Vec<&str> = self.tasks.iter().map(|t| self.app.tenants[self.app.procs[t.cell.proc].tenant].org.as_str()).collect();
        orgs.sort_unstable();
        orgs.dedup();
        (orgs.len() > 1).then(|| (ROUND / orgs.len()).max(1))
    }

    /// The env vars a process's cells get: its app's (no manifest: the node's).
    /// ... plus the host's own: `COMBD_ORG`, `COMBD_APP`, `COMBD_PROCESS`,
    /// `COMBD_NODE` (an app can't declare `COMBD_` names).
    fn environ_for(&self, proc: usize) -> Arc<Vec<Vec<u8>>> {
        if self.app.router.legacy {
            return self.set.environ.clone();
        }
        let p = &self.app.procs[proc];
        let t = &self.app.tenants[p.tenant];
        let node = self.set.node.unwrap_or(0);
        let mut e = (*self.code.environ[p.tenant]).clone();
        e.extend([format!("COMBD_ORG={}", t.org), format!("COMBD_APP={}", t.app), format!("COMBD_PROCESS={}", p.name), format!("COMBD_NODE={node}")].map(String::into_bytes));
        Arc::new(e)
    }

    fn overloaded(&mut self, origin: Origin) {
        self.cells.stats.mailbox_refused.fetch_add(1, Relaxed);
        let _ = self.answer(origin, (503, "{\"error\":\"overloaded\"}".into()));
    }

    /// The cells' door for an object's request (a session's is
    /// `session_submit`): its mailbox, its storage opened if it isn't yet,
    /// then now or after the ones ahead of it.
    fn arrive(&mut self, origin: Origin, name: String, req: String, new: bool) {
        if new && self.objects.get(&name).is_some_and(|o| o.waiting.len() >= self.app.procs[self.app.object_proc(&name)].mailbox) {
            return match origin {
                // The core's: it ran, refused, and changed nothing.
                Origin::Fleet(req) => {
                    self.cells.stats.mailbox_refused.fetch_add(1, Relaxed);
                    let _ = self.up(Up::Ran { name, req, reply: (503, "{\"error\":\"overloaded\"}".into()), wrote: None, ws: vec![], rest: vec![] });
                }
                o => self.overloaded(o),
            };
        }
        if !self.objects.contains_key(&name) {
            if self.set.node.is_some() {
                return; // dropped while its request waited; the core failed it
            }
            let path = store::path_for(&self.set.data, &name);
            if !path.exists() {
                if self.disk_low {
                    let _ = self.answer(origin, (507, "{\"error\":\"no room for new objects\"}".into()));
                    return;
                }
                if let Err(why) = crate::policy::new_object(&name) {
                    let _ = self.answer(origin, (403, serde_json::json!({ "error": why }).to_string()));
                    return;
                }
            }
            let dirs = if path.exists() { Ok(vec![]) } else { store::make_dirs(&self.set.data, &path) };
            let db = match dirs.and_then(|d| Db::open(&path).map(|db| (db, d)).map_err(std::io::Error::other)) {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("object {name:?}: can't open storage: {e}");
                    let _ = self.answer(origin, (500, "{\"error\":\"storage\"}".into()));
                    return;
                }
            };
            self.cells.stats.objects_live.fetch_add(1, Relaxed);
            self.cells.stats.objects_opened.fetch_add(1, Relaxed);
            let mut o = Object::new(db.0);
            o.new_dirs = db.1;
            self.objects.insert(name.clone(), o);
        }
        let o = self.objects.get_mut(&name).unwrap();
        if o.busy || o.checkpointing {
            o.waiting.push_back((origin, req));
            return;
        }
        match self.run_object(&name, origin, req, vec![]) {
            Ok(true) => {}
            Ok(false) => self.fleet_skip(&name, origin),
            Err(reply) => {
                let _ = self.answer(origin, reply);
            }
        }
    }

    /// Begin one request on an object: its warm cell (or a fresh one), inside
    /// a transaction. `Ok(false)`: nothing to run (a stale alarm).
    /// `more`: a batch, the requests after the first (`req` holds them all,
    /// framed for `handle_batch`).
    fn run_object(&mut self, name: &str, origin: Origin, mut req: String, more: Vec<Origin>) -> Result<bool, (u16, String)> {
        let (eng, app, code) = (self.eng, self.app.clone(), self.code.clone());
        let ticking = self.ticks.contains_key(name);
        let proc = app.object_proc(name);
        let environ = self.environ_for(proc);
        // WebSocket events say what they are in their text, the same here
        // and when forwarded from the node that holds the connection.
        let sock_of = |s: &str| s.trim_start_matches('/').parse::<u64>().ok();
        let mut ws_open = None;
        let mut ws_in = vec![];
        if let Some(rest) = req.strip_prefix("do WSOPEN ") {
            ws_open = rest.rsplit(' ').next().and_then(sock_of);
        } else if let Some(rest) = req.strip_prefix("do WSCLOSE ") {
            if let Some(id) = rest.split(' ').next().and_then(sock_of) {
                self.ws_gone.entry(name.to_string()).or_default().insert(id);
            }
        } else if let Some(rest) = req.strip_prefix("do WSBIN ") {
            // Bytes for `ws_msg`; the cell sees their length.
            let (sock, b64) = rest.split_once(' ').unwrap_or((rest, ""));
            ws_in = crate::ws::unbase64(b64).unwrap_or_default();
            req = format!("do WSBIN {sock} {}", ws_in.len());
        }
        let gone: BTreeSet<u64> = self.ws_gone.get(name).cloned().unwrap_or_default();
        let o = self.objects.get_mut(name).unwrap();
        o.last_used = Instant::now();
        {
            let db = o.db.lock().unwrap();
            if let Err(e) = db.begin() {
                eprintln!("object {name:?}: begin failed: {e}");
                return Err((500, "{\"error\":\"storage\"}".into()));
            }
            if matches!(origin, Origin::Fleet(_)) && req.starts_with("do ALARM ") {
                // Fleet alarms: run only if storage says one is due now.
                match db.alarm() {
                    Some(t) if t <= now_ms() => {
                        let _ = db.set_alarm(None); // the handler can set a new one
                    }
                    _ => {
                        db.rollback();
                        return Ok(false);
                    }
                }
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
            if o.socks.is_none() {
                o.socks = Some(db.sockets().into_iter().collect());
            }
        }
        // Gone sockets leave the list in this request's transaction; the
        // rest are the ones it may use (and, for a handshake, its own: sends
        // to it go out after the 101).
        let socks = o.socks.as_ref().unwrap();
        let ws_drop: Vec<u64> = gone.iter().copied().filter(|id| socks.contains(id)).collect();
        let ids: Vec<u64> = socks.iter().copied().filter(|id| !gone.contains(id)).chain(ws_open).collect();
        if !ws_drop.is_empty() {
            let db = o.db.lock().unwrap();
            if let Err(e) = db.update_sockets(None, &ws_drop) {
                eprintln!("object {name:?}: socket list: {e}");
                db.rollback();
                return Err((500, "{\"error\":\"storage\"}".into()));
            }
        }
        let mut cell = match o.cell.take() {
            // A cell of the old code (a deploy came since) gives way, but not
            // in the middle of something (open sockets, a beat: a game in
            // play): it finishes on the version it started on.
            Some(c) if !app.procs[c.proc].retired || ticking || !ids.is_empty() => c,
            _ => match fresh(eng, &app, &code, proc, &environ) {
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
        cell.host().ws_ids = ids;
        cell.host().ws_out.clear();
        cell.host().out.clear();
        cell.host().ws_in = ws_in;
        let export = if more.is_empty() { "handle" } else { "handle_batch" };
        if let Err(trap) = crate::start_export(&mut cell.vm, export, &req, &mut cell.st) {
            eprintln!("object {name:?} trapped starting a request: {trap}");
            o.db.lock().unwrap().rollback();
            return Err((500, String::new()));
        }
        o.busy = true;
        self.tasks.push_back(Task { origin, cell, obj: Some(name.to_string()), ws_open, ws_drop, pg: vec![], ev: None, more });
        Ok(true)
    }

    /// The cells' side of a drop: the object leaves memory, and its file goes.
    fn drop_object(&mut self, name: &str) {
        if self.remove_object(name).is_some() {
            self.cells.stats.objects_live.fetch_sub(1, Relaxed);
        }
        let path = store::path_for(&self.set.data, name);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    /// Tell the node if an object now holds a database session or a beat,
    /// or no longer does (fleet mode: placement leaves those alone).
    fn anchor(&mut self, name: &str) {
        if self.set.node.is_none() {
            return;
        }
        let on = self.objects.get(name).is_some_and(|o| !o.pg.is_empty()) || self.ticks.contains_key(name);
        if on != self.anchors.contains(name) {
            if on {
                self.anchors.insert(name.to_string());
            } else {
                self.anchors.remove(name);
            }
            let _ = self.up(Up::Anchor { name: name.to_string(), on });
        }
    }

    /// The cells' side of a restore: rebuild its database, then serve.
    fn restore(&mut self, name: String, act: Act, segs: Result<Vec<Vec<u8>>, String>, image: Option<String>) {
        let path = store::path_for(&self.set.data, &name);
        // Its folder; durability here comes from the peers and the bucket.
        if segs.is_ok() {
            let _ = store::make_dirs(&self.set.data, &path);
        }
        if self.remove_object(&name).is_some() {
            self.cells.stats.objects_live.fetch_sub(1, Relaxed);
        }
        let restored = segs.and_then(|segs| store::restore(&path, &segs).map_err(|e| e.to_string()));
        // The image: the file as restored, before anything runs on it.
        let image = match (&restored, image) {
            (Ok(()), Some(key)) => match store::image(&path) {
                Ok(Some(entry)) => Some((key, entry)),
                Ok(None) => None,
                Err(e) => {
                    eprintln!("object {name:?}: no image: {e}");
                    None
                }
            },
            _ => None,
        };
        let alarm = match restored.and_then(|()| Db::open(&path).map_err(|e| e.to_string())) {
            Ok(db) => {
                self.cells.stats.objects_live.fetch_add(1, Relaxed);
                self.cells.stats.objects_opened.fetch_add(1, Relaxed);
                let alarm = db.alarm();
                self.objects.insert(name.clone(), Object::new(db));
                Some(alarm)
            }
            Err(e) => {
                eprintln!("object {name:?}: restore failed: {e}");
                None
            }
        };
        let _ = self.up(Up::Restored { name, act, alarm, image });
    }

    /// The cells' side of a seal: the changes committed since the last one,
    /// and an image if asked and nothing's running (else none this time;
    /// the core asks again later). A seal that fails drops the object.
    fn seal(&mut self, name: String, act: Act, image: Option<String>) {
        let Some(o) = self.objects.get_mut(&name) else { return };
        let (changes, image) = {
            let db = o.db.lock().unwrap();
            let c = db.seal();
            // ponytail: read on the loop, the whole file; a big object
            // stalls its loop that long every compact_segs uploads.
            let img = match (&c, image) {
                (Ok(_), Some(key)) if !o.busy => match db.image_now() {
                    Ok(Some(entry)) => Some((key, entry)),
                    Ok(None) => None,
                    Err(e) => {
                        eprintln!("object {name:?}: no image: {e}");
                        None
                    }
                },
                _ => None,
            };
            // Checkpoint right after a seal, never mid-request.
            if c.is_ok() && !o.busy && db.wants_checkpoint() {
                let _ = db.checkpoint_local();
            }
            (c.map_err(|e| e.to_string()), img)
        };
        if let Err(e) = &changes {
            eprintln!("object {name:?}: seal failed: {e}");
            self.remove_object(&name);
        }
        let _ = self.up(Up::Sealed { name, act, changes, image });
    }

    /// A request the core ran is done: commit locally (no fsync: the bucket
    /// is what makes it durable), and tell the node (`Up::Ran`).
    #[allow(clippy::too_many_arguments)]
    fn fleet_finish(&mut self, name: String, req: own::ReqId, mut cell: Cell<E>, answer: Result<String, String>, ws: Vec<WsOut>, ws_open: Option<u64>, ws_drop: Vec<u64>, rest: Vec<(Origin, Result<String, String>)>) -> Option<(u16, String)> {
        let Some(o) = self.objects.get_mut(&name) else { return None };
        o.busy = false;
        o.last_used = Instant::now();
        let (mut committed, mut wrote) = (None, None);
        let reply = {
            let db = o.db.lock().unwrap();
            match answer {
                Ok(a) if ws_accepted(&a, ws_open).is_some_and(|id| db.update_sockets(Some(id), &[]).is_err()) => {
                    db.rollback();
                    (500, "{\"error\":\"storage\"}".into())
                }
                Ok(a) => match db.commit() {
                    Ok(w) => {
                        o.cell = Some(cell);
                        committed = Some(ws_accepted(&a, ws_open));
                        wrote = w.then(|| db.alarm());
                        split_answer(&a)
                    }
                    Err(e) => {
                        eprintln!("object {name:?}: commit failed: {e}");
                        db.rollback();
                        (500, "{\"error\":\"storage\"}".into())
                    }
                },
                Err(trap) => {
                    eprintln!("object {name:?} trapped: {trap}{}", said(&mut cell));
                    db.rollback();
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            }
        };
        if let Some(joined) = committed {
            self.sockets_committed(&name, joined, &ws_drop);
        }
        // The sends wait with the answer, for the core to say it's proven.
        let ok = committed.is_some();
        // The rest of a batch: answers of their own (or the first's failure).
        let rest = rest
            .into_iter()
            .filter_map(|(o, a)| {
                let Origin::Fleet(r) = o else { return None };
                Some((r, match a {
                    Ok(a) if ok => split_answer(&a),
                    _ => reply.clone(),
                }))
            })
            .collect();
        // What waited starts before the core hears, so the object takes its
        // requests in order.
        self.next_waiting(&name);
        let _ = self.up(Up::Ran { name, req, reply, wrote, ws: if ok { ws } else { vec![] }, rest });
        None
    }

    /// An alarm request found nothing due: it's done, and changed nothing.
    fn fleet_skip(&mut self, name: &str, origin: Origin) {
        if let Origin::Fleet(req) = origin {
            let _ = self.up(Up::Skipped { name: name.to_string(), req });
        }
    }

    /// Eviction: hand objects that have been idle too long back to the core.
    fn fleet_sweep(&mut self) {
        self.last_sweep = Instant::now();
        self.retire_cells();
        self.sweep_cells();
        let idle = self.idle_limit();
        let gone: Vec<String> = self.objects.iter().filter(|(_, o)| !o.busy && o.waiting.is_empty() && o.last_used.elapsed() >= idle).map(|(n, _)| n.clone()).collect();
        if !gone.is_empty() {
            let _ = self.up(Up::Idle(gone));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_pooled_cells_go_but_the_newest_stays() {
        let now = Instant::now();
        let old = now - Duration::from_secs(120);
        let mut pool = vec![(1, old), (2, old), (3, now), (4, now)];
        trim_idle(&mut pool, Duration::from_secs(60));
        assert_eq!(pool.iter().map(|x| x.0).collect::<Vec<_>>(), [3, 4], "the idle ones go");
        let mut pool = vec![(1, old), (2, old)];
        trim_idle(&mut pool, Duration::from_secs(60));
        assert_eq!(pool.iter().map(|x| x.0).collect::<Vec<_>>(), [2], "the newest stays, however old");
    }
}

/// This build: a hash of the running binary. A child of another build (the
/// binary changed under a running node) won't run.
pub fn build_id() -> String {
    use sha2::Digest;
    std::fs::read(own_exe()).map(|b| sha2::Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect()).unwrap_or_default()
}

/// The running binary. On Linux, `/proc/self/exe` is the one running, even
/// once an update has replaced its file.
fn own_exe() -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().unwrap_or_default()
    }
}

/// What a deploy must be to go live, checked where the cells are (the node
/// loads no program): each new module loads, a `memory_kb` under what its
/// module starts with is refused with why (not at its first request), and
/// what's worth knowing about a ws process the host freezes is said.
pub fn check<E: Engine>(eng: &E, progs: &Progs<E::Program>, app: &App) -> Result<(), String> {
    let code = Code::empty().next(app, |p| progs.get(eng, p))?;
    for p in app.procs.iter().filter(|p| !p.retired) {
        if let (true, Sleep::Auto, Some(note)) = (p.socket, p.sleep, eng.note(&code.progs[p.prog])) {
            eprintln!("combd     warning: {}/{}: {note}", app.tenants[p.tenant].key(), p.name);
        }
        if let Some(pages) = p.memory_pages {
            let mut vm = eng.spawn(&code.progs[p.prog])?;
            vm.cap_memory(pages).map_err(|e| format!("{}/{}: {e}", app.tenants[p.tenant].key(), p.name))?;
        }
    }
    Ok(())
}

/// The node's hold on its cells, wherever they run: each loop's end of its
/// line, the control socket (deploy checks; while it's open, they run), and
/// the cells now, which the node's keeper starts again when they die
/// (`http::keep_cells`).
pub struct Link {
    pub ends: Vec<mio::net::UnixStream>,
    ctl: Mutex<std::os::unix::net::UnixStream>,
    now: Mutex<Now>,
    /// What they were told first (`line::Start`, encoded), to start again.
    start: Vec<u8>,
    child: bool,
}

/// The cells running now: which start of them it is (one more each time),
/// and whether a loop asked to stop them (`Link::kill`).
struct Now {
    run: Running,
    inst: u64,
    stop: bool,
}

enum Running {
    Child(std::process::Child),
    /// On threads (tests): the thread, and the cells' ends of their
    /// sockets, to cut, as a process that dies cuts its own.
    Threads(std::thread::JoinHandle<()>, Vec<std::os::unix::net::UnixStream>),
}

impl Link {
    /// Will `app` go live? The cells load what's new in it and say.
    pub fn check(&self, app: &App) -> Result<(), String> {
        use std::io::{Read, Write};
        let mut b = vec![];
        crate::line::Ctl::Check(app.for_cells(0)).encode(&mut b);
        let mut ctl = self.ctl.lock().unwrap();
        let gone = |e: std::io::Error| format!("the cells didn't answer a check: {e}");
        ctl.write_all(&(b.len() as u32).to_le_bytes()).and_then(|()| ctl.write_all(&b)).map_err(gone)?;
        let mut n = [0u8; 4];
        ctl.read_exact(&mut n).map_err(gone)?;
        let mut b = vec![0; u32::from_le_bytes(n) as usize];
        ctl.read_exact(&mut b).map_err(gone)?;
        crate::line::Ctl::decode_answer(&b).ok_or_else(|| "the cells' answer to a check doesn't decode".to_string())?
    }

    /// Which start of the cells runs now (0 first; each line a loop has
    /// comes from one).
    #[cfg(test)]
    pub fn inst(&self) -> u64 {
        self.now.lock().unwrap().inst
    }

    /// A loop found start `inst`'s line closed or broken: stop them, if
    /// they're still the ones running (`wait` does it, and says they died).
    pub fn kill(&self, inst: u64) {
        let mut now = self.now.lock().unwrap();
        if now.inst == inst {
            now.stop = true;
        }
    }

    /// Wait until the cells die (stopping them, if asked), and say how; or
    /// `None` once `quit` is set.
    pub fn wait(&self, quit: &std::sync::atomic::AtomicBool) -> Option<String> {
        while !quit.load(Relaxed) {
            {
                let mut now = self.now.lock().unwrap();
                let stop = std::mem::take(&mut now.stop);
                match &mut now.run {
                    Running::Child(c) => {
                        if stop {
                            let _ = c.kill();
                        }
                        if let Ok(Some(status)) = c.try_wait() {
                            use std::os::unix::process::ExitStatusExt;
                            // `SIGSYS`: seccomp's, for a call off the lockdown's list.
                            let why = if status.signal() == Some(rustix::process::Signal::SYS.as_raw()) { ", killed by its lockdown for a system call it may not make" } else { "" };
                            return Some(format!("{status}{why}"));
                        }
                    }
                    Running::Threads(t, theirs) => {
                        if stop {
                            for s in theirs.iter() {
                                let _ = s.shutdown(std::net::Shutdown::Both);
                            }
                        }
                        if t.is_finished() {
                            return Some("their threads stopped".into());
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Start the cells again, as they were first (`start`): each loop's new
    /// end of its line, and which start this is.
    pub fn again(&self) -> std::io::Result<(Vec<mio::net::UnixStream>, u64)> {
        let (ends, ctl, run) = spawn(&self.start, self.child)?;
        *self.ctl.lock().unwrap() = ctl;
        let mut now = self.now.lock().unwrap();
        now.run = run;
        now.inst += 1;
        now.stop = false;
        Ok((ends, now.inst))
    }
}

/// Start a node's cells: in a process of their own (`combd cells`), or, if
/// not `child`, on threads here. Either way they're reached over sockets,
/// and make their own engine from `start.engine` (`Engine::child`).
pub fn start(start: crate::line::Start, child: bool) -> std::io::Result<Link> {
    let mut b = vec![];
    start.encode(&mut b);
    let (ends, ctl, run) = spawn(&b, child)?;
    Ok(Link { ends, ctl: Mutex::new(ctl), now: Mutex::new(Now { run, inst: 0, stop: false }), start: b, child })
}

fn spawn(start: &[u8], child: bool) -> std::io::Result<(Vec<mio::net::UnixStream>, std::os::unix::net::UnixStream, Running)> {
    use std::io::{Read, Write};
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    let spec = crate::line::Start::decode(start).ok_or_else(|| std::io::Error::other("a start that doesn't decode"))?;
    let (ctl, ctl_cells) = UnixStream::pair()?;
    let (mut ends, mut theirs) = (vec![], vec![]);
    for _ in 0..spec.set.threads {
        let (a, b) = UnixStream::pair()?;
        a.set_nonblocking(true)?;
        ends.push(mio::net::UnixStream::from_std(a));
        theirs.push(b);
    }
    if !child {
        let cut = theirs.iter().chain([&ctl_cells]).map(UnixStream::try_clone).collect::<std::io::Result<_>>()?;
        let t = std::thread::Builder::new().name("cells".into()).spawn(move || {
            if let Err(e) = serve_spec(spec, theirs, ctl_cells, false) {
                eprintln!("combd     cells: {e}");
            }
        })?;
        return Ok((ends, ctl, Running::Threads(t, cut)));
    }
    // An empty environment, nothing open but the control socket (as its
    // stdin), which brings its loops' sockets and then what it's told first.
    let mut ctl_w = ctl;
    let child = std::process::Command::new(own_exe()).arg("cells").env_clear().stdin(std::process::Stdio::from(OwnedFd::from(ctl_cells))).spawn()?;
    println!("combd     cells in process {}", child.id());
    let fds: Vec<_> = theirs.iter().map(|s| s.as_fd()).collect();
    let mut space = vec![std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(fds.len()))];
    let mut anc = rustix::net::SendAncillaryBuffer::new(&mut space);
    anc.push(rustix::net::SendAncillaryMessage::ScmRights(&fds));
    rustix::net::sendmsg(&ctl_w, &[std::io::IoSlice::new(&[0])], &mut anc, rustix::net::SendFlags::empty())?;
    // Ours stay open until the child says it has them: a socket that's
    // only in flight can be flushed by the kernel meanwhile (macOS, when
    // another socket closes: a restarted child's lines read as closed).
    let mut got = [0u8; 1];
    ctl_w.set_read_timeout(Some(Duration::from_secs(30)))?;
    ctl_w.read_exact(&mut got).map_err(|e| std::io::Error::other(format!("the cells' process didn't take its sockets: {e}")))?;
    ctl_w.set_read_timeout(None)?;
    drop(theirs);
    ctl_w.write_all(&(start.len() as u32).to_le_bytes())?;
    ctl_w.write_all(start)?;
    Ok((ends, ctl_w, Running::Child(child)))
}

/// `combd cells`: a node's cells, in a process of their own (`start`).
pub fn child() -> ! {
    let code = match child_run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("combd     cells: {e}");
            1
        }
    };
    std::process::exit(code)
}

fn child_run() -> std::io::Result<()> {
    use std::io::Read;
    use std::os::fd::AsFd;
    let mut ctl = std::os::unix::net::UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    // Its loops' sockets (as many as a node has loops: 1024 at most here).
    let mut space = vec![std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1024))];
    let mut anc = rustix::net::RecvAncillaryBuffer::new(&mut space);
    let mut one = [0u8; 1];
    // (macOS has no `CMSG_CLOEXEC`; the child starts no programs anyway.)
    #[cfg(target_os = "linux")]
    let flags = rustix::net::RecvFlags::CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = rustix::net::RecvFlags::empty();
    rustix::net::recvmsg(&ctl, &mut [std::io::IoSliceMut::new(&mut one)], &mut anc, flags)?;
    let mut ends = vec![];
    for m in anc.drain() {
        if let rustix::net::RecvAncillaryMessage::ScmRights(fds) = m {
            ends.extend(fds.map(std::os::unix::net::UnixStream::from));
        }
    }
    std::io::Write::write_all(&mut ctl, &[1])?;
    let mut n = [0u8; 4];
    ctl.read_exact(&mut n)?;
    let mut b = vec![0; u32::from_le_bytes(n) as usize];
    ctl.read_exact(&mut b)?;
    let start = crate::line::Start::decode(&b).ok_or_else(|| std::io::Error::other("a start that doesn't decode"))?;
    if start.build != build_id() {
        return Err(std::io::Error::other("another build than the node's: its binary changed under it"));
    }
    if let Some(d) = &start.aot_dir {
        std::env::set_var("COMBD_AOT_DIR", d);
    }
    serve_spec(start, ends, ctl, true)
}

/// The cells, with the engine `start.engine` says. In a `child`, locked
/// down first.
fn serve_spec(start: crate::line::Start, ends: Vec<std::os::unix::net::UnixStream>, ctl: std::os::unix::net::UnixStream, child: bool) -> std::io::Result<()> {
    if ends.len() != start.set.threads {
        return Err(std::io::Error::other(format!("{} sockets for {} loops", ends.len(), start.set.threads)));
    }
    // `<kind>:<n>:<rest>`, the rest (a folder) as it is, colons and all.
    let engine = start.engine.clone();
    let parts: Vec<&str> = engine.splitn(3, ':').collect();
    let bad = || std::io::Error::other(format!("an engine the cells can't make: {engine}"));
    let num = |i: usize| parts.get(i).and_then(|n| n.parse::<u64>().ok()).ok_or_else(bad);
    if child {
        let builds = parts.get(2).filter(|b| parts[0] == "both" && !b.is_empty()).map(Path::new);
        lockdown(&start, parts[0] != "interp", builds)?;
    }
    match parts[0] {
        "interp" => serve(&crate::interp::Interp { max_pages: num(1)? as u32 }, start, ends, ctl),
        #[cfg(feature = "aot-lite")]
        "aot" => serve(&crate::aot::Aot, start, ends, ctl),
        #[cfg(feature = "aot-lite")]
        "both" => {
            let builds = parts.get(2).filter(|b| !b.is_empty()).map(PathBuf::from);
            serve(&crate::both::Both { interp: crate::interp::Interp { max_pages: num(1)? as u32 }, aot: crate::aot::Aot, builds }, start, ends, ctl)
        }
        #[cfg(feature = "wasmtime")]
        "wasmtime" => serve(&crate::wt::Wt::new(num(1)?, num(2).ok().map(|n| n as u32)), start, ends, ctl),
        _ => Err(bad()),
    }
}

/// The child's walls (`combd_wall`), up before it makes its engine or loads
/// a module: its objects' folder read and written, compiled builds only
/// read, and memory made executable only for an engine that loads machine
/// code (`exec`).
fn lockdown(start: &crate::line::Start, exec: bool, builds: Option<&Path>) -> std::io::Result<()> {
    if !start.lock {
        println!("combd     cells not locked down (COMBD_LOCKDOWN=off)");
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        println!("combd     cells not locked down: landlock and seccomp are Linux's");
        return Ok(());
    }
    // SQLite seeds its randomness from `/dev/urandom` (else, quietly, from
    // the time and pid).
    let ro: Vec<&Path> = start.aot_dir.iter().map(PathBuf::as_path).chain(builds).chain([Path::new("/dev/urandom")]).collect();
    let walls = combd_wall::Walls { rw: vec![&start.set.data], ro, exec };
    let locked = combd_wall::lock(&walls).map_err(|e| std::io::Error::other(format!("locking down: {e} (COMBD_LOCKDOWN=off runs the cells without it)")))?;
    println!("combd     cells locked down: landlock {} and seccomp", locked.landlock);
    Ok(())
}

/// The cells: a loop for each line, and deploy checks on the control
/// socket, until the node closes it (and its lines).
fn serve<E>(eng: &E, start: crate::line::Start, ends: Vec<std::os::unix::net::UnixStream>, mut ctl: std::os::unix::net::UnixStream) -> std::io::Result<()>
where
    E: Engine + Sync,
    E::Program: Sync + Send,
{
    use std::io::{Read, Write};
    let (mut loops, mut to) = (vec![], vec![]);
    for s in ends {
        let poll = Poll::new()?;
        let waker = Arc::new(mio::Waker::new(poll.registry(), WAKE)?);
        let (tx, rx) = std::sync::mpsc::channel();
        s.set_nonblocking(true)?;
        loops.push((poll, rx, mio::net::UnixStream::from_std(s)));
        to.push((tx, waker));
    }
    let cells = Cells::start(&start.set.data, start.sync_delay, move |back, done| {
        let (tx, waker): &(std::sync::mpsc::Sender<Done>, Arc<mio::Waker>) = &to[back];
        let _ = tx.send(done);
        let _ = waker.wake();
    });
    let progs = Progs::new();
    let (set, cells, progs) = (&start.set, &cells, &progs);
    std::thread::scope(|sc| {
        for (me, (poll, rx, s)) in loops.into_iter().enumerate() {
            std::thread::Builder::new()
                .name(format!("cells-{me}"))
                .spawn_scoped(sc, move || {
                    let mut c = CellLoop::new(eng, set, cells, progs, me);
                    match c.run(End::new(s, usize::MAX), poll, rx) {
                        // The node's gone mid-write: as good as closed.
                        Err(e) if matches!(e.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset) => {}
                        Err(e) => eprintln!("cells loop stopped: {e}"),
                        Ok(()) => {}
                    }
                })
                .expect("cells thread");
        }
        // Deploy checks, one at a time, until the node closes the socket.
        let mut n = [0u8; 4];
        while ctl.read_exact(&mut n).is_ok() {
            let mut b = vec![0; u32::from_le_bytes(n) as usize];
            if ctl.read_exact(&mut b).is_err() {
                break;
            }
            let answer = match crate::line::Ctl::decode(&b) {
                Some(crate::line::Ctl::Check(d)) => App::from_node(&App::empty(), d).ok_or_else(|| "a check of an app that doesn't hold together".to_string()).and_then(|app| check(eng, progs, &app)),
                None => Err("a check that doesn't decode".into()),
            };
            let mut out = vec![];
            crate::line::Ctl::encode_answer(&answer, &mut out);
            if ctl.write_all(&(out.len() as u32).to_le_bytes()).and_then(|()| ctl.write_all(&out)).is_err() {
                break;
            }
        }
    });
    Ok(())
}
