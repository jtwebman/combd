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
//!   `COMBD_DATA`). Each request is a transaction. A crash rolls it back and
//!   drops the cell; the next request gets a fresh one, which loads from
//!   storage.
//! - **The output gate, as a group commit.** Commits don't fsync. A pool of
//!   threads fsyncs each object's WAL file, at most one at a time per object,
//!   and an answer is held until an fsync has covered its commit (or, for a
//!   read, every commit before it). One fsync releases everything that
//!   committed while the last one ran, and many objects sync at once.
//! - **Alarms.** A cell sets one with `alarm_set`; it commits with the request.
//!   The owner runs `ALARM` when it's due. A failed alarm is retried with
//!   backoff, up to `ALARM_TRIES` times.
//! - **Eviction.** Objects idle for `COMBD_IDLE_MS`, or past `COMBD_MAX_OBJECTS`
//!   per loop, leave memory. They come back from storage when needed.
//!
//! `GET /_combd/stats` shows what the node is doing.
//!
//! **Fleet mode (D2),** when `COMBD_S3` is set (see `fleet.rs`): many nodes
//! share one bucket, and the bucket decides who owns each object. Each loop
//! runs a decision core (`own.rs`) for its objects. The core says when to
//! run a request, seal its changes (changed SQLite pages), and restore an
//! object from the bucket. An answer leaves only after its changes are in the
//! bucket and a fresh read of the ownership record shows we still own the
//! object: the output gate for D2, instead of a local fsync. A request for an
//! object another node owns is forwarded there over the wire.
//! - Eviction hands an idle object back to the core; the next request claims
//!   it again (a new epoch) and restores it from the bucket.
//! - Alarms live in the object's SQLite, so they travel with its segments.
//!   An index in the bucket (`alarm/<id>/<due>`, holding the name) lets the
//!   fleet find due alarms of objects that aren't running anywhere. It's
//!   written before the segment that sets the alarm is uploaded, so it's never
//!   missing; a stale entry costs one wasted fire, which checks storage and
//!   cleans it up. Loop 0 scans it each second and fires due alarms as
//!   ordinary routed requests; a fire that finds nothing due does nothing.
//!   ponytail: every node scans the whole index. At scale each node scans
//!   only the objects that place onto it.

use crate::engine::{CellVm, Engine, Outcome};
use crate::fleet::{DiskPeers, FleetCfg, Frame};
use crate::wire::{self, Wire};
use crate::own::{self, Act, Core, Effect};
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
/// Threads that run fsyncs, so the event loops never wait on the disk.
const SYNC_THREADS: usize = 16;

pub struct Config {
    pub threads: usize,
    pub data: PathBuf,
    /// Objects idle this long leave memory.
    pub idle: Duration,
    /// The most objects in memory, per loop.
    pub max_objects: usize,
    /// Set it to stop the server.
    pub stop: Arc<AtomicBool>,
    /// Tests only: every fsync takes at least this long, to prove the output
    /// gate waits for it.
    pub sync_delay: Duration,
    /// Fleet mode (D2), or `None` for one node on its own disk (D1).
    pub fleet: Option<FleetCfg>,
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
            sync_delay: Duration::ZERO,
            fleet: FleetCfg::from_env(),
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
    /// Commits that wrote something, and the fsyncs that made them durable.
    commits: AtomicU64,
    fsyncs: AtomicU64,
    checkpoints: Arc<AtomicU64>,
    /// fsync times: slow ones (10 ms or more), and the slowest, in us.
    fsyncs_slow: Arc<AtomicU64>,
    fsync_max_us: Arc<AtomicU64>,
    /// Fleet mode: batches acknowledged by the peers (D3) or the bucket
    /// (D2), takeovers that fenced, and entries recovered from peers.
    peer_acks: AtomicU64,
    bucket_acks: AtomicU64,
    fences: AtomicU64,
    tail_entries: AtomicU64,
}

/// Make an object's commits up to `gen` durable, then tell loop `back`.
struct SyncJob {
    file: Arc<std::fs::File>,
    db_path: PathBuf,
    /// Checkpoint after the fsync. The object is paused (no request runs on
    /// it), so the checkpoint copies the whole WAL and the WAL can restart.
    ckpt: bool,
    obj: String,
    gen: u64,
    back: usize,
}

/// What the loops share.
struct Shared {
    cfg: Config,
    peers: Vec<(Sender<Msg>, Arc<Waker>)>,
    sync: Mutex<Sender<SyncJob>>,
    index: Mutex<AlarmIndex>,
    stats: Stats,
    fleet: Option<FleetShared>,
}

/// Fleet mode: what the loops share.
struct FleetShared {
    cfg: FleetCfg,
    /// Bucket work, done by a pool of threads.
    jobs: Mutex<Sender<Job>>,
    /// The wire to other nodes (mTLS).
    wire: Wire,
    /// This node's peer store (D3), on its own thread.
    peer_store: Mutex<Sender<(own::PeerCall, PeerBack)>>,
    next_id: AtomicU64,
    start: Instant,
}

enum Job {
    Call { op: own::OpId, call: own::Call, back: usize },
    /// Fetch an object's segments, and its alarm index entries.
    Fetch { obj: own::Obj, act: Act, segs: Vec<String>, back: usize },
    /// Move an object's alarm index entry from `old` to `new` (due times).
    Index { name: String, obj: own::Obj, old: Option<i64>, new: Option<i64>, back: usize },
    /// The due alarms in the index.
    AlarmScan { back: usize },
}

/// Where a peer store answer goes: back over the wire (connection, frame
/// id), or to a loop on this node (loop, id).
enum PeerBack {
    Wire(usize, u64),
    Loop(usize, u64),
}

fn alarm_prefix(obj: own::Obj) -> String {
    format!("alarm/{obj:032x}/")
}

fn alarm_key(obj: own::Obj, at: i64) -> String {
    format!("alarm/{obj:032x}/{at:013}")
}

/// Loop 0 scans the alarm index this often.
const ALARM_SCAN: Duration = Duration::from_secs(1);

/// A forwarded request with no reply in time gets a 503 (the peer may be
/// frozen), so the client can retry. "In time" is measured per peer, as TCP
/// does it: smoothed round trip + 4x its variation, within these bounds, so a
/// peer across the world gets more time than one in the same rack.
const FORWARD_MIN: Duration = Duration::from_secs(1);
const FORWARD_MAX: Duration = Duration::from_secs(30);
const FORWARD_FIRST: Duration = Duration::from_secs(5);

/// Bucket I/O is slow and parallel; these threads do it.
const BUCKET_THREADS: usize = 32;
/// How many times a request may be forwarded between nodes.
const MAX_HOPS: u8 = 1;

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
    /// Fleet mode: a request the decision core started.
    Fleet(own::ReqId),
    /// Fleet mode: a request another node forwarded (connection, frame id).
    Wire(usize, u64),
}

/// Between loops.
enum Msg {
    /// Run this request on an object you own.
    Run { origin: Origin, obj: String, req: String },
    /// Here is the answer for one of your connections.
    Reply { who: Who, reply: (u16, String) },
    /// An fsync finished: the object's commits up to `gen` are on disk.
    Synced { obj: String, gen: u64, ok: bool },
    /// The object's checkpoint finished; it can run requests again.
    Checkpointed { obj: String },
    /// Fleet mode: a bucket call's result.
    Bucket { op: own::OpId, res: own::Res },
    /// Fleet mode: an object's segments, for a restore, and its alarm index entries.
    Fetched { obj: own::Obj, act: Act, data: Result<Vec<Vec<u8>>, String>, index: Vec<i64> },
    /// Fleet mode: an alarm index move finished.
    Indexed { name: String, new: Option<i64>, ok: bool },
    /// Fleet mode: due alarms from the index: (name, due).
    AlarmsDue { due: Vec<(String, i64)> },
    /// Fleet mode: another node forwarded us a request.
    WireIn { conn: usize, id: u64, name: String, req: String },
    /// Fleet mode: the answer to a request we forwarded.
    WireReply { id: u64, reply: (u16, String) },
    /// Fleet mode: the answer to a peer call (`None`: lost).
    PeerReply { id: u64, res: Option<own::PeerRes> },
}

/// Fleet mode: a request the decision core knows about.
struct FReq {
    origin: Origin,
    name: String,
    cell_req: String,
    hops: u8,
    act: Act,
    /// The answer, held until the core says it's proven.
    resp: Option<(u16, String)>,
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
    /// Commits that wrote (numbered), and how many of them are on disk.
    committed: u64,
    synced: u64,
    syncing: bool,
    /// Answers waiting for an fsync: (the commit they need, origin, answer).
    held: Vec<(u64, Origin, (u16, String))>,
    /// Paused for a checkpoint: requests wait until it's done.
    checkpointing: bool,
    /// Fleet mode: the alarm index entry, as far as we know; one move at a
    /// time; and a seal held until the index has the alarm it carries.
    index_at: Option<i64>,
    index_busy: bool,
    seal_wait: Option<(Act, Vec<u8>)>,
}

impl<E: Engine> Object<E> {
    fn new(db: Db) -> Object<E> {
        Object {
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
            index_at: None,
            index_busy: false,
            seal_wait: None,
        }
    }
}

/// A parsed request.
enum Req {
    Cell(String),
    Object(String, String),
    Stats,
    /// A new object id.
    NewId,
}

/// A UUIDv7 (RFC 9562): 48 bits of Unix ms, then random bits. It sorts by
/// creation time, so ids are handy in logs and listings; bucket keys hash it
/// (`own::obj_id`), so new objects still spread across the bucket instead of
/// piling onto one key range. ponytail: no ordering within one ms.
pub fn uuid_v7() -> String {
    let ms = now_ms() as u64 & 0xffff_ffff_ffff;
    let mut r = [0u8; 10];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut r));
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6..].copy_from_slice(&r);
    b[6] = 0x70 | (b[6] & 0x0f); // version 7
    b[8] = 0x80 | (b[8] & 0x3f); // variant 10
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Any well-formed UUID, lowercased.
fn parse_uuid(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let ok = b.len() == 36
        && b.iter().enumerate().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { *c == b'-' } else { c.is_ascii_hexdigit() });
    ok.then(|| s.to_ascii_lowercase())
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

pub fn serve_with<E>(eng: &E, prog: &E::Program, port: u16, mut cfg: Config)
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
    let (sync_tx, sync_rx) = channel::<SyncJob>();
    let sync_rx = Arc::new(Mutex::new(sync_rx));
    let no_fsync = std::env::var_os("COMBD_EXPERIMENT_NO_FSYNC").is_some();
    let delay = cfg.sync_delay;
    let checkpoints = Arc::new(AtomicU64::new(0));
    let (slow, max_us) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    for _ in 0..SYNC_THREADS {
        let (rx, peers, checkpoints) = (sync_rx.clone(), peers.clone(), checkpoints.clone());
        let (slow, max_us) = (slow.clone(), max_us.clone());
        std::thread::spawn(move || loop {
            let Ok(job) = rx.lock().unwrap().recv() else { return };
            // EXPERIMENT ONLY, never ship: COMBD_EXPERIMENT_NO_FSYNC measures
            // what the fsync costs. It breaks the output gate's promise.
            std::thread::sleep(delay);
            let t = Instant::now();
            let ok = no_fsync || job.file.sync_data().is_ok();
            let us = t.elapsed().as_micros() as u64;
            max_us.fetch_max(us, Relaxed);
            if us >= 10_000 {
                slow.fetch_add(1, Relaxed);
            }
            // Release the answers first; the checkpoint doesn't hold them up.
            let (tx, waker) = &peers[job.back];
            let _ = tx.send(Msg::Synced { obj: job.obj.clone(), gen: job.gen, ok });
            let _ = waker.wake();
            // Keep the WAL small, here instead of on the event loop.
            if job.ckpt {
                if ok {
                    match store::checkpoint(&job.db_path) {
                        Ok(()) => {
                            checkpoints.fetch_add(1, Relaxed);
                        }
                        Err(e) => eprintln!("object {:?}: checkpoint: {e}", job.obj),
                    }
                }
                let _ = tx.send(Msg::Checkpointed { obj: job.obj });
                let _ = waker.wake();
            }
        });
    }
    let fleet = cfg.fleet.take().map(|fc| start_fleet(fc, &peers, cfg.threads, &cfg.data));
    let sh = Shared {
        cfg,
        peers,
        sync: Mutex::new(sync_tx),
        index,
        stats: Stats { checkpoints, fsyncs_slow: slow, fsync_max_us: max_us, ..Stats::default() },
        fleet,
    };
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
                    core: sh.fleet.as_ref().map(|f| {
                        let mut fleet: Vec<own::Node> = f.cfg.peers.keys().copied().chain([f.cfg.node]).collect();
                        fleet.sort();
                        fleet.dedup();
                        let mut c = Core::with_lease(f.cfg.node, own::Bugs::default(), f.cfg.lease_ms).replicate(fleet, f.cfg.replicas);
                        // Loop 0 keeps the node's lease alive.
                        c.renews = me == 0;
                        c
                    }),
                    freqs: HashMap::new(),
                    next_req: 0,
                    names: HashMap::new(),
                    wire_out: HashMap::new(),
                    peer_out: HashMap::new(),
                    rtt: HashMap::new(),
                    alarm_fire: HashMap::new(),
                    last_scan: Instant::now(),
                    last_tick: Instant::now(),
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
    // Fleet mode only.
    core: Option<Core>,
    freqs: HashMap<own::ReqId, FReq>,
    next_req: own::ReqId,
    /// Object ids back to names.
    names: HashMap<own::Obj, String>,
    /// Requests we forwarded: frame id -> where the answer goes, the node, when
    /// it was sent, and when it gives up.
    wire_out: HashMap<u64, (Origin, own::Node, Instant, Duration)>,
    /// Peer calls out: frame id -> the core's op, the node, when sent, and
    /// when it gives up.
    peer_out: HashMap<u64, (own::OpId, own::Node, Instant, Duration)>,
    /// Each peer's round trip: (smoothed, variation), in seconds.
    rtt: HashMap<own::Node, (f64, f64)>,
    /// Alarms loop 0 fired from the index: name -> (due, tries, not again before).
    alarm_fire: HashMap<String, (i64, u32, i64)>,
    last_scan: Instant,
    last_tick: Instant,
}

/// Fleet mode: check the bucket, then start the bucket pool and the wire.
fn start_fleet(fc: FleetCfg, loops: &[(Sender<Msg>, Arc<Waker>)], threads: usize, data: &std::path::Path) -> FleetShared {
    // The startup storage test: never serve on a bucket that can't do CAS.
    if let Err(e) = fc.s3.check() {
        eprintln!("combd     the bucket at {} failed the storage test: {e}", fc.s3.host);
        std::process::exit(1);
    }
    println!("combd     node {} in a fleet, bucket {} passed the storage test", fc.node, fc.s3.host);
    let (tx, rx) = channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..BUCKET_THREADS {
        let (rx, loops, s3) = (rx.clone(), loops.to_vec(), fc.s3.clone());
        std::thread::spawn(move || loop {
            let Ok(job) = rx.lock().unwrap().recv() else { return };
            let (back, msg) = match job {
                Job::Call { op, call, back } => (back, Msg::Bucket { op, res: crate::fleet::run_call(&s3, &call) }),
                Job::Fetch { obj, act, segs, back } => {
                    let index = s3.list(&alarm_prefix(obj)).unwrap_or_default().iter().filter_map(|k| k.rsplit('/').next()?.parse().ok()).collect();
                    (back, Msg::Fetched { obj, act, data: crate::fleet::fetch(&s3, &segs), index })
                }
                Job::Index { name, obj, old, new, back } => {
                    // The new entry first, then the old one goes: never a moment with neither.
                    let put = new.map_or(Ok(()), |at| s3.put(&alarm_key(obj, at), name.as_bytes(), &crate::s3::Cond::Any).map(|_| ()));
                    let del = match old.filter(|o| Some(*o) != new) {
                        Some(o) if put.is_ok() => s3.delete(&alarm_key(obj, o)),
                        _ => Ok(()),
                    };
                    (back, Msg::Indexed { name, new, ok: put.is_ok() && del.is_ok() })
                }
                Job::AlarmScan { back } => {
                    let now = now_ms();
                    let due = s3
                        .list("alarm/")
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|k| {
                            let at: i64 = k.rsplit('/').next()?.parse().ok()?;
                            (at <= now).then_some((k, at))
                        })
                        .filter_map(|(k, at)| Some((String::from_utf8(s3.get(&k).ok()??.0).ok()?, at)))
                        .collect();
                    (back, Msg::AlarmsDue { due })
                }
            };
            let (tx, waker) = &loops[back];
            let _ = tx.send(msg);
            let _ = waker.wake();
        });
    }
    // The wire: mTLS between nodes, unless plain TCP is asked for.
    let tls = match &fc.wire_certs {
        Some(dir) => match wire::load_tls(dir, fc.node) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("combd     can't load the wire certificates from {}: {e}", dir.display());
                std::process::exit(1);
            }
        },
        None if fc.insecure => {
            eprintln!("combd     WARNING: the wire is plain TCP (COMBD_WIRE_INSECURE=1)");
            None
        }
        None => {
            eprintln!("combd     set COMBD_WIRE_CERTS (see `combd-spike wire-certs`), or COMBD_WIRE_INSECURE=1 for plain TCP");
            std::process::exit(1);
        }
    };
    // The peer store first: the wire hands it calls as soon as it's up. A
    // node that has run before (its lease is in the bucket) but lost its
    // peer store must not come back as the same node: it would answer
    // fences knowing nothing, and acknowledged writes could be lost.
    // ponytail: the fix is a store that knows it's new, and an owner that
    // then uses another peer's tail. Until then, a lost disk needs a new
    // node number.
    let fresh = !data.join("peers/peers.log").exists();
    if fresh && fc.replicas > 0 && matches!(fc.s3.get(&own::lease_key(fc.node)), Ok(Some(_))) {
        eprintln!("combd     node {} has run before but its peer store ({}) is gone; start it as a new node number", fc.node, data.join("peers").display());
        std::process::exit(1);
    }
    let disk = DiskPeers::open(&data.join("peers")).unwrap_or_else(|e| {
        eprintln!("combd     can't open the peer store: {e}");
        std::process::exit(1);
    });
    let (ptx, prx) = channel();
    let sink = LoopSink { loops: loops.to_vec(), threads, peer_store: ptx.clone() };
    let wire = wire::start(fc.wire_port, fc.peers.clone(), tls, sink).expect("start the wire");
    let (w, ls) = (wire.clone(), loops.to_vec());
    crate::fleet::serve_peers(disk, prx, move |back, res| match back {
        PeerBack::Wire(conn, id) => w.reply(conn, Frame::PeerReply { id, res }),
        PeerBack::Loop(lp, id) => {
            let (tx, waker) = &ls[lp];
            let _ = tx.send(Msg::PeerReply { id, res: Some(res) });
            let _ = waker.wake();
        }
    });
    if fc.replicas > 0 {
        println!("combd     D3: {} peers per object", fc.replicas);
    }
    FleetShared {
        cfg: fc,
        jobs: Mutex::new(tx),
        wire,
        peer_store: Mutex::new(ptx),
        next_id: AtomicU64::new(1),
        start: Instant::now(),
    }
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
            // A signal (SIGCONT after a freeze, say) interrupts the wait: just
            // wait again. Treating it as an error stopped the loop for good.
            match self.poll.poll(&mut events, Some(timeout)) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => r?,
            }
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
            if self.core.is_some() {
                let late: Vec<u64> = self.wire_out.iter().filter(|(_, (_, _, t, limit))| t.elapsed() >= *limit).map(|(id, _)| *id).collect();
                for id in late {
                    let (origin, ..) = self.wire_out.remove(&id).unwrap();
                    self.deliver(origin, (503, "{\"error\":\"owner didn't answer\"}".into()))?;
                }
                let late: Vec<u64> = self.peer_out.iter().filter(|(_, (_, _, t, limit))| t.elapsed() >= *limit).map(|(id, _)| *id).collect();
                for id in late {
                    let (op, ..) = self.peer_out.remove(&id).unwrap();
                    self.core_on(own::Event::Reply { op, res: own::Res::Lost });
                }
                // Every loop ticks its core (to retry quiet peers); only
                // loop 0's renews the node's lease.
                let renew = self.sh.fleet.as_ref().map_or(own::RENEW_MS, |f| f.cfg.lease_ms / 3);
                if self.last_tick.elapsed() >= Duration::from_millis(renew) {
                    self.last_tick = Instant::now();
                    self.core_on(own::Event::Tick);
                }
                if self.me == 0 && self.last_scan.elapsed() >= ALARM_SCAN {
                    self.last_scan = Instant::now();
                    self.job(Job::AlarmScan { back: 0 });
                }
                if self.last_sweep.elapsed() >= TICK {
                    self.fleet_sweep();
                }
            } else {
                self.fire_alarms();
                if self.last_sweep.elapsed() >= TICK {
                    self.sweep();
                }
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
                if let Some(reply) = self.finish_task(t, answer) {
                    self.deliver(origin, reply)?;
                }
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
                Msg::Synced { obj, gen, ok } => self.synced(&obj, gen, ok)?,
                Msg::Checkpointed { obj } => {
                    if let Some(o) = self.objects.get_mut(&obj) {
                        o.checkpointing = false;
                    }
                    self.next_waiting(&obj);
                    self.kick_sync(&obj);
                }
                Msg::Bucket { op, res } => self.core_on(own::Event::Reply { op, res }),
                Msg::Fetched { obj, act, data, index } => self.fetched(obj, act, data, index),
                Msg::Indexed { name, new, ok } => {
                    if let Some(o) = self.objects.get_mut(&name) {
                        o.index_busy = false;
                        if ok {
                            o.index_at = new;
                        }
                    }
                    self.reconcile(&name);
                }
                Msg::AlarmsDue { due } => self.alarms_due(due),
                Msg::WireIn { conn, id, name, req } => self.submit(Origin::Wire(conn, id), name, req),
                Msg::WireReply { id, reply } => {
                    if let Some((origin, node, sent, _)) = self.wire_out.remove(&id) {
                        self.measured(node, sent.elapsed());
                        self.deliver(origin, reply)?;
                    }
                }
                Msg::PeerReply { id, res } => {
                    if let Some((op, node, sent, _)) = self.peer_out.remove(&id) {
                        if res.is_some() && node != self.sh.fleet.as_ref().unwrap().cfg.node {
                            self.measured(node, sent.elapsed());
                        }
                        self.core_on(own::Event::Reply { op, res: res.map_or(own::Res::Lost, own::Res::Peer) });
                    }
                }
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
            Origin::Alarm { .. } | Origin::Fleet(_) => Ok(()), // `finish_task` already handled it
            Origin::Wire(conn, id) => {
                if let Some(f) = &self.sh.fleet {
                    f.wire.reply(conn, Frame::Reply { id, status: reply.0, body: reply.1 });
                }
                Ok(())
            }
        }
    }

    /// A request is done: put its cell back (or drop it after a crash), close
    /// its object's transaction, and say what to answer now. `None`: the
    /// answer is held until an fsync covers it.
    fn finish_task(&mut self, t: Task<E>, answer: Result<String, String>) -> Option<(u16, String)> {
        let Task { mut cell, obj, origin } = t;
        let Some(name) = obj else {
            return Some(match answer {
                Ok(a) => {
                    self.pool.push(cell);
                    split_answer(&a)
                }
                Err(trap) => {
                    // Let it crash: this cell is dropped; the pool makes a fresh one when needed.
                    eprintln!("cell trapped: {trap}");
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            });
        };
        if let Origin::Fleet(req) = origin {
            return self.fleet_finish(name, req, cell, answer);
        }
        let o = self.objects.get_mut(&name).expect("object of a running task");
        o.busy = false;
        o.last_used = Instant::now();
        let dirty = std::mem::take(&mut cell.host().alarm_dirty) || matches!(origin, Origin::Alarm { .. });
        let mut ok = false;
        let mut new_alarm = None;
        let mut wrote = false;
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
                    match db.commit() {
                        Ok(w) => {
                            o.cell = Some(cell);
                            ok = true;
                            wrote = w;
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
        // The output gate: an answer leaves only once every commit up to now
        // is on disk, its own included. Reads wait too, since they may have
        // seen a write that isn't on disk yet.
        let mut out = Some(reply);
        if let Some(o) = self.objects.get_mut(&name) {
            if wrote {
                o.committed += 1;
                self.sh.stats.commits.fetch_add(1, Relaxed);
            }
            if ok && o.committed > o.synced {
                o.held.push((o.committed, origin, out.take().unwrap()));
                self.kick_sync(&name);
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
                self.sh.stats.fsyncs.fetch_add(1, Relaxed);
                let job = SyncJob { file, db_path, ckpt, obj: name.to_string(), gen, back: self.me };
                let _ = self.sh.sync.lock().unwrap().send(job);
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
        let ready: Vec<(Origin, (u16, String))> = if ok {
            o.synced = o.synced.max(gen);
            let synced = o.synced;
            let (done, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut o.held).into_iter().partition(|(g, _, _)| *g <= synced);
            o.held = rest;
            done.into_iter().map(|(_, origin, reply)| (origin, reply)).collect()
        } else {
            // After a failed fsync, nothing in memory can be trusted to be on
            // disk (the "fsyncgate" lesson). Fail what's held, and drop the
            // object so the next request reads what's really there.
            eprintln!("object {name:?}: fsync failed, dropping it");
            let o = self.objects.remove(name).unwrap();
            self.sh.stats.objects_live.fetch_sub(1, Relaxed);
            let mut failed: Vec<(Origin, (u16, String))> =
                o.held.into_iter().map(|(_, origin, _)| (origin, (500, "{\"error\":\"storage\"}".to_string()))).collect();
            failed.extend(o.waiting.into_iter().map(|(origin, _)| (origin, (503, "{\"error\":\"storage\"}".to_string()))));
            failed
        };
        for (origin, reply) in ready {
            self.deliver(origin, reply)?;
        }
        self.kick_sync(name);
        Ok(())
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
            drop(db);
            if let Some(o) = self.objects.get_mut(name) {
                o.committed += 1;
            }
            self.kick_sync(name);
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
        let free = |o: &Object<E>| !o.busy && o.waiting.is_empty() && !o.syncing && o.held.is_empty() && !o.checkpointing;
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
        if self.objects.get(name).is_some_and(|o| o.checkpointing || o.busy) {
            return;
        }
        while let Some((origin, req)) = self.objects.get_mut(name).and_then(|o| o.waiting.pop_front()) {
            if let Origin::Local(who) = origin {
                if self.conn(who).is_none() {
                    continue;
                }
            }
            match self.run_object(name, origin, req) {
                Ok(true) => return,
                Ok(false) => {
                    self.fleet_skip(name, origin); // a stale alarm: nothing ran
                    continue;
                }
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
                    "{{\"objects_live\":{},\"objects_opened\":{},\"evictions\":{},\"alarms_fired\":{},\"alarms_failed\":{},\"forwarded\":{},\"commits\":{},\"fsyncs\":{},\"checkpoints\":{},\"fsyncs_slow\":{},\"fsync_max_us\":{},\"peer_acks\":{},\"bucket_acks\":{},\"fences\":{},\"tail_entries\":{}}}",
                    s.objects_live.load(Relaxed),
                    s.objects_opened.load(Relaxed),
                    s.evictions.load(Relaxed),
                    s.alarms_fired.load(Relaxed),
                    s.alarms_failed.load(Relaxed),
                    s.forwarded.load(Relaxed),
                    s.commits.load(Relaxed),
                    s.fsyncs.load(Relaxed),
                    s.checkpoints.load(Relaxed),
                    s.fsyncs_slow.load(Relaxed),
                    s.fsync_max_us.load(Relaxed),
                    s.peer_acks.load(Relaxed),
                    s.bucket_acks.load(Relaxed),
                    s.fences.load(Relaxed),
                    s.tail_entries.load(Relaxed)
                );
                self.queue_reply(who, (200, body));
            }
            Req::NewId => self.queue_reply(who, (200, format!("{{\"id\":\"{}\"}}", uuid_v7()))),
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
        if self.core.is_some() && !matches!(origin, Origin::Fleet(_)) {
            let hops = if matches!(origin, Origin::Wire(..)) { 1 } else { 0 };
            return self.fleet_request(origin, name, req, hops);
        }
        if !self.objects.contains_key(&name) {
            if self.core.is_some() {
                return; // dropped while its request waited; the core failed it
            }
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
            self.objects.insert(name.clone(), Object::new(db));
        }
        let o = self.objects.get_mut(&name).unwrap();
        if o.busy || o.checkpointing {
            o.waiting.push_back((origin, req));
            return;
        }
        match self.run_object(&name, origin, req) {
            Ok(true) => {}
            Ok(false) => self.fleet_skip(&name, origin),
            Err(reply) => {
                let _ = self.deliver(origin, reply);
            }
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

impl<E: Engine> Loop<'_, E> {
    fn core_now(&self) -> u64 {
        self.sh.fleet.as_ref().map_or(0, |f| f.start.elapsed().as_millis() as u64)
    }

    /// Hand the core an event, then do what it says.
    fn core_on(&mut self, ev: own::Event) {
        let now = self.core_now();
        let core = self.core.as_mut().expect("fleet mode");
        let was = core.stats;
        let effects = core.on(now, ev);
        let (st, now) = (&self.sh.stats, core.stats);
        st.peer_acks.fetch_add(now.peer_acks - was.peer_acks, Relaxed);
        st.bucket_acks.fetch_add(now.bucket_acks - was.bucket_acks, Relaxed);
        st.fences.fetch_add(now.fences - was.fences, Relaxed);
        st.tail_entries.fetch_add(now.tail_entries - was.tail_entries, Relaxed);
        for e in effects {
            self.effect(e);
        }
    }

    fn fleet_request(&mut self, origin: Origin, name: String, cell_req: String, hops: u8) {
        self.next_req += 1;
        let req = self.next_req;
        let obj = own::obj_id(&name);
        self.names.insert(obj, name.clone());
        self.freqs.insert(req, FReq { origin, name, cell_req, hops, act: 0, resp: None });
        self.core_on(own::Event::Request { obj, req });
    }

    fn job(&self, job: Job) {
        let _ = self.sh.fleet.as_ref().unwrap().jobs.lock().unwrap().send(job);
    }

    fn effect(&mut self, e: Effect) {
        match e {
            Effect::Call { op, call } => self.job(Job::Call { op, call, back: self.me }),
            Effect::Answer { req, ans } => {
                let Some(fr) = self.freqs.remove(&req) else { return };
                let reply = match ans {
                    own::Answer::Done { .. } => fr.resp.unwrap_or((500, "{\"error\":\"no answer\"}".into())),
                    own::Answer::Failed => (503, "{\"error\":\"retry\"}".into()),
                    own::Answer::Redirect(n) if fr.hops < MAX_HOPS => return self.forward(n, fr),
                    own::Answer::Redirect(_) => (503, "{\"error\":\"retry\"}".into()),
                };
                let _ = self.deliver(fr.origin, reply);
            }
            Effect::Run { obj: _, act, req } => {
                let Some(fr) = self.freqs.get_mut(&req) else { return };
                fr.act = act;
                let (name, cell_req) = (fr.name.clone(), fr.cell_req.clone());
                self.submit(Origin::Fleet(req), name, cell_req);
            }
            Effect::Seal { obj, act } => {
                let Some(name) = self.names.get(&obj).cloned() else { return };
                let Some(o) = self.objects.get(&name) else { return };
                let changes = {
                    let db = o.db.lock().unwrap();
                    let c = db.seal();
                    // Checkpoint right after a seal, never mid-request.
                    if c.is_ok() && !o.busy && db.wants_checkpoint() {
                        let _ = db.checkpoint_local();
                    }
                    c
                };
                match changes {
                    Ok(changes) => {
                        // The index must have an alarm before the segment that sets it
                        // goes up, or a crash could leave an alarm nobody finds.
                        let o = self.objects.get_mut(&name).unwrap();
                        let alarm = o.db.lock().unwrap().alarm();
                        if alarm.is_some() && o.index_at != alarm {
                            o.seal_wait = Some((act, changes));
                            self.reconcile(&name);
                        } else {
                            self.core_on(own::Event::Sealed { obj, act, changes });
                        }
                    }
                    Err(e) => {
                        eprintln!("object {name:?}: seal failed: {e}");
                        self.objects.remove(&name);
                    }
                }
            }
            Effect::Restore { obj, act, segs } => self.job(Job::Fetch { obj, act, segs, back: self.me }),
            Effect::Peer { op, node, call } => {
                let f = self.sh.fleet.as_ref().unwrap();
                let id = f.next_id.fetch_add(1, Relaxed);
                self.peer_out.insert(id, (op, node, Instant::now(), self.forward_limit(node)));
                if node == f.cfg.node {
                    let _ = f.peer_store.lock().unwrap().send((call, PeerBack::Loop(self.me, id)));
                } else {
                    f.wire.send(node, Frame::Peer { id, call }, self.me);
                }
            }
            Effect::Drop { obj } => {
                if let Some(name) = self.names.get(&obj) {
                    if self.objects.remove(name).is_some() {
                        self.sh.stats.objects_live.fetch_sub(1, Relaxed);
                    }
                }
            }
        }
    }

    /// An object's segments arrived: rebuild its database, then serve.
    fn fetched(&mut self, obj: own::Obj, act: Act, data: Result<Vec<Vec<u8>>, String>, index: Vec<i64>) {
        let Some(name) = self.names.get(&obj).cloned() else { return };
        let path = store::path_for(&self.sh.cfg.data, &name);
        if self.objects.remove(&name).is_some() {
            self.sh.stats.objects_live.fetch_sub(1, Relaxed);
        }
        let db = data.and_then(|segs| store::restore(&path, &segs).map_err(|e| e.to_string())).and_then(|()| Db::open(&path).map_err(|e| e.to_string()));
        let ok = match db {
            Ok(db) => {
                self.sh.stats.objects_live.fetch_add(1, Relaxed);
                self.sh.stats.objects_opened.fetch_add(1, Relaxed);
                let mut o = Object::new(db);
                // The newest index entry counts; older ones (from a crash) go.
                let mut index = index;
                index.sort();
                o.index_at = index.pop();
                for old in index {
                    self.job(Job::Index { name: name.clone(), obj, old: Some(old), new: None, back: self.me });
                }
                self.objects.insert(name.clone(), o);
                self.reconcile(&name);
                true
            }
            Err(e) => {
                eprintln!("object {name:?}: restore failed: {e}");
                false
            }
        };
        self.core_on(own::Event::Restored { obj, act, ok });
    }

    /// A request the core ran is done: commit locally (no fsync: the bucket
    /// is what makes it durable), hold the answer, and tell the core.
    fn fleet_finish(&mut self, name: String, req: own::ReqId, cell: Cell<E>, answer: Result<String, String>) -> Option<(u16, String)> {
        let Some(o) = self.objects.get_mut(&name) else { return None };
        o.busy = false;
        o.last_used = Instant::now();
        let (reply, changed) = {
            let db = o.db.lock().unwrap();
            match answer {
                Ok(a) => match db.commit() {
                    Ok(w) => {
                        o.cell = Some(cell);
                        (split_answer(&a), w)
                    }
                    Err(e) => {
                        eprintln!("object {name:?}: commit failed: {e}");
                        db.rollback();
                        ((500, "{\"error\":\"storage\"}".into()), false)
                    }
                },
                Err(trap) => {
                    eprintln!("object {name:?} trapped: {trap}");
                    db.rollback();
                    ((500, format!("{{\"error\":{trap:?}}}")), false)
                }
            }
        };
        let act = self.freqs.get_mut(&req).map(|fr| {
            fr.resp = Some(reply);
            fr.act
        });
        self.reconcile(&name);
        self.next_waiting(&name);
        if let Some(act) = act {
            self.core_on(own::Event::Ran { obj: own::obj_id(&name), act, req, changed });
        }
        None
    }

    /// An alarm request found nothing due: it's done, and changed nothing.
    fn fleet_skip(&mut self, name: &str, origin: Origin) {
        let Origin::Fleet(req) = origin else { return };
        let act = self.freqs.get_mut(&req).map(|fr| {
            fr.resp = Some((200, "{}".into()));
            fr.act
        });
        self.reconcile(name);
        if let Some(act) = act {
            self.core_on(own::Event::Ran { obj: own::obj_id(name), act, req, changed: false });
        }
    }

    /// Move the object's alarm index entry toward what its storage says, one
    /// move at a time, and release a seal that was waiting for it.
    fn reconcile(&mut self, name: &str) {
        let me = self.me;
        let Some(o) = self.objects.get_mut(name) else { return };
        // Mid-request, the alarm may be about to change; look again after.
        if o.busy {
            return;
        }
        let want = o.db.lock().unwrap().alarm();
        if o.index_at == want {
            if let Some((act, changes)) = o.seal_wait.take() {
                self.core_on(own::Event::Sealed { obj: own::obj_id(name), act, changes });
            }
            return;
        }
        if o.index_busy {
            return;
        }
        o.index_busy = true;
        let old = o.index_at;
        self.job(Job::Index { name: name.to_string(), obj: own::obj_id(name), old, new: want, back: me });
    }

    /// Eviction: hand objects that have been idle too long back to the core.
    fn fleet_sweep(&mut self) {
        self.last_sweep = Instant::now();
        let idle = self.sh.cfg.idle;
        let gone: Vec<String> = self
            .objects
            .iter()
            .filter(|(_, o)| !o.busy && o.waiting.is_empty() && !o.index_busy && o.seal_wait.is_none() && o.last_used.elapsed() >= idle)
            .map(|(n, _)| n.clone())
            .collect();
        for name in gone {
            let before = self.objects.len();
            self.core_on(own::Event::Release { obj: own::obj_id(&name) });
            if self.objects.len() < before {
                self.sh.stats.evictions.fetch_add(1, Relaxed);
            }
        }
    }

    /// Due alarms from the index: fire each as an ordinary request, routed to
    /// its owner. A fire that keeps failing backs off (0.5 s, doubling, up to
    /// a minute). ponytail: it never gives up, where Cloudflare stops after 6.
    fn alarms_due(&mut self, due: Vec<(String, i64)>) {
        let now = now_ms();
        for (name, at) in due {
            let e = self.alarm_fire.entry(name.clone()).or_insert((at, 0, 0));
            if e.0 == at && now < e.2 {
                continue; // fired recently; let it run
            }
            let tries = if e.0 == at { e.1 + 1 } else { 0 };
            *e = (at, tries, now + (ALARM_BACKOFF_MS << tries.min(7)).min(60_000));
            self.sh.stats.alarms_fired.fetch_add(1, Relaxed);
            let req = format!("do ALARM / {}", tries.min(ALARM_TRIES));
            let o = owner(&name, self.sh.cfg.threads);
            if o == self.me {
                self.submit(Origin::Alarm { at, tries }, name, req);
            } else {
                let (tx, waker) = &self.sh.peers[o];
                let _ = tx.send(Msg::Run { origin: Origin::Alarm { at, tries }, obj: name, req });
                let _ = waker.wake();
            }
        }
        self.alarm_fire.retain(|_, e| now < e.2 + 600_000);
    }

    /// Send a request to the node that owns its object, over the wire.
    fn forward(&mut self, node: own::Node, fr: FReq) {
        let f = self.sh.fleet.as_ref().unwrap();
        let id = f.next_id.fetch_add(1, Relaxed);
        self.sh.stats.forwarded.fetch_add(1, Relaxed);
        let limit = self.forward_limit(node);
        self.wire_out.insert(id, (fr.origin, node, Instant::now(), limit));
        f.wire.send(node, Frame::Forward { id, name: fr.name, req: fr.cell_req }, self.me);
    }
}

impl<E: Engine> Loop<'_, E> {
    /// A round trip to `node`, in TCP's way (RFC 6298).
    fn measured(&mut self, node: own::Node, d: Duration) {
        let r = d.as_secs_f64();
        let e = self.rtt.entry(node).or_insert((r, r / 2.0));
        e.1 = 0.75 * e.1 + 0.25 * (e.0 - r).abs();
        e.0 = 0.875 * e.0 + 0.125 * r;
    }

    fn forward_limit(&self, node: own::Node) -> Duration {
        match self.rtt.get(&node) {
            Some(&(srtt, var)) => Duration::from_secs_f64(srtt + 4.0 * var).clamp(FORWARD_MIN, FORWARD_MAX),
            None => FORWARD_FIRST,
        }
    }
}

/// Where the wire hands what arrives: forwards to the loop that owns the
/// object, replies to the loop that's waiting.
struct LoopSink {
    loops: Vec<(Sender<Msg>, Arc<Waker>)>,
    threads: usize,
    peer_store: Sender<(own::PeerCall, PeerBack)>,
}

impl wire::Sink for LoopSink {
    fn forward(&self, conn: usize, id: u64, name: String, req: String) {
        let (tx, waker) = &self.loops[owner(&name, self.threads)];
        let _ = tx.send(Msg::WireIn { conn, id, name, req });
        let _ = waker.wake();
    }
    fn reply(&self, back: usize, id: u64, reply: (u16, String)) {
        let (tx, waker) = &self.loops[back];
        let _ = tx.send(Msg::WireReply { id, reply });
        let _ = waker.wake();
    }
    fn peer(&self, conn: usize, id: u64, call: own::PeerCall) {
        let _ = self.peer_store.send((call, PeerBack::Wire(conn, id)));
    }
    fn peer_reply(&self, back: usize, id: u64, res: Option<own::PeerRes>) {
        let (tx, waker) = &self.loops[back];
        let _ = tx.send(Msg::PeerReply { id, res });
        let _ = waker.wake();
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
    // Only standard methods: `ALARM` is how the host runs an object's alarm,
    // and a client must never be able to fire one.
    if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"].contains(&method) {
        return Err(());
    }
    let line = |kind: &str, path: &str| if body.is_empty() { format!("{kind} {method} {path}") } else { format!("{kind} {method} {path} {body}") };
    let r = if path == "/_combd/stats" {
        Req::Stats
    } else if path == "/do/new" {
        Req::NewId
    } else if let Some(rest) = path.strip_prefix("/do/id/") {
        // `/do/id/<uuid>/<rest>`: an object made by `/do/new`. Its name gets an
        // `id:` prefix, so it can never meet an object named the same way.
        let (id, sub) = rest.split_once('/').map_or((rest, "/".to_string()), |(n, s)| (n, format!("/{s}")));
        Req::Object(format!("id:{}", parse_uuid(id).ok_or(())?), line("do", &sub))
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
