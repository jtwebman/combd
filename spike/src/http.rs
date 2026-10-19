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
use crate::app::{App, Hit, Router};
use crate::host::{now_ms, AsyncOp, EvAction, HostState, WsOut};
use crate::store::{self, AlarmIndex, Db};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LISTENER: Token = Token(usize::MAX);
const WAKER: Token = Token(usize::MAX - 1);
const MAX_REQUEST: usize = 1 << 20;
/// A WebSocket's unread bytes: a whole message, and more queued behind it.
/// A client that sends faster than its object keeps up is closed past this.
const MAX_WS_READ: usize = 4 * crate::ws::MAX_MESSAGE;
/// A WebSocket's unsent bytes. A client that reads too slowly is closed past this.
const MAX_WS_WRITE: usize = 8 << 20;
/// A socket's messages queued at its object, before we stop reading it.
const MAX_WS_INFLIGHT: u32 = 64;
/// Slices per round of the run queue, shared between orgs when more than
/// one has work (fewer tasks than this: everyone runs every round).
const ROUND: usize = 64;

/// File names are a hash of the name (`store::path_for`), so this is only a
/// sanity cap: `<org>/<app>/<process>/<id>` fits with room to spare.
const MAX_NAME: usize = 512;
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
    /// Postgres bindings (`pg.rs`), if any.
    pub pg: Option<crate::pg::PgCfg>,
    /// Event cells: requests in flight per instance before another one starts.
    pub ev_max: usize,
    /// The app's env vars, as WASI hands them out: `NAME=value` each.
    pub environ: Arc<Vec<Vec<u8>>>,
    /// Every node's public address, for clients picking the nearest
    /// (`COMBD_PUBLIC_URLS`: `0=http://a:8080,1=http://b:8080`).
    pub public: BTreeMap<u32, String>,
    /// Fleet mode: this node's catalog (pushes reach it over the wire).
    pub catalog: Option<Arc<crate::catnode::CatNode>>,
    /// Fleet mode: the master key's file (`COMBD_MASTER_KEY_FILE`), which
    /// opens the apps' secrets. Every node needs the same one.
    pub master_key: Option<PathBuf>,
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
            pg: None,
            ev_max: num("COMBD_EV_MAX", 32).max(1),
            environ: Arc::default(),
            catalog: None,
            master_key: std::env::var("COMBD_MASTER_KEY_FILE").ok().map(PathBuf::from),
            public: std::env::var("COMBD_PUBLIC_URLS").unwrap_or_default().split(',').filter_map(|kv| kv.split_once('=')).filter_map(|(n, u)| Some((n.trim().parse().ok()?, u.trim().to_string()))).collect(),
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
    uploads: AtomicU64,
    lease_reads: AtomicU64,
    fence_waits: AtomicU64,
    gc_deleted: AtomicU64,
    /// WebSockets open, and messages in from them.
    sockets_open: AtomicU64,
    ws_messages: AtomicU64,
    /// Handshakes another loop accepted and handed to the object's loop.
    ws_handoffs: AtomicU64,
    /// Event cells: instances started, and requests they took.
    ev_instances: AtomicU64,
    ev_requests: AtomicU64,
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
    /// A new object's folders: fsynced too, or a power cut could lose its file.
    dirs: Vec<PathBuf>,
}

/// What the loops share.
struct Shared {
    /// Postgres pools, one per binding (`pg.rs`).
    pg: Option<Arc<crate::pg::Pools>>,
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
    /// This process's boot number, in its heartbeats.
    boot: u64,
    /// Bucket work, done by a pool of threads.
    jobs: Mutex<Sender<Job>>,
    /// The wire to other nodes (mTLS).
    wire: Wire,
    /// This node's peer store (D3), on its own thread.
    peer_store: Mutex<Sender<(own::PeerCall, PeerBack)>>,
    next_id: AtomicU64,
    start: Instant,
    /// When each node was last heard from (its heartbeats).
    heard: Arc<Mutex<HashMap<own::Node, (u64, Instant)>>>,
    /// Where pool messages from other nodes go.
    pools: Arc<std::sync::OnceLock<Arc<crate::pg::Pools>>>,
}

enum Job {
    Call { op: own::OpId, call: own::Call, back: usize },
    /// Fetch an object's segments, and its alarm index entries.
    Fetch { obj: own::Obj, act: Act, segs: Vec<String>, image: Option<String>, back: usize },
    /// Write an object's image (write-once), for the next owner to start
    /// from. No answer: without it, the next one reads the long way.
    Image { obj: own::Obj, act: Act, key: String, entry: Vec<u8>, back: usize },
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
/// peer across the world gets more time than one in the same rack. The
/// floor leaves room for the owner's own work: a fresh object's start is a
/// few bucket calls first, and a bucket can be an ocean away. (A dead owner
/// is found by its silence, not by this.)
const FORWARD_MIN: Duration = Duration::from_secs(5);
const FORWARD_MAX: Duration = Duration::from_secs(30);
const FORWARD_FIRST: Duration = Duration::from_secs(5);

/// Bucket I/O is slow and parallel; these threads do it.
const BUCKET_THREADS: usize = 32;
/// How many times a request may be forwarded between nodes.
const MAX_HOPS: u8 = 1;
/// Times a request that never ran is routed again before the client hears 503.
const MAX_NOT_RUN: u8 = 3;

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
    /// A WebSocket event: its connection, socket id, and whether it's the
    /// handshake (`WSOPEN`, answered with 101 to accept).
    Socket(Who, u64, bool),
    /// An event cell's callback (on instance N): its answers go by `respond`.
    Ev(usize),
    /// A `call` from another process: (the caller's loop, the call's id).
    Call(usize, u64),
    /// A `send`: nobody hears the answer.
    Discard,
    /// An object's beat (`tick_every`): nobody hears the answer either.
    Tick,
}

/// Who waits on a `call`'s answer.
#[derive(Clone, Copy)]
enum CallTo {
    /// An event cell's ticket (instance, generation, ticket).
    Ev(usize, u64, u64),
    /// A parked cell.
    Park(u64),
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
    Fetched { obj: own::Obj, act: Act, data: Result<Vec<Vec<u8>>, String>, index: Vec<i64>, image: Option<String> },
    /// An object's image is written.
    Imaged { obj: own::Obj, act: Act, key: String },
    /// Fleet mode: an alarm index move finished.
    Indexed { name: String, new: Option<i64>, ok: bool },
    /// Fleet mode: due alarms from the index: (name, due).
    AlarmsDue { due: Vec<(String, i64)> },
    /// Fleet mode: another node forwarded us a request.
    WireIn { conn: usize, id: u64, name: String, req: String },
    /// Fleet mode: a heartbeat from `node`.
    Heard { node: own::Node },
    /// Fleet mode: the answer to a request we forwarded.
    WireReply { id: u64, reply: (u16, String) },
    /// Fleet mode: the answer to a peer call (`None`: lost).
    PeerReply { id: u64, res: Option<own::PeerRes> },
    /// A WebSocket handshake for an object you own: take the connection.
    Adopt { s: TcpStream, rbuf: Vec<u8>, name: String, path: String, key: String },
    /// Fleet mode: another node's object sends on a socket we hold.
    WsPush { name: String, sock: u64, op: u8, payload: Vec<u8> },
    /// Fleet mode: a push of ours couldn't reach the socket's node.
    WsLost { name: String, sock: u64 },
    /// A database call a parked cell waits on is done.
    DbDone { park: u64, done: DbDone },
    /// An event cell's async call is done: (instance, its generation, ticket).
    EvDone { inst: usize, gen: u64, ticket: u64, done: DbDone },
    /// The answer to a `call` this loop made.
    CallDone { id: u64, reply: (u16, String) },
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

/// What a database call came back with.
enum DbDone {
    Answer(Vec<u8>),
    /// A connection was leased, or why not.
    Pinned(Result<crate::pg::Lease, String>),
}

/// A leased Postgres connection a cell holds.
struct PgHandle {
    lease: crate::pg::Lease,
    /// The object it belongs to (it outlives requests), or `None` for a
    /// stateless request's (it closes when the request ends).
    obj: Option<String>,
}

/// Fleet mode: a request the decision core knows about.
struct FReq {
    origin: Origin,
    name: String,
    cell_req: String,
    hops: u8,
    act: Act,
    /// The answer, held until the core says it's proven, and the WebSocket
    /// sends that wait with it.
    resp: Option<(u16, String)>,
    ws: Vec<WsOut>,
    /// Times it was routed again after "never ran".
    retries: u8,
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
    /// Set once the connection asked to be a WebSocket.
    ws: Option<WsConn>,
}

/// A WebSocket. It belongs to an object, but outlives it: an idle object
/// leaves memory (hibernates) while its sockets stay open, and the next
/// message brings it back.
struct WsConn {
    obj: String,
    id: u64,
    key: String,
    dec: crate::ws::Decoder,
    /// The handshake is done (the object answered 101).
    open: bool,
    /// We've sent a close, or it's gone: no more events for the object.
    gone: bool,
    /// Messages handed to the object and not done yet.
    inflight: u32,
}

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

fn fresh<E: Engine>(eng: &E, app: &App<E::Program>, proc: usize, environ: &Arc<Vec<Vec<u8>>>) -> std::io::Result<Cell<E>> {
    let mut vm = eng.spawn(&app.progs[app.procs[proc].prog]).map_err(std::io::Error::other)?;
    let mut st = HostState { environ: environ.clone(), ..HostState::default() };
    crate::init(&mut vm, &mut st).map_err(std::io::Error::other)?;
    Ok(Cell { vm, st, proc })
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
}

/// A Durable Object in memory.
struct Object<E: Engine> {
    /// An image taken at a seal, until the core says to put it.
    image: Option<(String, Vec<u8>)>,
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
    /// Fleet mode: the alarm index entry, as far as we know; one move at a
    /// time; and a seal held until the index has the alarm it carries.
    index_at: Option<i64>,
    index_busy: bool,
    seal_wait: Option<(Act, Vec<u8>)>,
    /// Its WebSocket list (`_combd_sockets`), read once it's needed.
    socks: Option<BTreeSet<u64>>,
    /// Postgres handles it holds (pinned connections), and when one was last used.
    pg: Vec<u64>,
    pg_used: Instant,
}

impl<E: Engine> Object<E> {
    fn new(db: Db) -> Object<E> {
        Object {
            image: None,
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
            index_at: None,
            index_busy: false,
            seal_wait: None,
            socks: None,
            pg: vec![],
            pg_used: Instant::now(),
        }
    }
}

/// A burst leaves a pool as big as it was: keep only what's been used in
/// the last `idle` (the newest stays, warm for the next request).
fn trim_idle<T>(pool: &mut Vec<(T, Instant)>, idle: Duration) {
    let stale = pool.iter().take(pool.len().saturating_sub(1)).take_while(|(_, at)| at.elapsed() >= idle).count();
    pool.drain(..stale);
}

/// A parsed request.
enum Req {
    /// A WebSocket handshake for an object: name, path, `sec-websocket-key`.
    Upgrade(String, String, String),
    /// A worker process's request.
    Cell(usize, String),
    Object(String, String),
    Stats,
    /// A manifest's routes don't match.
    NotFound,
    /// The fleet's nodes and their public addresses (for clients choosing
    /// the nearest), and a ping to time a round trip.
    Nodes,
    Ping,
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

/// One program, the old fixed routes, no manifest.
pub fn serve<E>(eng: &E, prog: E::Program, port: u16)
where
    E: Engine + Sync,
    E::Program: Sync + Send,
{
    let cfg = Config::from_env();
    let app = App::single(prog, cfg.ev_max);
    serve_with(eng, app, port, cfg)
}

/// The app in `dir`: its manifest, resolved for the local `dev` environment,
/// and its modules. Any mistake and nothing starts.
pub fn serve_app<E>(eng: &E, dir: &std::path::Path, port: u16)
where
    E: Engine + Sync,
    E::Program: Sync + Send,
{
    serve_apps_in(eng, &[("local".into(), dir.to_path_buf())], port)
}

/// Every app under `root`, laid out as `<org>/<app>/` (the org is the
/// tenant: in a fleet it comes from who deployed, here from the folder).
pub fn serve_apps<E>(eng: &E, root: &std::path::Path, port: u16)
where
    E: Engine + Sync,
    E::Program: Sync + Send,
{
    let mut apps = vec![];
    let dirs = |d: &std::path::Path| -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(d).map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
        v.sort();
        v
    };
    for org in dirs(root) {
        for app in dirs(&org) {
            apps.push((org.file_name().unwrap().to_string_lossy().into_owned(), app));
        }
    }
    serve_apps_in(eng, &apps, port)
}

fn serve_apps_in<E>(eng: &E, apps: &[(String, std::path::PathBuf)], port: u16)
where
    E: Engine + Sync,
    E::Program: Sync + Send,
{
    match load_apps(eng, apps) {
        Ok((app, pg)) => {
            let cfg = Config { pg, ..Config::from_env() };
            serve_with(eng, app, port, cfg)
        }
        Err(e) => {
            // Any mistake in any app, and nothing starts.
            eprintln!("combd     {e}");
            std::process::exit(1);
        }
    }
}

/// A node serving the apps deployed to its fleet: the catalog says which
/// version of each is live (and its vars), and whenever this node's copy of
/// the catalog moves, it swaps in what changed. Deploys come in through
/// `deploy_bundle` (the admin API), on any node.
pub fn serve_fleet_apps<E>(eng: &E, port: u16, mut cfg: Config, admin: Option<crate::admin::AdminCfg>) -> Result<(), String>
where
    E: Engine + Sync,
    E::Program: Sync + Send,
{
    let fc = cfg.fleet.as_ref().ok_or("fleet mode needs a bucket (COMBD_S3)")?;
    let s3 = fc.s3.clone();
    // The storage test first (it makes the bucket too): never serve on one
    // that can't do compare-and-swap.
    s3.check().map_err(|e| format!("the bucket at {} failed the storage test: {e}", s3.host))?;
    let peers: Vec<own::Node> = fc.peers.keys().copied().filter(|&n| n != fc.node).collect();
    std::fs::create_dir_all(&cfg.data).map_err(|e| e.to_string())?;
    let cat = crate::catnode::CatNode::start(Arc::new(s3.clone()), cfg.data.join("catalog.snap"), peers);
    cfg.catalog = Some(cat.clone());
    if !cat.ready(Duration::from_secs(60)) {
        return Err("the catalog didn't catch up within 60 s: is the bucket reachable?".into());
    }
    let master = cfg.master_key.as_deref().map(crate::secrets::Key::load).transpose()?;
    let master = master.as_ref();
    let cache = cfg.data.join("apps");
    let mut known = crate::deploy::Known::new();
    let (state, mut gen) = cat.now();
    let first = sync_apps(eng, &s3, &cache, &mut known, &App::empty(), &state, master)?;
    let live = Live::new(first.unwrap_or_else(App::empty));
    let scratch = cfg.data.join("scratch");
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    std::thread::scope(|sc| {
        let (live, stop) = (&live, cfg.stop.clone());
        if let Some(admin) = admin {
            let (s3, stop, cat) = (&s3, cfg.stop.clone(), cat.clone());
            let scratch = scratch.clone();
            sc.spawn(move || {
                let api = NodeApi { eng, live, s3, cat, scratch, master };
                if let Err(e) = crate::admin::serve(admin, &api, &stop) {
                    eprintln!("combd     the admin API stopped: {e}");
                }
            });
        }
        let (s3, cat) = (s3.clone(), cat.clone());
        sc.spawn(move || {
            while !stop.load(Relaxed) {
                // Woken by the catalog when its copy moves (a push, or the
                // slow look at the bucket in case one was missed).
                let (state, g) = cat.wait(gen, Duration::from_secs(1));
                if g == gen {
                    continue;
                }
                gen = g;
                match sync_apps(eng, &s3, &cache, &mut known, &live.now(), &state, master) {
                    Ok(Some(next)) => live.swap(next),
                    Ok(None) => {}
                    Err(e) => eprintln!("combd     deploys: {e} (still serving what's loaded)"),
                }
            }
        });
        serve_live(eng, live, port, cfg);
    });
    Ok(())
}

/// The admin API's work on a node of a fleet.
struct NodeApi<'a, E: Engine> {
    eng: &'a E,
    live: &'a Live<E::Program>,
    s3: &'a crate::s3::S3,
    cat: Arc<crate::catnode::CatNode>,
    scratch: std::path::PathBuf,
    master: Option<&'a crate::secrets::Key>,
}

impl<E: Engine> NodeApi<'_, E> {
    /// The live version's manifest, if one is live.
    fn manifest(&self, state: &crate::catalog::State, org: &str, app: &str) -> Result<Option<crate::manifest::Manifest>, String> {
        let Some(v) = crate::deploy::live(state, org, app) else { return Ok(None) };
        let dir = self.scratch.join(format!("m-{org}-{app}-{v}"));
        crate::deploy::unpack(&crate::deploy::get_version(self.s3, org, app, &v)?, &dir)?;
        let m = crate::manifest::load(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        m
    }
}

impl<E: Engine + Sync> crate::admin::Api for NodeApi<'_, E>
where
    E::Program: Sync + Send,
{
    fn deploy(&self, org: &str, app: &str, bundle: &[u8]) -> Result<String, String> {
        deploy_bundle(self.eng, self.live, &self.cat, self.s3, &self.scratch, self.master, org, app, bundle)
    }

    fn current(&self, org: &str, app: &str) -> Result<Option<String>, String> {
        Ok(crate::deploy::live(&self.cat.now().0, org, app))
    }

    /// An earlier version, checked again (vars may have changed since).
    fn rollback(&self, org: &str, app: &str, version: &str) -> Result<(), String> {
        let b = crate::deploy::get_version(self.s3, org, app, version)?;
        deploy_bundle(self.eng, self.live, &self.cat, self.s3, &self.scratch, self.master, org, app, &b).map(|_| ())
    }

    fn remove(&self, org: &str, app: &str) -> Result<(), String> {
        self.cat.submit(&format!("{org}/{app}"), vec![]).map(|_| ())
    }

    /// Set (or, with `null`, unset) vars. With a version live, each is
    /// checked against its declared type, so a bad value never reaches it.
    fn set_vars(&self, org: &str, app: &str, set: std::collections::BTreeMap<String, serde_json::Value>) -> Result<(), String> {
        let state = self.cat.now().0;
        let manifest = self.manifest(&state, org, app)?;
        // ponytail: two vars changes at once on two nodes: the later one
        // wins whole (each writes the full set). Merge in the core if needed.
        let mut vars = crate::deploy::vars(&state, org, app);
        for (k, v) in set {
            if v.is_null() {
                vars.remove(&k);
                continue;
            }
            let v = match &manifest {
                Some(m) => m.var_value(&k, v)?,
                None => v,
            };
            vars.insert(k, v);
        }
        self.cat.submit(&format!("{org}/{app}#vars"), serde_json::to_vec(&vars).unwrap()).map(|_| ())
    }

    /// Set (or, with `None`, delete) a secret, sealed before it's written.
    /// With a version live, it must be declared and valid for its type, and
    /// one it requires can't be deleted.
    fn set_secret(&self, org: &str, app: &str, name: &str, value: Option<&[u8]>) -> Result<(), String> {
        use crate::secrets::{key_entry, new_key, open_key, seal, secret_entry, valid_name, MAX_VALUE};
        if !valid_name(name) {
            return Err("secret names are a-z, 0-9, -, _ and ., up to 128".into());
        }
        let master = self.master.ok_or("this node has no master key (COMBD_MASTER_KEY_FILE), so it can't take secrets")?;
        let value = value.map(|v| std::str::from_utf8(v).map_err(|_| "a secret is text (UTF-8)")).transpose()?;
        if value.is_some_and(|v| v.len() > MAX_VALUE) {
            return Err(format!("a secret is at most {MAX_VALUE} bytes"));
        }
        let state = self.cat.now().0;
        if let Some(m) = self.manifest(&state, org, app)? {
            m.secret_value(name, value)?;
        }
        let Some(value) = value else { return self.cat.submit(&secret_entry(org, app, name), vec![]).map(|_| ()) };
        let key = match open_key(master, &state, org, app)? {
            Some(k) => k,
            None => {
                let (k, sealed) = new_key(master, org, app);
                match self.cat.submit(&key_entry(org, app), sealed) {
                    Ok(_) => k,
                    // Another node made it first: use theirs.
                    Err(_) => open_key(master, &self.cat.now().0, org, app)?.ok_or("couldn't make the app's data key: try again")?,
                }
            }
        };
        self.cat.submit(&secret_entry(org, app, name), seal(&key, org, app, name, value)).map(|_| ())
    }
}

/// What changed in the catalog since `known`, as a new app (`None`: nothing).
fn sync_apps<E: Engine>(eng: &E, s3: &crate::s3::S3, cache: &std::path::Path, known: &mut crate::deploy::Known, now: &App<E::Program>, state: &crate::catalog::State, master: Option<&crate::secrets::Key>) -> Result<Option<App<E::Program>>, String> {
    let (changes, gone) = crate::deploy::changes(state, s3, cache, known, master)?;
    if changes.is_empty() && gone.is_empty() {
        return Ok(None);
    }
    // Take down what's gone, then each change on its own: one app that
    // won't load is skipped (and tried again next time), not the rest.
    let mut next = now.with_apps(&[], &gone, |w| eng.load(w).map_err(|e| e.to_string()))?;
    for g in &gone {
        known.remove(g);
    }
    let (mut errs, mut applied) = (vec![], !gone.is_empty());
    for c in &changes {
        let one = c.env.as_ref().map_err(|e| e.clone()).and_then(|env| load_one(&c.dir, env)).and_then(|(m, r)| {
            if m.app != c.app {
                return Err(format!("its manifest says it's {:?}", m.app));
            }
            let a = crate::app::AppDir { org: &c.org, dir: &c.dir, resolved: &r, app: &m.app, domains: &m.domains };
            next.with_apps(&[a], &[], |w| eng.load(w).map_err(|e| e.to_string()))
        });
        match one {
            Ok(n) => {
                next = n;
                applied = true;
                known.insert((c.org.clone(), c.app.clone()), (c.version.clone(), c.raw.clone()));
            }
            Err(e) => errs.push(format!("{}/{}: {e}", c.org, c.app)),
        }
    }
    if !errs.is_empty() {
        eprintln!("combd     deploys skipped for now: {}", errs.join("; "));
    }
    if !applied {
        return Ok(None);
    }
    Ok(Some(next))
}

/// An app's manifest in `dir`, resolved with `env`.
fn load_one(dir: &std::path::Path, env: &crate::manifest::EnvValues) -> Result<(crate::manifest::Manifest, crate::manifest::Resolved), String> {
    let m = crate::manifest::load(dir)?.ok_or("no manifest")?;
    let r = m.resolve(env)?;
    Ok((m, r))
}

/// Deploy `bundle` as `<org>/<app>`: everything is checked first (it loads,
/// its vars resolve), the version goes up, and then the catalog takes it:
/// its write checks the domains again, against the log's head, so two
/// deploys on two nodes can never both claim one. Every node picks it up.
#[allow(clippy::too_many_arguments)]
pub fn deploy_bundle<E: Engine>(eng: &E, live: &Live<E::Program>, cat: &crate::catnode::CatNode, s3: &crate::s3::S3, scratch: &std::path::Path, master: Option<&crate::secrets::Key>, org: &str, app: &str, bundle: &[u8]) -> Result<String, String> {
    let ident = |s: &str| !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !ident(org) || !ident(app) {
        return Err("org and app: lowercase letters, digits, - and _".into());
    }
    let v = crate::deploy::version_of(bundle);
    let dir = scratch.join(format!("{org}-{app}-{v}"));
    crate::deploy::unpack(bundle, &dir)?;
    let checked = (|| {
        let (m, r) = load_one(&dir, &crate::deploy::env(&cat.now().0, master, org, app)?)?;
        if m.app != app {
            return Err(format!("its manifest says it's {:?}, not {app:?}", m.app));
        }
        let a = crate::app::AppDir { org, dir: &dir, resolved: &r, app: &m.app, domains: &m.domains };
        live.now().with_apps(&[a], &[], |w| eng.load(w).map_err(|e| e.to_string()))?;
        Ok(m.domains.clone())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    let domains = checked.map_err(|e| format!("{org}/{app}: {e}"))?;
    crate::deploy::put_version(s3, org, app, bundle)?;
    cat.submit(&format!("{org}/{app}"), crate::deploy::live_value(&v, &domains)).map_err(|e| format!("{org}/{app}: {e}"))?;
    Ok(v)
}

/// Deploy the app in `dir` (org `org`) onto a running node: checked and
/// loaded in full first, so a bad version never goes live and the old one
/// keeps serving.
pub fn deploy_dir<E: Engine>(eng: &E, live: &Live<E::Program>, org: &str, dir: &std::path::Path) -> Result<(), String> {
    let at = format!("{org}/{}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let m = crate::manifest::load(dir).map_err(|e| format!("{at}: {e}"))?.ok_or(format!("{at}: no manifest"))?;
    let r = m.resolve(&crate::manifest::load_dev(dir).map_err(|e| format!("{at}: {e}"))?).map_err(|e| format!("{at}: {e}"))?;
    let a = crate::app::AppDir { org, dir, resolved: &r, app: &m.app, domains: &m.domains };
    let next = live.now().with_apps(&[a], &[], |w| eng.load(w).map_err(|e| e.to_string()))?;
    live.swap(next);
    Ok(())
}

/// One app in `dir` (org `local`), ready to serve.
pub fn load_app<E: Engine>(eng: &E, dir: &std::path::Path) -> Result<(App<E::Program>, Option<crate::pg::PgCfg>), String> {
    load_apps(eng, &[("local".into(), dir.to_path_buf())])
}

/// Apps (org, folder), each from its manifest resolved for the local `dev`
/// environment: their processes and modules, and their pools (named
/// `<org>/<app>/<pool>`).
pub fn load_apps<E: Engine>(eng: &E, apps: &[(String, std::path::PathBuf)]) -> Result<(App<E::Program>, Option<crate::pg::PgCfg>), String> {
    let mut loaded = vec![];
    for (org, dir) in apps {
        let at = format!("{org}/{}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let m = crate::manifest::load(dir).map_err(|e| format!("{at}: {e}"))?.ok_or(format!("{at}: no manifest (combd.toml, combd.yaml, or combd.json)"))?;
        let r = m.resolve(&crate::manifest::load_dev(dir).map_err(|e| format!("{at}: {e}"))?).map_err(|e| format!("{at}: the dev environment:\n  {e}"))?;
        loaded.push((org.clone(), dir.clone(), m, r));
    }
    let list: Vec<crate::app::AppDir> = loaded.iter().map(|(org, dir, m, r)| crate::app::AppDir { org, dir, resolved: r, app: &m.app, domains: &m.domains }).collect();
    let app = App::from_apps(&list, |w| eng.load(w).map_err(|e| e.to_string()))?;
    let pools: Vec<crate::pg::PoolCfg> = loaded.iter().flat_map(|(org, _, m, r)| crate::pg::PgCfg::from_manifest(r, &format!("{org}/{}", m.app)).map(|c| c.pools).unwrap_or_default()).collect();
    Ok((app, (!pools.is_empty()).then_some(crate::pg::PgCfg { pools })))
}

/// What a node serves right now. A deploy `swap`s in a new `App` (made with
/// `App::with_apps`); each loop picks it up between rounds (within a tick).
pub struct Live<P> {
    app: Mutex<Arc<App<P>>>,
    gen: AtomicU64,
}

impl<P> Live<P> {
    pub fn new(app: App<P>) -> Live<P> {
        Live { app: Mutex::new(Arc::new(app)), gen: AtomicU64::new(0) }
    }

    pub fn now(&self) -> Arc<App<P>> {
        self.app.lock().unwrap().clone()
    }

    pub fn swap(&self, app: App<P>) {
        *self.app.lock().unwrap() = Arc::new(app);
        self.gen.fetch_add(1, Relaxed);
    }
}

/// Serve `app`, fixed: nothing swaps it.
pub fn serve_with<E>(eng: &E, app: App<E::Program>, port: u16, cfg: Config)
where
    E: Engine + Sync,
    E::Program: Sync + Send,
{
    serve_live(eng, &Live::new(app), port, cfg)
}

pub fn serve_live<E>(eng: &E, live: &Live<E::Program>, port: u16, mut cfg: Config)
where
    E: Engine + Sync,
    E::Program: Sync + Send,
{
    let app = live.now();
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
            let ok = no_fsync || (job.file.sync_data().is_ok() && job.dirs.iter().all(|d| std::fs::File::open(d).and_then(|f| f.sync_all()).is_ok()));
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
    let fleet = cfg.fleet.take().map(|fc| start_fleet(fc, &peers, cfg.threads, &cfg.data, cfg.catalog.clone()));
    // Always a set of pools (maybe empty): a deploy can add to it. In a
    // fleet with a catalog, each pool is one for the whole fleet.
    let pool_fleet = fleet.as_ref().zip(cfg.catalog.clone()).map(|(f, cat)| crate::pg::PoolFleet { me: f.cfg.node, wire: f.wire.clone(), cat, heard: f.heard.clone(), lease_ms: f.cfg.lease_ms });
    let pools = Arc::new(crate::pg::Pools::start(&cfg.pg.clone().unwrap_or_default(), pool_fleet, cfg.stop.clone()));
    if let Some(f) = &fleet {
        let _ = f.pools.set(pools.clone());
    }
    let pg = Some(pools);
    let sh = Shared {
        pg,
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
            let app = app.clone();
            sc.spawn(move || {
                let mut lp = Loop {
                    eng,
                    app: app.clone(),
                    live,
                    app_gen: live.gen.load(Relaxed),
                    sh,
                    me,
                    inbox,
                    poll,
                    pools: (0..app.procs.len()).map(|_| vec![]).collect(),
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
                        Core::with_lease(f.cfg.node, own::Bugs::default(), f.cfg.lease_ms).replicate(fleet, f.cfg.replicas).uploads(f.cfg.upload_ms, f.cfg.upload_bytes).read_lease(if f.cfg.read_lease { f.cfg.lease_ms / 2 } else { 0 })
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
                    local_socks: HashMap::new(),
                    next_sock: 0,
                    boot: {
                        let mut b = [0u8; 8];
                        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
                        u64::from_le_bytes(b) ^ now_ms() as u64
                    },
                    ws_gone: HashMap::new(),
                    ws_lost: Default::default(),
                    parked: HashMap::new(),
                    next_park: 0,
                    pg_handles: HashMap::new(),
                    ev_mode: vec![false; app.procs.len()],
                    ev: vec![],
                    ev_gen: 0,
                    next_ev_req: 0,
                    ev_timers: BinaryHeap::new(),
                    calls: HashMap::new(),
                    next_call: 0,
                    ticks: HashMap::new(),
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
    /// The app as of the last round; `live` has the newest.
    app: Arc<App<E::Program>>,
    live: &'a Live<E::Program>,
    app_gen: u64,
    sh: &'a Shared,
    me: usize,
    inbox: Receiver<Msg>,
    poll: Poll,
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
    /// Forwards waiting for the owner: (origin, owner, sent, limit, object).
    wire_out: HashMap<u64, (Origin, own::Node, Instant, Duration, String)>,
    /// Peer calls out: frame id -> the core's op, the node, when sent, and
    /// when it gives up.
    peer_out: HashMap<u64, (own::OpId, own::Node, Instant, Duration)>,
    /// Each peer's round trip: (smoothed, variation), in seconds.
    rtt: HashMap<own::Node, (f64, f64)>,
    /// Alarms loop 0 fired from the index: name -> (due, tries, not again before).
    alarm_fire: HashMap<String, (i64, u32, i64)>,
    last_scan: Instant,
    last_tick: Instant,
    /// WebSocket connections this loop holds: socket id -> connection. They
    /// stay while their object is out of memory, or runs on another node.
    local_socks: HashMap<u64, Who>,
    next_sock: u64,
    /// Random bits for socket ids, so a restart never reuses one.
    boot: u64,
    /// Sockets that are gone, per object, until a committed request has
    /// taken them off the object's list (`_combd_sockets`).
    ws_gone: HashMap<String, BTreeSet<u64>>,
    /// Lost sockets we've told their object about already.
    ws_lost: std::collections::HashSet<u64>,
    /// Cells waiting on the database, by park number; and the Postgres
    /// handles cells on this loop hold.
    parked: HashMap<u64, (Task<E>, bool)>,
    next_park: u64,
    pg_handles: HashMap<u64, PgHandle>,
    /// Event cells: which processes are (their module exports
    /// `on_request`), their instances, and their timers (due, instance,
    /// generation, ticket).
    ev_mode: Vec<bool>,
    ev: Vec<Option<EvInst<E>>>,
    ev_gen: u64,
    next_ev_req: u64,
    ev_timers: BinaryHeap<Reverse<(Instant, usize, u64, u64)>>,
    /// `call`s out: id -> who waits, and when it gives up.
    calls: HashMap<u64, (CallTo, Instant)>,
    next_call: u64,
    /// Objects' beats: name -> (every, next, count).
    ticks: HashMap<String, (Duration, Instant, u64)>,
}

/// Fleet mode: check the bucket, then start the bucket pool and the wire.
/// Never two nodes with one id: they'd fight over everything it owns. So
/// before a node serves, it asks every peer when it last heard this id, and
/// from which process (`boot`). Heard lately from another process: someone
/// else has the id, or this node's last process only just died. Wait for it
/// to go quiet (a restart takes up to a lease longer); still heard after
/// three leases, it's a live duplicate: don't start.
/// ponytail: the check's own short-lived wire stays (one thread, idle).
pub fn node_id_free(fc: &FleetCfg, boot: u64) -> Result<(), String> {
    let peers: Vec<own::Node> = fc.peers.keys().copied().filter(|&n| n != fc.node).collect();
    if peers.is_empty() {
        return Ok(());
    }
    struct Ask(Mutex<Sender<(u16, String)>>);
    impl wire::Sink for Ask {
        fn forward(&self, _: usize, _: u64, _: String, _: String) {}
        fn reply(&self, _: usize, _: u64, reply: (u16, String)) {
            let _ = self.0.lock().unwrap().send(reply);
        }
        fn peer(&self, _: usize, _: u64, _: own::PeerCall) {}
        fn peer_reply(&self, _: usize, _: u64, _: Option<own::PeerRes>) {}
        fn ws_push(&self, _: String, _: u64, _: u8, _: Vec<u8>) {}
        fn ws_lost(&self, _: String, _: u64) {}
        fn catalog(&self, _: u64, _: Vec<u8>) {}
        fn heartbeat(&self, _: own::Node, _: u64) {}
        fn pool(&self, _: Vec<u8>) {}
    }
    let tls = match (&fc.wire_certs, fc.insecure) {
        (Some(dir), _) => Some(wire::load_tls(dir, fc.node)?),
        (None, true) => None,
        (None, false) => return Err("the wire needs certificates (COMBD_WIRE_CERTS) or COMBD_WIRE_INSECURE=1".into()),
    };
    let (tx, rx) = channel();
    let w = wire::start(0, fc.peers.clone(), tls, Ask(Mutex::new(tx))).map_err(|e| format!("the wire: {e}"))?;
    let t = Instant::now();
    loop {
        for &p in &peers {
            w.send(p, Frame::Forward { id: rand_u64(), name: "_combd/node".into(), req: fc.node.to_string() }, 0);
        }
        let mut busy = false;
        let deadline = Instant::now() + Duration::from_millis((fc.lease_ms / 3).max(200));
        while let Ok((200, body)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            busy |= v["boot"].as_u64().is_some_and(|b| b != boot) && v["ms"].as_u64().is_some_and(|ms| ms < fc.lease_ms);
        }
        if !busy {
            return Ok(());
        }
        if t.elapsed() > Duration::from_millis(fc.lease_ms * 3) {
            return Err(format!("node id {} is already in use by a running node: give this one another COMBD_NODE", fc.node));
        }
        std::thread::sleep(Duration::from_millis(fc.lease_ms / 3));
    }
}

fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    u64::from_le_bytes(b)
}

fn start_fleet(fc: FleetCfg, loops: &[(Sender<Msg>, Arc<Waker>)], threads: usize, data: &std::path::Path, catalog: Option<Arc<crate::catnode::CatNode>>) -> FleetShared {
    // The startup storage test: never serve on a bucket that can't do CAS.
    if let Err(e) = fc.s3.check() {
        eprintln!("combd     the bucket at {} failed the storage test: {e}", fc.s3.host);
        std::process::exit(1);
    }
    println!("combd     node {} in a fleet, bucket {} passed the storage test", fc.node, fc.s3.host);
    let boot = rand_u64();
    if let Err(e) = node_id_free(&fc, boot) {
        eprintln!("combd     {e}");
        std::process::exit(1);
    }
    let (tx, rx) = channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..BUCKET_THREADS {
        let (rx, loops, s3) = (rx.clone(), loops.to_vec(), fc.s3.clone());
        std::thread::spawn(move || loop {
            let Ok(job) = rx.lock().unwrap().recv() else { return };
            let (back, msg) = match job {
                Job::Call { op, call, back } => (back, Msg::Bucket { op, res: crate::fleet::run_call(&s3, &call) }),
                Job::Fetch { obj, act, segs, image, back } => {
                    let index = s3.list(&alarm_prefix(obj)).unwrap_or_default().iter().filter_map(|k| k.rsplit('/').next()?.parse().ok()).collect();
                    (back, Msg::Fetched { obj, act, data: crate::fleet::fetch(&s3, &segs), index, image })
                }
                Job::Image { obj, act, key, entry, back } => match s3.put(&key, &own::frame(&[entry]), &crate::s3::Cond::IfAbsent) {
                    Ok(Ok(_)) => (back, Msg::Imaged { obj, act, key }),
                    Ok(Err(_)) => {
                        eprintln!("{key}: an image is there already");
                        continue;
                    }
                    Err(e) => {
                        eprintln!("{key}: image not written: {e}");
                        continue;
                    }
                },
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
    // The peer store first: the wire hands it calls as soon as it's up. If
    // this node has run before (its lease is in the bucket; if we can't
    // tell, assume so) but its store is gone, the disk was lost: the store
    // comes back wiped (see `own::PeerStore`). Write the lease now, before
    // serving anything, so a node that served always has one.
    let lease = fc.s3.get(&own::lease_key(fc.node));
    let ran_before = !matches!(lease, Ok(None));
    let cur = match &lease {
        Ok(Some((b, _))) => std::str::from_utf8(b).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0),
        _ => 0,
    };
    if let Err(e) = fc.s3.put(&own::lease_key(fc.node), (cur + 1).to_string().as_bytes(), &crate::s3::Cond::Any) {
        eprintln!("combd     can't write this node's lease: {e}");
        std::process::exit(1);
    }
    let mut disk = DiskPeers::open(&data.join("peers"), ran_before).unwrap_or_else(|e| {
        eprintln!("combd     can't open the peer store: {e}");
        std::process::exit(1);
    });
    // Read leases: half a lease, so a takeover waits at most that much more.
    disk.promise(fc.lease_ms / 2);
    let (ptx, prx) = channel();
    let heard: Arc<Mutex<HashMap<own::Node, (u64, Instant)>>> = Arc::default();
    let wire_cell: Arc<std::sync::OnceLock<Wire>> = Arc::default();
    let pools: Arc<std::sync::OnceLock<Arc<crate::pg::Pools>>> = Arc::default();
    let sink = LoopSink { loops: loops.to_vec(), threads, peer_store: ptx.clone(), catalog: catalog.clone(), heard: heard.clone(), wire: wire_cell.clone(), pools: pools.clone() };
    let wire = wire::start(fc.wire_port, fc.peers.clone(), tls, sink).expect("start the wire");
    // The catalog started first (deploys are read before serving); now it
    // can push.
    if let Some(c) = &catalog {
        let _ = c.wire.set(wire.clone());
    }
    let _ = wire_cell.set(wire.clone());
    // Heartbeats go from loop 0's own tick (`heartbeat`), not a thread of
    // their own: if the event loops stop or stall, so do the heartbeats, and
    // the other nodes take over. (A thread of its own kept a stopped node
    // "alive" forever.)
    let (w, ls, s3) = (wire.clone(), loops.to_vec(), fc.s3.clone());
    let learn = move |obj| match s3.get(&own::own_key(obj)) {
        Ok(Some((b, _))) => match own::Val::decode(&own::own_key(obj), &b) {
            Some(own::Val::Rec(r)) => Some(r.epoch),
            _ => None,
        },
        Ok(None) => Some(0),
        Err(_) => None,
    };
    crate::fleet::serve_peers(disk, prx, learn, move |back, res| match back {
        // No answer (the bucket was out of reach): the caller times out.
        PeerBack::Wire(conn, id) => {
            if let Some(res) = res {
                w.reply(conn, Frame::PeerReply { id, res });
            }
        }
        PeerBack::Loop(lp, id) => {
            let (tx, waker) = &ls[lp];
            let _ = tx.send(Msg::PeerReply { id, res });
            let _ = waker.wake();
        }
    });
    if fc.replicas > 0 {
        println!("combd     D3: {} peers per object", fc.replicas);
    }
    FleetShared {
        cfg: fc,
        boot,
        jobs: Mutex::new(tx),
        wire,
        peer_store: Mutex::new(ptx),
        next_id: AtomicU64::new(1),
        start: Instant::now(),
        heard,
        pools,
    }
}

impl<E: Engine> Loop<'_, E> {
    fn run(&mut self, mut listener: TcpListener) -> std::io::Result<()> {
        let mut events = Events::with_capacity(1024);
        self.poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
        self.warm(0)?;
        // Alarms from before a restart.
        let threads = self.sh.cfg.threads;
        for (name, at) in self.sh.index.lock().unwrap().all().unwrap_or_default() {
            if owner(&name, threads) == self.me {
                self.alarms.push(Reverse((at, name)));
            }
        }
        let mut touched: Vec<usize> = vec![];
        while !self.sh.cfg.stop.load(Relaxed) {
            self.pick_up_app()?;
            // Never block while requests are waiting to run.
            let timeout = if self.tasks.is_empty() {
                let next = self.alarms.peek().map(|Reverse((at, _))| Duration::from_millis((at - now_ms()).max(0) as u64));
                let timer = self.ev_timers.peek().map(|Reverse((at, ..))| *at).into_iter().chain(self.ticks.values().map(|t| t.1)).min().map(|at| at.saturating_duration_since(Instant::now()));
                let next = match (next, timer) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
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
                let late: Vec<u64> = self.wire_out.iter().filter(|(_, (_, _, t, limit, _))| t.elapsed() >= *limit).map(|(id, _)| *id).collect();
                for id in late {
                    let (origin, node, _, limit, name) = self.wire_out.remove(&id).unwrap();
                    eprintln!("object {name:?}: forwarded to node {node}, no answer in {limit:?}");
                    self.deliver(origin, (503, "{\"error\":\"owner didn't answer\"}".into()))?;
                }
                let late: Vec<u64> = self.peer_out.iter().filter(|(_, (_, _, t, limit))| t.elapsed() >= *limit).map(|(id, _)| *id).collect();
                for id in late {
                    let (op, ..) = self.peer_out.remove(&id).unwrap();
                    self.core_on(own::Event::Reply { op, res: own::Res::Lost });
                }
                // Every loop ticks its core (to retry quiet peers); only
                // loop 0 sends the node's heartbeats.
                let renew = self.sh.fleet.as_ref().map_or(own::RENEW_MS, |f| f.cfg.lease_ms / 3);
                if self.last_tick.elapsed() >= Duration::from_millis(renew) {
                    self.last_tick = Instant::now();
                    if self.me == 0 {
                        self.heartbeat();
                    }
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
            self.ev_tick();
            self.fire_ticks();
            // One round of the run queue: every request gets one slice, but
            // each org gets an equal share of the round, so one busy tenant
            // can't crowd out the rest. An org that needs less than its share
            // gets all it needs; what it leaves goes unspent this round, and
            // the round is shorter (the poll comes sooner).
            let share = self.round_share();
            let app = self.app.clone();
            let mut used: HashMap<&str, usize> = HashMap::new();
            for _ in 0..self.tasks.len() {
                let mut t = self.tasks.pop_front().unwrap();
                if let Some(share) = share {
                    let org = app.tenants[app.procs[t.cell.proc].tenant].org.as_str();
                    let n = used.entry(org).or_default();
                    if *n >= share {
                        self.tasks.push_back(t);
                        continue;
                    }
                    *n += 1;
                }
                let answer = match t.cell.vm.run(crate::SLICE) {
                    Outcome::Yield => {
                        // A compiled cell paused inside a database call: park it.
                        match t.cell.vm.db_pending() {
                            Some(op) => self.park(t, op, false),
                            None => self.tasks.push_back(t),
                        }
                        continue;
                    }
                    Outcome::HostCall { import, args } => match crate::serve(&mut t.cell.vm, import, &args, &mut t.cell.st) {
                        Ok(()) => {
                            match t.cell.st.pending.take() {
                                Some(op) => self.park(t, op, true),
                                None => self.tasks.push_back(t),
                            }
                            continue;
                        }
                        Err(trap) => Err(trap),
                    },
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
        }
        Ok(())
    }

    fn accept(&mut self, listener: &mut TcpListener) -> std::io::Result<()> {
        loop {
            match listener.accept() {
                Ok((s, _)) => {
                    s.set_nodelay(true)?;
                    self.add_conn(s, vec![])?;
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
                Msg::Adopt { s, rbuf, name, path, key } => {
                    let id = self.add_conn(s, rbuf)?;
                    self.upgrade(id, name, path, key);
                }
                Msg::WsPush { name, sock, op, payload } => self.ws_local(&name, sock, op, &payload)?,
                Msg::WsLost { name, sock } => self.socket_lost(&name, sock),
                Msg::DbDone { park, done } => self.db_done(park, done),
                Msg::EvDone { inst, gen, ticket, done } => self.ev_done(inst, gen, ticket, done),
                Msg::CallDone { id, reply } => self.call_done(id, reply),
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
                Msg::Fetched { obj, act, data, index, image } => self.fetched(obj, act, data, index, image),
                Msg::Imaged { obj, act, key } => self.core_on(own::Event::Imaged { obj, act, key }),
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
                Msg::Heard { node } => {
                    if self.core.is_some() {
                        self.core_on(own::Event::Heard { node });
                    }
                }
                Msg::WireReply { id, reply } => {
                    if let Some((origin, node, sent, ..)) = self.wire_out.remove(&id) {
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
            Origin::Ev(_) | Origin::Discard | Origin::Tick => Ok(()),
            Origin::Call(l, id) if l == self.me => {
                self.call_done(id, reply);
                Ok(())
            }
            Origin::Call(l, id) => {
                let (tx, waker) = &self.sh.peers[l];
                let _ = tx.send(Msg::CallDone { id, reply });
                waker.wake()
            }
            Origin::Socket(who, sock, open) => {
                let Some(c) = self.conn(who) else { return Ok(()) };
                let Some(w) = c.ws.as_mut() else { return Ok(()) };
                if !open {
                    // An event is done: read on if we'd paused.
                    w.inflight = w.inflight.saturating_sub(1);
                    return self.ws_read(who.0);
                }
                if reply.0 == 101 && !w.gone {
                    // The object took it: finish the handshake.
                    w.open = true;
                    let accept = crate::ws::accept(&w.key);
                    c.wbuf.extend_from_slice(accept.as_bytes());
                    self.sh.stats.sockets_open.fetch_add(1, Relaxed);
                    return self.ws_read(who.0);
                }
                // Refused: answer it as plain HTTP, and close.
                w.gone = true;
                let _ = write!(c.wbuf, "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", reply.0, reply.1.len());
                c.wbuf.extend_from_slice(reply.1.as_bytes());
                c.close_after = true;
                self.local_socks.remove(&sock);
                self.settle(who.0)
            }
        }
    }

    /// An answer is ready (after the output gate): send it, then do the
    /// WebSocket sends that waited with it.
    fn release(&mut self, name: Option<&str>, origin: Origin, reply: (u16, String), ws: Vec<WsOut>) -> std::io::Result<()> {
        self.deliver(origin, reply)?;
        match name {
            Some(n) if !ws.is_empty() => self.apply_ws(n, ws),
            _ => Ok(()),
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
                let inst = EvInst { gen: self.ev_gen, proc, cell: Some(fresh(self.eng, &self.app, proc, &self.environ_for(proc))?), reqs: HashMap::new(), tickets: HashMap::new(), queue: VecDeque::new(), running: None };
                self.sh.stats.ev_instances.fetch_add(1, Relaxed);
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
        self.sh.stats.ev_requests.fetch_add(1, Relaxed);
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
        let t = Task { origin: Origin::Ev(i), cell, obj: None, ws_open: None, ws_drop: vec![], pg: vec![], ev: Some(i) };
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
            let _ = self.deliver(origin, reply);
        }
        for h in closes {
            self.pg_close(h);
        }
        for (req, ticket, op) in ops {
            self.ev_start(i, gen, req, ticket, op);
        }
        for m in sends {
            if let Err(e) = self.start_call(from, &m, None) {
                eprintln!("send: {e}");
            }
        }
        self.ev_reap(i);
        self.ev_kick(i);
    }

    /// Start an async call for an event cell's request.
    fn ev_start(&mut self, i: usize, gen: u64, req: u64, ticket: u64, op: AsyncOp) {
        let (tx, waker) = self.sh.peers[self.me].clone();
        let reply = move |done: DbDone| {
            let _ = tx.send(Msg::EvDone { inst: i, gen, ticket, done });
            let _ = waker.wake();
        };
        let binding = |req: &[u8]| serde_json::from_slice::<serde_json::Value>(req).ok().and_then(|v| v["db"].as_str().map(String::from));
        let proc = self.ev[i].as_ref().map_or(0, |e| e.proc);
        if let AsyncOp::Call(msg) = &op {
            let from = proc;
            if let Err(e) = self.start_call(from, msg, Some(CallTo::Ev(i, gen, ticket))) {
                reply(DbDone::Answer(json_err(&e)));
            }
            return;
        }
        let Some(pools) = &self.sh.pg else {
            if let AsyncOp::Timer(ms) = op {
                return self.ev_timers.push(Reverse((Instant::now() + Duration::from_millis(ms), i, gen, ticket)));
            }
            return reply(DbDone::Answer(json_err("no database bindings are set up")));
        };
        match op {
            AsyncOp::Timer(ms) => self.ev_timers.push(Reverse((Instant::now() + Duration::from_millis(ms), i, gen, ticket))),
            AsyncOp::Call(_) => unreachable!("handled above"),
            AsyncOp::PgOpen(r) => match binding(&r).filter(|b| pools.has(&self.app.pool_key(proc, b))).map(|b| self.may_lease(proc, &b).map(|()| self.app.pool_key(proc, &b))) {
                Some(Ok(b)) => pools.lease(&b, Box::new(move |r| reply(DbDone::Pinned(r)))),
                Some(Err(e)) => reply(DbDone::Answer(json_err(&e))),
                None => reply(DbDone::Answer(json_err("pg_open_async: name a database binding (\"db\")"))),
            },
            AsyncOp::PgQuery(0, r) => match binding(&r).map(|b| self.may_lease(proc, &b).map(|()| self.app.pool_key(proc, &b))) {
                Some(Ok(b)) => pools.query(&b, r, Box::new(move |a| reply(DbDone::Answer(a)))),
                Some(Err(e)) => reply(DbDone::Answer(json_err(&e))),
                None => reply(DbDone::Answer(json_err("pg_query_async: name a database binding (\"db\"), or use a handle"))),
            },
            AsyncOp::PgQuery(h, r) => {
                let owned = self.ev[i].as_ref().and_then(|e| e.reqs.get(&req)).is_some_and(|q| q.handles.contains(&h));
                match self.pg_handles.get(&h).filter(|_| owned) {
                    Some(ph) => ph.lease.query(r, Box::new(move |a| reply(DbDone::Answer(a)))),
                    None => reply(DbDone::Answer(json_err(&format!("pg_query_async: no such handle {h}")))),
                }
            }
        }
    }

    /// An async call is back: queue its event for the instance.
    fn ev_done(&mut self, i: usize, gen: u64, ticket: u64, done: DbDone) {
        let live = self.ev.get(i).and_then(Option::as_ref).is_some_and(|e| e.gen == gen);
        let req = if live { self.ev[i].as_mut().unwrap().tickets.remove(&ticket) } else { None };
        let alive_req = req.filter(|r| self.ev[i].as_ref().is_some_and(|e| e.reqs.contains_key(r)));
        let payload = match done {
            DbDone::Answer(a) => a,
            DbDone::Pinned(Err(e)) => json_err(&e),
            DbDone::Pinned(Ok(lease)) => {
                let Some(r) = alive_req else {
                    // Its request (or instance) is gone: give it straight back.
                    return lease.release();
                };
                self.next_park += 1;
                let h = self.next_park << 8 | self.me as u64;
                self.pg_handles.insert(h, PgHandle { lease, obj: None });
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
                let _ = self.deliver(q.origin, (500, "{\"error\":\"the cell crashed\"}".into()));
            }
            for h in q.handles {
                self.pg_close(h);
            }
        }
    }

    /// Timers that are due, calls and requests past their time.
    fn ev_tick(&mut self) {
        let now = Instant::now();
        let late: Vec<u64> = self.calls.iter().filter(|(_, (_, at))| *at <= now).map(|(&id, _)| id).collect();
        for id in late {
            let (to, _) = self.calls.remove(&id).unwrap();
            self.call_reply(to, json_err("timeout"));
        }
        while self.ev_timers.peek().is_some_and(|Reverse((at, ..))| *at <= now) {
            let Reverse((_, i, gen, ticket)) = self.ev_timers.pop().unwrap();
            self.ev_done(i, gen, ticket, DbDone::Answer(vec![]));
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
                let _ = self.deliver(origin, (504, "{\"error\":\"the cell didn't answer in time\"}".into()));
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
        Some(o)
    }

    /// Close a database handle: its connection is reset and goes back.
    fn pg_close(&mut self, h: u64) {
        let Some(ph) = self.pg_handles.remove(&h) else { return };
        if let Some(o) = ph.obj.as_ref().and_then(|n| self.objects.get_mut(n)) {
            o.pg.retain(|&x| x != h);
        }
        ph.lease.release();
    }

    /// A cell waits on a database call: hand it to the pool and park the
    /// cell (paused, no CPU) until the answer is back. `interp`: it paused at
    /// a host call (the interpreter); else on its fiber (compiled).
    fn park(&mut self, t: Task<E>, op: crate::host::DbOp, interp: bool) {
        use crate::host::DbOp;
        // A call to another process: nothing to do with the database.
        if let DbOp::Call(msg) = &op {
            self.next_park += 1;
            let park = self.next_park;
            let from = t.cell.proc;
            self.parked.insert(park, (t, interp));
            if let Err(e) = self.start_call(from, msg, Some(CallTo::Park(park))) {
                self.db_done(park, DbDone::Answer(json_err(&e)));
            }
            return;
        }
        let fail = |m: String| json_err(&m);
        let binding = |req: &[u8]| serde_json::from_slice::<serde_json::Value>(req).ok().and_then(|v| v["db"].as_str().map(String::from));
        // May this cell use handle `h`? Its object's, or its request's.
        let owns = |me: &Self, t: &Task<E>, h: u64| match &t.obj {
            Some(n) => me.objects.get(n).is_some_and(|o| o.pg.contains(&h)),
            None => t.pg.contains(&h),
        };
        let Some(pools) = &self.sh.pg else { return self.db_resume(t, interp, 0, fail("no database bindings are set up".into())) };
        self.next_park += 1;
        let park = self.next_park;
        let (tx, waker) = self.sh.peers[self.me].clone();
        let reply = move |done: DbDone| {
            let _ = tx.send(Msg::DbDone { park, done });
            let _ = waker.wake();
        };
        match op {
            DbOp::Call(_) => unreachable!("handled above"),
            DbOp::Open(req) => {
                let proc = t.cell.proc;
                let Some(b) = binding(&req).filter(|b| pools.has(&self.app.pool_key(proc, b))) else { return self.db_resume(t, interp, 0, fail("pg_open: name a database binding (\"db\")".into())) };
                if let Err(e) = self.may_lease(proc, &b) {
                    return self.db_resume(t, interp, 0, fail(e));
                }
                pools.lease(&self.app.pool_key(proc, &b), Box::new(move |r| reply(DbDone::Pinned(r))));
            }
            DbOp::Query(0, req) => {
                let Some(b) = binding(&req) else { return self.db_resume(t, interp, 0, fail("pg_query: name a database binding (\"db\"), or use a handle".into())) };
                if let Err(e) = self.may_lease(t.cell.proc, &b) {
                    return self.db_resume(t, interp, 0, fail(e));
                }
                pools.query(&self.app.pool_key(t.cell.proc, &b), req, Box::new(move |a| reply(DbDone::Answer(a))));
            }
            DbOp::Query(h, req) => {
                if !owns(self, &t, h) {
                    return self.db_resume(t, interp, 0, fail(format!("pg_query: no such handle {h}")));
                }
                let ph = &self.pg_handles[&h];
                if let Some(o) = ph.obj.as_ref().and_then(|n| self.objects.get_mut(n)) {
                    o.pg_used = Instant::now();
                }
                ph.lease.query(req, Box::new(move |a| reply(DbDone::Answer(a))));
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
        }
        self.parked.insert(park, (t, interp));
    }

    /// A database call is back: hand the answer to its cell, and run it again.
    fn db_done(&mut self, park: u64, done: DbDone) {
        let Some((mut t, interp)) = self.parked.remove(&park) else { return };
        let (value, out) = match done {
            DbDone::Answer(a) => (a.len() as u64, a),
            DbDone::Pinned(Err(e)) => (0, json_err(&e)),
            DbDone::Pinned(Ok(lease)) => {
                // A handle id: unique on this loop, never 0.
                self.next_park += 1;
                let h = self.next_park << 8 | self.me as u64;
                self.pg_handles.insert(h, PgHandle { lease, obj: t.obj.clone() });
                match &t.obj {
                    Some(n) => {
                        if let Some(o) = self.objects.get_mut(n) {
                            o.pg.push(h);
                            o.pg_used = Instant::now();
                        }
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

    /// The node a socket's connection is on: its id says.
    fn sock_node(sock: u64) -> u32 {
        (sock >> 52) as u32
    }

    /// Do what a cell asked of its object's sockets: here, or on the node
    /// that holds the connection (fleet mode).
    fn apply_ws(&mut self, name: &str, ws: Vec<WsOut>) -> std::io::Result<()> {
        let me = self.sh.fleet.as_ref().map_or(0, |f| f.cfg.node);
        for out in ws {
            if let WsOut::Message(m) = &out {
                if let Err(e) = self.start_call(self.app.object_proc(name), m, None) {
                    eprintln!("send from {name}: {e}");
                }
                continue;
            }
            let id = out.id();
            if let WsOut::Close(..) = out {
                // Closed by the object: it hears nothing back, and the socket
                // leaves its list with the next request.
                self.ws_gone.entry(name.to_string()).or_default().insert(id);
            }
            let (op, payload) = match out {
                WsOut::Send(_, m) => (1, m.into_bytes()),
                WsOut::Binary(_, b) => (2, b),
                WsOut::Close(_, code) => (8, code.to_be_bytes().to_vec()),
                WsOut::Message(_) => unreachable!("sent above"),
            };
            if Self::sock_node(id) != me {
                if let Some(f) = &self.sh.fleet {
                    let node = Self::sock_node(id);
                    f.wire.push(node, Frame::WsPush { name: name.to_string(), sock: id, op, payload });
                }
                continue;
            }
            self.ws_local(name, id, op, &payload)?;
        }
        Ok(())
    }

    /// Send on a socket whose connection is on this loop. One that isn't here
    /// any more (it closed, or this node restarted) is lost: its object hears.
    fn ws_local(&mut self, name: &str, id: u64, op: u8, payload: &[u8]) -> std::io::Result<()> {
        let live = self.local_socks.get(&id).copied().filter(|&who| self.conn(who).and_then(|c| c.ws.as_ref()).is_some_and(|w| w.open && !w.gone));
        let Some(who) = live else {
            if op != 8 {
                self.socket_lost(name, id);
            }
            return Ok(());
        };
        let frame = crate::ws::encode(op, payload);
        let c = self.conn(who).unwrap();
        let w = c.ws.as_mut().unwrap();
        if c.wbuf.len() - c.wpos + frame.len() > MAX_WS_WRITE {
            // It reads too slowly to keep up: let it go.
            w.gone = true;
            c.close_after = true;
            c.wbuf.truncate(c.wpos);
            self.drop_socket(who, crate::ws::TOO_SLOW);
        } else {
            c.wbuf.extend_from_slice(&frame);
            if op == 8 {
                w.gone = true;
                c.close_after = true;
                self.forget(id, true);
            }
        }
        self.settle(who.0)
    }

    /// A socket in an object's list is gone without a close: tell the
    /// object once (1006), wherever it runs.
    fn socket_lost(&mut self, name: &str, id: u64) {
        if self.ws_gone.get(name).is_some_and(|g| g.contains(&id)) || !self.ws_lost.insert(id) {
            return;
        }
        self.submit(Origin::Socket((usize::MAX, 0), id, false), name.to_string(), format!("do WSCLOSE /{id} 1006"));
    }

    /// This loop no longer holds a socket's connection.
    fn forget(&mut self, id: u64, was_open: bool) {
        if self.local_socks.remove(&id).is_some() && was_open {
            self.sh.stats.sockets_open.fetch_sub(1, Relaxed);
        }
    }

    /// A socket is done from the client's side (it closed, broke, or broke
    /// the rules): forget it, and tell its object, waking it if it sleeps.
    fn drop_socket(&mut self, who: Who, code: u16) {
        let Some(w) = self.conn(who).and_then(|c| c.ws.as_mut()) else { return };
        let (name, id, open, held) = (w.obj.clone(), w.id, w.open, !w.gone);
        w.gone = true;
        if self.local_socks.contains_key(&id) {
            self.forget(id, open);
            if open && held {
                self.submit(Origin::Socket(who, id, false), name, format!("do WSCLOSE /{id} {code}"));
            }
        }
    }

    /// A socket has bytes: hand its messages to its object, answer pings,
    /// and handle a close. Stops while too many messages wait at the object.
    fn ws_read(&mut self, id: usize) -> std::io::Result<()> {
        use crate::ws::Event;
        loop {
            let Some(c) = self.conns.get_mut(id).and_then(Option::as_mut) else { return Ok(()) };
            let who = (id, c.gen);
            let Some(w) = c.ws.as_mut() else { return Ok(()) };
            if !w.open || w.gone || w.inflight >= MAX_WS_INFLIGHT {
                break;
            }
            match w.dec.next(&mut c.rbuf) {
                Ok(None) => break,
                Ok(Some(ev @ (Event::Text(_) | Event::Binary(_)))) => {
                    w.inflight += 1;
                    let (name, sock) = (w.obj.clone(), w.id);
                    self.sh.stats.ws_messages.fetch_add(1, Relaxed);
                    // A binary message rides in the event as base64; the host
                    // turns it back into bytes just before the cell runs.
                    let req = match ev {
                        Event::Text(t) => format!("do WSMSG /{sock} {t}"),
                        Event::Binary(b) => format!("do WSBIN /{sock} {}", crate::ws::base64(&b)),
                        _ => unreachable!(),
                    };
                    self.submit(Origin::Socket(who, sock, false), name, req);
                }
                Ok(Some(Event::Ping(p))) => c.wbuf.extend_from_slice(&crate::ws::pong(&p)),
                Ok(Some(Event::Pong)) => {}
                Ok(Some(Event::Close(code))) => {
                    // Echo the close (RFC 6455), then tell the object.
                    let echo = if code == 1005 || !(1000..5000).contains(&code) { 1000 } else { code };
                    c.wbuf.extend_from_slice(&crate::ws::close(echo));
                    c.close_after = true;
                    self.drop_socket(who, code);
                    break;
                }
                Err(code) => {
                    c.wbuf.extend_from_slice(&crate::ws::close(code));
                    c.close_after = true;
                    self.drop_socket(who, code);
                    break;
                }
            }
        }
        self.settle(id)
    }

    /// A handshake for an object whose requests this loop handles: hold the
    /// connection, and ask the object (`do WSOPEN <path> <id>`; 101 accepts).
    /// In fleet mode the object may run on another node; its events go there
    /// like any request, and the connection stays here.
    fn upgrade(&mut self, id: usize, name: String, path: String, key: String) {
        let Some(c) = self.conns.get_mut(id).and_then(Option::as_mut) else { return };
        let who = (id, c.gen);
        self.next_sock += 1;
        // Unique across the fleet and across restarts: node, boot, loop, count.
        let node = self.sh.fleet.as_ref().map_or(0, |f| f.cfg.node) as u64;
        let sock = (node & 0x7ff) << 52 | (self.boot & 0xffff) << 36 | (self.me as u64 & 0xff) << 28 | (self.next_sock & 0xfff_ffff);
        c.busy = true; // no more HTTP on this connection
        c.ws = Some(WsConn { obj: name.clone(), id: sock, key, dec: Default::default(), open: false, gone: false, inflight: 0 });
        self.local_socks.insert(sock, who);
        self.submit(Origin::Socket(who, sock, true), name, format!("do WSOPEN {path} {sock}"));
    }

    /// A new connection, in a free slot.
    fn add_conn(&mut self, mut s: TcpStream, rbuf: Vec<u8>) -> std::io::Result<usize> {
        let id = self.free.pop().unwrap_or_else(|| {
            self.conns.push(None);
            self.conns.len() - 1
        });
        self.poll.registry().register(&mut s, Token(id), Interest::READABLE)?;
        self.next_gen += 1;
        self.conns[id] = Some(Conn { s, gen: self.next_gen, rbuf, wbuf: vec![], wpos: 0, writing: false, close_after: false, busy: false, ws: None });
        Ok(id)
    }

    /// A request is done: put its cell back (or drop it after a crash), close
    /// its object's transaction, and say what to answer now. `None`: the
    /// answer is held until an fsync covers it.
    ///
    /// A cell that trapped is never used again. Its memory is half an
    /// operation, and the guard-page AOT tier doesn't promise it's exact
    /// at a trap either (LLVM may drop a store around a load that faults).
    fn finish_task(&mut self, t: Task<E>, answer: Result<String, String>) -> Option<((u16, String), Vec<WsOut>)> {
        let Task { mut cell, obj, origin, ws_open, ws_drop, pg, .. } = t;
        // A beat the request asked for, if it succeeded.
        if let (Some(ms), Some(name), true) = (cell.host().tick.take(), &obj, answer.is_ok()) {
            if ms == 0 {
                self.ticks.remove(name);
            } else {
                let every = Duration::from_millis(ms);
                let count = self.ticks.get(name).map_or(0, |t| t.2);
                self.ticks.insert(name.clone(), (every, Instant::now() + every, count));
            }
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
                        if let WsOut::Message(m) = out {
                            if let Err(e) = self.start_call(cell.proc, &m, None) {
                                eprintln!("send: {e}");
                            }
                        }
                    }
                    if !self.app.procs[cell.proc].retired {
                        self.pools[cell.proc].push((cell, Instant::now()));
                    }
                    split_answer(&a)
                }
                Err(trap) => {
                    // Let it crash: this cell is dropped; the pool makes a fresh one when needed.
                    eprintln!("cell trapped: {trap}");
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            }, vec![]));
        };
        if let Origin::Fleet(req) = origin {
            return self.fleet_finish(name, req, cell, answer, ws, ws_open, ws_drop).map(|r| (r, vec![]));
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
                    eprintln!("object {name:?} trapped: {trap}");
                    db.rollback();
                    (500, format!("{{\"error\":{trap:?}}}"))
                }
            }
        };
        if ok {
            self.sockets_committed(&name, joined, &ws_drop);
        }
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
        let mut out = Some((reply, ws));
        if let Some(o) = self.objects.get_mut(&name) {
            if wrote {
                o.committed += 1;
                self.sh.stats.commits.fetch_add(1, Relaxed);
            }
            if ok && o.committed > o.synced {
                let (reply, ws) = out.take().unwrap();
                o.held.push((o.committed, origin, reply, ws));
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
                let job = SyncJob { file, db_path, ckpt, obj: name.to_string(), gen, back: self.me, dirs: std::mem::take(&mut o.new_dirs) };
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
            self.sh.stats.objects_live.fetch_sub(1, Relaxed);
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
                self.submit(Origin::Tick, name, format!("do TICK / {n}"));
            }
        }
    }

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
        self.retire_cells();
        let idle = self.sh.cfg.idle;
        for pool in &mut self.pools {
            trim_idle(pool, idle);
        }
        // Database handles keep an object here until they've been idle for
        // their pool's `session_idle_ms`; then they close, and it can leave.
        let handles = &self.pg_handles;
        let idle_for = |o: &Object<E>| o.pg.iter().filter_map(|h| handles.get(h)).map(|ph| ph.lease.session_idle).min();
        let stale: Vec<u64> = self.objects.values().filter(|o| !o.busy && idle_for(o).is_some_and(|d| o.pg_used.elapsed() >= d)).flat_map(|o| o.pg.clone()).collect();
        for h in stale {
            self.pg_close(h);
        }
        let free = |o: &Object<E>| !o.busy && o.waiting.is_empty() && !o.syncing && o.held.is_empty() && !o.checkpointing && o.pg.is_empty();
        // A ticking object stays: its beat is its life.
        let ticks = &self.ticks;
        let mut gone: Vec<String> = self.objects.iter().filter(|(n, o)| free(o) && !ticks.contains_key(*n) && o.last_used.elapsed() >= idle).map(|(n, _)| n.clone()).collect();
        let over = self.objects.len().saturating_sub(gone.len()).saturating_sub(self.sh.cfg.max_objects);
        if over > 0 {
            let mut lru: Vec<(&String, Instant)> =
                self.objects.iter().filter(|(n, o)| free(o) && !gone.contains(n)).map(|(n, o)| (n, o.last_used)).collect();
            lru.sort_by_key(|(_, t)| *t);
            gone.extend(lru.into_iter().take(over).map(|(n, _)| n.clone()));
        }
        for name in gone {
            self.remove_object(&name);
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
            let (head, body) = reply_parts(&payload);
            let _ = write!(c.wbuf, "HTTP/1.1 {status} OK\r\n");
            if !head.iter().any(|h| h.starts_with("content-type:")) {
                let _ = write!(c.wbuf, "content-type: application/json\r\n");
            }
            for h in &head {
                let _ = write!(c.wbuf, "{h}\r\n");
            }
            let _ = write!(c.wbuf, "content-length: {}\r\n\r\n", body.len());
            c.wbuf.extend_from_slice(body.as_bytes());
            c.busy = false;
        }
    }

    /// Flush what's buffered, then close or update write interest.
    fn settle(&mut self, id: usize) -> std::io::Result<()> {
        let Some(c) = self.conns.get_mut(id).and_then(Option::as_mut) else { return Ok(()) };
        // A WebSocket is always "busy"; it closes once its last bytes are out.
        let closed = flush(c) || (c.close_after && (!c.busy || c.ws.is_some()) && c.wpos == c.wbuf.len());
        if closed {
            if c.ws.as_ref().is_some_and(|w| !w.gone) {
                // Gone without a close frame (RFC 6455's 1006).
                let who = (id, c.gen);
                self.drop_socket(who, 1006);
            }
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
        if c.ws.is_some() {
            return self.ws_read(id);
        }
        if c.busy {
            return Ok(());
        }
        let who = (id, c.gen);
        let req = match parse(c, &self.app.router) {
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
            Req::Upgrade(name, path, key) => {
                let o = owner(&name, self.sh.cfg.threads);
                if o == self.me {
                    self.upgrade(id, name, path, key);
                } else {
                    // The object's loop keeps all its sockets: hand this one over.
                    let mut c = self.conns[id].take().unwrap();
                    self.poll.registry().deregister(&mut c.s)?;
                    self.free.push(id);
                    let (tx, waker) = &self.sh.peers[o];
                    self.sh.stats.ws_handoffs.fetch_add(1, Relaxed);
                    let _ = tx.send(Msg::Adopt { s: c.s, rbuf: c.rbuf, name, path, key });
                    waker.wake()?;
                }
            }
            Req::Stats => {
                let s = &self.sh.stats;
                let body = format!(
                    "{{\"objects_live\":{},\"objects_opened\":{},\"evictions\":{},\"alarms_fired\":{},\"alarms_failed\":{},\"forwarded\":{},\"commits\":{},\"fsyncs\":{},\"checkpoints\":{},\"fsyncs_slow\":{},\"fsync_max_us\":{},\"peer_acks\":{},\"bucket_acks\":{},\"fences\":{},\"tail_entries\":{},\"uploads\":{},\"lease_reads\":{},\"fence_waits\":{},\"gc_deleted\":{},\"sockets_open\":{},\"ws_messages\":{},\"ws_handoffs\":{},\"ev_instances\":{},\"ev_requests\":{}}}",
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
                    s.tail_entries.load(Relaxed),
                    s.uploads.load(Relaxed),
                    s.lease_reads.load(Relaxed),
                    s.fence_waits.load(Relaxed),
                    s.gc_deleted.load(Relaxed),
                    s.sockets_open.load(Relaxed),
                    s.ws_messages.load(Relaxed),
                    s.ws_handoffs.load(Relaxed),
                    s.ev_instances.load(Relaxed),
                    s.ev_requests.load(Relaxed)
                );
                self.queue_reply(who, (200, body));
            }
            Req::NewId => self.queue_reply(who, (200, format!("{{\"id\":\"{}\"}}", uuid_v7()))),
            Req::NotFound => self.queue_reply(who, (404, "{\"error\":\"no route\"}".into())),
            Req::Ping => self.queue_reply(who, (200, "\0access-control-allow-origin: *\ncache-control: no-store\0{}".into())),
            Req::Nodes => {
                let me = self.sh.fleet.as_ref().map_or(0, |f| f.cfg.node);
                let nodes: serde_json::Map<String, serde_json::Value> = self.sh.cfg.public.iter().map(|(n, u)| (n.to_string(), serde_json::Value::String(u.clone()))).collect();
                let body = serde_json::json!({ "me": me, "nodes": nodes, "version": option_env!("COMBD_VERSION").unwrap_or("dev") }).to_string();
                self.queue_reply(who, (200, format!("\0access-control-allow-origin: *\0{body}")));
            }
            Req::Cell(p, cell_req) => self.run_cell(Origin::Local(who), p, cell_req)?,
            Req::Object(name, cell_req) => self.run_obj(Origin::Local(who), name, cell_req)?,
        }
        Ok(())
    }

    /// A request for a worker process, on this loop.
    fn run_cell(&mut self, origin: Origin, p: usize, req: String) -> std::io::Result<()> {
        if self.ev_mode[p] {
            return self.ev_request(origin, p, req);
        }
        let mut cell = match self.pools[p].pop().map(|(c, _)| c) {
            Some(cell) => cell,
            None => fresh(self.eng, &self.app, p, &self.environ_for(p))?,
        };
        if let Err(trap) = crate::start(&mut cell.vm, &req, &mut cell.st) {
            eprintln!("cell trapped starting a request: {trap}");
            return self.deliver(origin, (500, String::new()));
        }
        self.tasks.push_back(Task { origin, cell, obj: None, ws_open: None, ws_drop: vec![], pg: vec![], ev: None });
        Ok(())
    }

    /// A request for an object, on the loop that owns it.
    fn run_obj(&mut self, origin: Origin, name: String, req: String) -> std::io::Result<()> {
        let o = owner(&name, self.sh.cfg.threads);
        if o == self.me {
            self.submit(origin, name, req);
            return Ok(());
        }
        // Another loop owns it: hand the request over.
        self.sh.stats.forwarded.fetch_add(1, Relaxed);
        let origin = match origin {
            Origin::Local(who) => Origin::Remote(self.me, who),
            o => o,
        };
        let (tx, waker) = &self.sh.peers[o];
        let _ = tx.send(Msg::Run { origin, obj: name, req });
        waker.wake()
    }

    /// `call` (with someone to tell) or `send` (`to: None`) from process
    /// `from`. The manifest's `callers` decide; `Err` says why not. A call
    /// always ends in exactly one answer: the target's, or a timeout.
    fn start_call(&mut self, from: usize, msg: &[u8], to: Option<CallTo>) -> Result<(), String> {
        let v: serde_json::Value = serde_json::from_slice(msg).map_err(|_| "want JSON: {\"to\", \"method\", \"path\", \"body\"}".to_string())?;
        let target = v["to"].as_str().ok_or("name the process: \"to\"")?;
        let caller = &self.app.procs[from].name;
        let tenant = self.app.procs[from].tenant;
        // By name: a caller still on an older version (a deploy just came)
        // is the same process as far as `callers` goes.
        let allowed = |t: &usize| !self.app.router.legacy && self.app.procs[*t].callers.iter().any(|&c| self.app.procs[c].name == *caller);
        let t = self.app.proc_in(tenant, target).filter(allowed).ok_or_else(|| format!("denied: {caller} may not call {target}"))?;
        let proc = &self.app.procs[t];
        let method = v["method"].as_str().unwrap_or("POST");
        if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"].contains(&method) {
            return Err(format!("method {method:?}"));
        }
        let path = v["path"].as_str().unwrap_or("/");
        if !path.starts_with('/') || path.contains(char::is_whitespace) {
            return Err("path: want /..., with no spaces".into());
        }
        let body = v["body"].as_str().unwrap_or("");
        let line = |kind: &str| if body.is_empty() { format!("{kind} {method} {path}") } else { format!("{kind} {method} {path} {body}") };
        let timeout = v["timeout_ms"].as_u64().map_or(proc.timeout, |ms| Duration::from_millis(ms).min(proc.timeout));
        let origin = match to {
            Some(to) => {
                self.next_call += 1;
                self.calls.insert(self.next_call, (to, Instant::now() + timeout));
                Origin::Call(self.me, self.next_call)
            }
            None => Origin::Discard,
        };
        let r = if proc.durable {
            let id = v["id"].as_str().filter(|id| !id.is_empty() && !id.contains('/')).ok_or(format!("{target} is durable: name the object, \"id\""))?;
            let name = self.app.object_name(t, id);
            if name.len() > MAX_NAME {
                return Err("id: too long".into());
            }
            // `"node"`: start (or reach) the object on that node, as a
            // matchmaker does to put a match near both players.
            match (v["node"].as_u64(), &self.sh.fleet) {
                (Some(n), Some(f)) if n as u32 != f.cfg.node && f.cfg.peers.contains_key(&(n as u32)) => {
                    self.forward(n as u32, FReq { origin, name, cell_req: line("do"), hops: 0, act: 0, resp: None, ws: vec![], retries: 0 });
                    Ok(())
                }
                _ => self.run_obj(origin, name, line("do")),
            }
        } else {
            self.run_cell(origin, t, line("api"))
        };
        r.map_err(|e: std::io::Error| e.to_string())
    }

    /// A deploy swapped in a new app: take it. Processes it retired keep
    /// what's running and get nothing new; their warm cells go, and an
    /// object's old cell gives way to a fresh one (its storage stays).
    fn pick_up_app(&mut self) -> std::io::Result<()> {
        let gen = self.live.gen.load(Relaxed);
        if gen == self.app_gen {
            return Ok(());
        }
        self.app_gen = gen;
        let from = self.app.procs.len();
        self.app = self.live.now();
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
        // The apps' pools: started here if they're new (idempotent).
        if let Some(pools) = &self.sh.pg {
            pools.ensure(&self.app.pools());
        }
        self.pools.resize_with(self.app.procs.len(), Vec::new);
        self.ev_mode.resize(self.app.procs.len(), false);
        for p in from..self.app.procs.len() {
            if self.app.procs[p].retired {
                continue;
            }
            let mut first = fresh(self.eng, &self.app, p, &self.environ_for(p))?;
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
            return self.sh.cfg.environ.clone();
        }
        let p = &self.app.procs[proc];
        let t = &self.app.tenants[p.tenant];
        let node = self.sh.fleet.as_ref().map_or(0, |f| f.cfg.node);
        let mut e = (*t.environ).clone();
        e.extend([format!("COMBD_ORG={}", t.org), format!("COMBD_APP={}", t.app), format!("COMBD_PROCESS={}", p.name), format!("COMBD_NODE={node}")].map(String::into_bytes));
        Arc::new(e)
    }

    /// May process `proc` lease from pool `pool`? Its `callers` decide (with
    /// no manifest, anyone may).
    fn may_lease(&self, proc: usize, pool: &str) -> Result<(), String> {
        if self.app.router.legacy || self.app.procs[proc].owns.iter().any(|p| p == pool) {
            return Ok(());
        }
        Err(format!("denied: {} isn't one of pool {pool}'s callers", self.app.procs[proc].name))
    }

    /// A `call` is answered: tell whoever waits (once; a late answer is dropped).
    fn call_done(&mut self, id: u64, (status, body): (u16, String)) {
        let Some((to, _)) = self.calls.remove(&id) else { return };
        let (_, body) = reply_parts(&body);
        self.call_reply(to, serde_json::json!({ "status": status, "body": body }).to_string().into_bytes());
    }

    fn call_reply(&mut self, to: CallTo, payload: Vec<u8>) {
        match to {
            CallTo::Ev(inst, gen, ticket) => self.ev_done(inst, gen, ticket, DbDone::Answer(payload)),
            CallTo::Park(park) => self.db_done(park, DbDone::Answer(payload)),
        }
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
            let path = store::path_for(&self.sh.cfg.data, &name);
            let dirs = if path.exists() { Ok(vec![]) } else { store::make_dirs(&self.sh.cfg.data, &path) };
            let db = match dirs.and_then(|d| Db::open(&path).map(|db| (db, d)).map_err(std::io::Error::other)) {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("object {name:?}: can't open storage: {e}");
                    let _ = self.deliver(origin, (500, "{\"error\":\"storage\"}".into()));
                    return;
                }
            };
            self.sh.stats.objects_live.fetch_add(1, Relaxed);
            self.sh.stats.objects_opened.fetch_add(1, Relaxed);
            let mut o = Object::new(db.0);
            o.new_dirs = db.1;
            self.objects.insert(name.clone(), o);
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
    fn run_object(&mut self, name: &str, origin: Origin, mut req: String) -> Result<bool, (u16, String)> {
        let (eng, app) = (self.eng, self.app.clone());
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
            _ => match fresh(eng, &app, proc, &environ) {
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
        cell.host().ws_in = ws_in;
        if let Err(trap) = crate::start(&mut cell.vm, &req, &mut cell.st) {
            eprintln!("object {name:?} trapped starting a request: {trap}");
            o.db.lock().unwrap().rollback();
            return Err((500, String::new()));
        }
        o.busy = true;
        self.tasks.push_back(Task { origin, cell, obj: Some(name.to_string()), ws_open, ws_drop, pg: vec![], ev: None });
        Ok(true)
    }
}

impl<E: Engine> Loop<'_, E> {
    fn core_now(&self) -> u64 {
        self.sh.fleet.as_ref().map_or(0, |f| f.start.elapsed().as_millis() as u64)
    }

    /// Hand the core an event, then do what it says.
    /// "This node is alive", to every peer. From loop 0's tick, so a node
    /// whose loops stop or stall goes quiet, and gets taken over.
    fn heartbeat(&self) {
        let f = self.sh.fleet.as_ref().unwrap();
        for &p in f.cfg.peers.keys().filter(|&&p| p != f.cfg.node) {
            f.wire.push(p, Frame::Heartbeat { node: f.cfg.node, boot: f.boot });
        }
    }

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
        st.uploads.fetch_add(now.uploads - was.uploads, Relaxed);
        st.lease_reads.fetch_add(now.lease_reads - was.lease_reads, Relaxed);
        st.fence_waits.fetch_add(now.fence_waits - was.fence_waits, Relaxed);
        st.gc_deleted.fetch_add(now.gc_deleted - was.gc_deleted, Relaxed);
        for e in effects {
            self.effect(e);
        }
    }

    fn fleet_request(&mut self, origin: Origin, name: String, cell_req: String, hops: u8) {
        self.next_req += 1;
        let req = self.next_req;
        let obj = own::obj_id(&name);
        self.names.insert(obj, name.clone());
        self.freqs.insert(req, FReq { origin, name, cell_req, hops, act: 0, resp: None, ws: vec![], retries: 0 });
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
                if matches!(ans, own::Answer::Done { .. }) && !fr.ws.is_empty() {
                    let _ = self.apply_ws(&fr.name, fr.ws.clone());
                }
                let reply = match ans {
                    own::Answer::Done { .. } => fr.resp.unwrap_or((500, "{\"error\":\"no answer\"}".into())),
                    own::Answer::Failed => (503, "{\"error\":\"retry\"}".into()),
                    // It never ran: route it again (the core now knows who
                    // won the object), so nobody sees the race.
                    own::Answer::NotRun if fr.retries < MAX_NOT_RUN => {
                        self.next_req += 1;
                        let req = self.next_req;
                        let obj = own::obj_id(&fr.name);
                        self.names.insert(obj, fr.name.clone());
                        self.freqs.insert(req, FReq { retries: fr.retries + 1, act: 0, resp: None, ws: vec![], ..fr });
                        return self.core_on(own::Event::Request { obj, req });
                    }
                    own::Answer::NotRun => (503, "{\"error\":\"retry\"}".into()),
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
            Effect::Seal { obj, act, image } => {
                let Some(name) = self.names.get(&obj).cloned() else { return };
                let Some(o) = self.objects.get_mut(&name) else { return };
                let changes = {
                    let db = o.db.lock().unwrap();
                    let c = db.seal();
                    // An image as of this seal, if asked and nothing's running
                    // (else none this time; the core asks again later).
                    // ponytail: read on the loop, the whole file; a big object
                    // stalls its loop that long every compact_segs uploads.
                    o.image = match (&c, image) {
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
                        self.remove_object(&name);
                    }
                }
            }
            Effect::PutImage { obj, act } => {
                let Some(name) = self.names.get(&obj).cloned() else { return };
                if let Some((key, entry)) = self.objects.get_mut(&name).and_then(|o| o.image.take()) {
                    self.job(Job::Image { obj, act, key, entry, back: self.me });
                }
            }
            Effect::Restore { obj, act, segs, image } => self.job(Job::Fetch { obj, act, segs, image, back: self.me }),
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
                if let Some(name) = self.names.get(&obj).cloned() {
                    if self.remove_object(&name).is_some() {
                        self.sh.stats.objects_live.fetch_sub(1, Relaxed);
                    }
                }
            }
        }
    }

    /// An object's segments arrived: rebuild its database, then serve.
    fn fetched(&mut self, obj: own::Obj, act: Act, data: Result<Vec<Vec<u8>>, String>, index: Vec<i64>, image: Option<String>) {
        let Some(name) = self.names.get(&obj).cloned() else { return };
        let path = store::path_for(&self.sh.cfg.data, &name);
        // Its folder; durability here comes from the peers and the bucket.
        let _ = store::make_dirs(&self.sh.cfg.data, &path);
        if self.remove_object(&name).is_some() {
            self.sh.stats.objects_live.fetch_sub(1, Relaxed);
        }
        let restored = data.and_then(|segs| store::restore(&path, &segs).map_err(|e| e.to_string()));
        // The image: the file as restored, before anything runs on it.
        if let (Ok(()), Some(key)) = (&restored, image) {
            match store::image(&path) {
                Ok(Some(entry)) => self.job(Job::Image { obj, act, key, entry, back: self.me }),
                Ok(None) => {}
                Err(e) => eprintln!("object {name:?}: no image: {e}"),
            }
        }
        let db = restored.and_then(|()| Db::open(&path).map_err(|e| e.to_string()));
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
    #[allow(clippy::too_many_arguments)]
    fn fleet_finish(&mut self, name: String, req: own::ReqId, cell: Cell<E>, answer: Result<String, String>, ws: Vec<WsOut>, ws_open: Option<u64>, ws_drop: Vec<u64>) -> Option<(u16, String)> {
        let Some(o) = self.objects.get_mut(&name) else { return None };
        o.busy = false;
        o.last_used = Instant::now();
        let mut committed = None;
        let (reply, changed) = {
            let db = o.db.lock().unwrap();
            match answer {
                Ok(a) if ws_accepted(&a, ws_open).is_some_and(|id| db.update_sockets(Some(id), &[]).is_err()) => {
                    db.rollback();
                    ((500, "{\"error\":\"storage\"}".into()), false)
                }
                Ok(a) => match db.commit() {
                    Ok(w) => {
                        o.cell = Some(cell);
                        committed = Some(ws_accepted(&a, ws_open));
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
        if let Some(joined) = committed {
            self.sockets_committed(&name, joined, &ws_drop);
        }
        // The sends wait with the answer, for the core to say it's proven.
        let ok = committed.is_some();
        let act = self.freqs.get_mut(&req).map(|fr| {
            fr.resp = Some(reply);
            if ok {
                fr.ws = ws;
            }
            // An object's own beat: its answer goes nowhere (see `Ran`).
            (fr.act, !matches!(fr.origin, Origin::Tick))
        });
        self.reconcile(&name);
        self.next_waiting(&name);
        if let Some((act, proof)) = act {
            self.core_on(own::Event::Ran { obj: own::obj_id(&name), act, req, changed, proof });
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
            self.core_on(own::Event::Ran { obj: own::obj_id(name), act, req, changed: false, proof: true });
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
        self.retire_cells();
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
        self.wire_out.insert(id, (fr.origin, node, Instant::now(), limit, fr.name.clone()));
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
        if let Some(p) = &self.sh.pg {
            p.rtt.lock().unwrap().insert(node, (e.0 * 1000.0) as u64);
        }
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
    catalog: Option<Arc<crate::catnode::CatNode>>,
    /// When each node was last heard from, and from which process.
    heard: Arc<Mutex<HashMap<own::Node, (u64, Instant)>>>,
    /// This node's wire, once it's up (to answer "who has node N?").
    wire: Arc<std::sync::OnceLock<Wire>>,
    /// This node's pools, once they're up.
    pools: Arc<std::sync::OnceLock<Arc<crate::pg::Pools>>>,
}

impl wire::Sink for LoopSink {
    fn forward(&self, conn: usize, id: u64, name: String, req: String) {
        // A node starting up asks when we last heard its id (`node_id_free`).
        if name == "_combd/node" {
            let n: own::Node = req.trim().parse().unwrap_or(u32::MAX);
            let body = match self.heard.lock().unwrap().get(&n) {
                Some((boot, at)) => serde_json::json!({ "boot": boot, "ms": at.elapsed().as_millis() as u64 }).to_string(),
                None => "{}".into(),
            };
            if let Some(w) = self.wire.get() {
                w.reply(conn, Frame::Reply { id, status: 200, body });
            }
            return;
        }
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
    fn ws_push(&self, name: String, sock: u64, op: u8, payload: Vec<u8>) {
        let (tx, waker) = &self.loops[owner(&name, self.threads)];
        let _ = tx.send(Msg::WsPush { name, sock, op, payload });
        let _ = waker.wake();
    }
    fn ws_lost(&self, name: String, sock: u64) {
        let (tx, waker) = &self.loops[owner(&name, self.threads)];
        let _ = tx.send(Msg::WsLost { name, sock });
        let _ = waker.wake();
    }
    fn catalog(&self, seq: u64, entry: Vec<u8>) {
        if let Some(c) = &self.catalog {
            c.push(seq, &entry);
        }
    }
    fn pool(&self, body: Vec<u8>) {
        if let Some(p) = self.pools.get() {
            p.deliver(&body);
        }
    }
    fn heartbeat(&self, node: own::Node, boot: u64) {
        self.heard.lock().unwrap().insert(node, (boot, Instant::now()));
        for (tx, waker) in &self.loops {
            let _ = tx.send(Msg::Heard { node });
            let _ = waker.wake();
        }
    }
}

fn json_err(m: &str) -> Vec<u8> {
    serde_json::json!({ "error": m }).to_string().into_bytes()
}

/// The socket a handshake's answer accepts, if it's a 101.
fn ws_accepted(answer: &str, ws_open: Option<u64>) -> Option<u64> {
    ws_open.filter(|_| split_answer(answer).0 == 101)
}

/// A cell's answer: `"<code> <body>"`, or with headers,
/// `"<code>\n<name>: <value>\n...\n\n<body>"`. Headers ride along in the
/// body as `\0<lines>\0<body>` (so they cross loops and the wire unchanged),
/// and only safe ones get through: never one that frames the message
/// (`content-length`, `connection`...), so a cell can't split a response.
fn split_answer(a: &str) -> (u16, String) {
    let code = a.get(..3).and_then(|c| c.parse::<u16>().ok()).unwrap_or(500);
    match a.as_bytes().get(3) {
        Some(b'\n') => {
            let rest = &a[4..];
            let (head, body) = rest.split_once("\n\n").unwrap_or((rest, ""));
            let ok: Vec<String> = head
                .lines()
                .filter_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    let (n, v) = (n.trim().to_ascii_lowercase(), v.trim());
                    let allowed = matches!(n.as_str(), "content-type" | "cache-control" | "location" | "set-cookie" | "etag" | "last-modified" | "vary") || n.starts_with("access-control-");
                    (allowed && !v.contains(['\r', '\n', '\0'])).then(|| format!("{n}: {v}"))
                })
                .collect();
            if ok.is_empty() {
                (code, body.to_string())
            } else {
                (code, format!("\0{}\0{body}", ok.join("\n")))
            }
        }
        _ => (code, a.get(4..).unwrap_or("").to_string()),
    }
}

/// An answer's headers (the safe ones `split_answer` kept) and its body.
fn reply_parts(payload: &str) -> (Vec<&str>, &str) {
    match payload.strip_prefix('\0').and_then(|r| r.split_once('\0')) {
        Some((head, body)) => (head.lines().collect(), body),
        None => (vec![], payload),
    }
}

/// Take one whole request off the front of the read buffer.
fn parse(c: &mut Conn, router: &Router) -> Result<Option<Req>, ()> {
    parse_buf(&mut c.rbuf, &mut c.close_after, router)
}

/// `parse` on just the bytes read so far (and the connection's close flag),
/// so it can be tested and fuzzed without a socket.
fn parse_buf(rbuf: &mut Vec<u8>, close_after: &mut bool, router: &Router) -> Result<Option<Req>, ()> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let hlen = match req.parse(rbuf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut clen: Option<usize> = None;
    let mut close = false;
    let (mut upgrade, mut ws_key, mut ws_ok) = (false, None, false);
    let mut host = "";
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("host") {
            host = std::str::from_utf8(h.value).map_err(|_| ())?;
        }
        if h.name.eq_ignore_ascii_case("upgrade") && h.value.eq_ignore_ascii_case(b"websocket") {
            upgrade = true;
        } else if h.name.eq_ignore_ascii_case("sec-websocket-key") {
            ws_key = std::str::from_utf8(h.value).ok().map(|k| k.trim().to_string()).filter(|k| k.len() == 24);
        } else if h.name.eq_ignore_ascii_case("sec-websocket-version") {
            ws_ok = h.value == b"13";
        }
        if h.name.eq_ignore_ascii_case("content-length") {
            // A bad length, or two that disagree, is refused (RFC 9112): read
            // as 0, the body would be taken for the next request.
            let v: usize = std::str::from_utf8(h.value).ok().and_then(|v| v.trim().parse().ok()).ok_or(())?;
            if clen.is_some_and(|c| c != v) {
                return Err(());
            }
            clen = Some(v);
        } else if h.name.eq_ignore_ascii_case("connection") && h.value.eq_ignore_ascii_case(b"close") {
            close = true;
        }
    }
    // Checked: a huge length used to wrap `hlen + clen` past usize::MAX, and
    // slicing the body then panicked the event loop (any client could).
    let clen = clen.unwrap_or(0);
    if clen > MAX_REQUEST - hlen.min(MAX_REQUEST) {
        return Err(());
    }
    if rbuf.len() < hlen + clen {
        // The body hasn't all arrived yet. Don't act on `connection: close`
        // until it has, or the idle connection gets closed under the request.
        return Ok(None);
    }
    *close_after |= close;
    let body = String::from_utf8_lossy(&rbuf[hlen..hlen + clen]);
    let (method, path) = (req.method.unwrap_or("GET"), req.path.unwrap_or("/"));
    // Only standard methods: `ALARM` is how the host runs an object's alarm,
    // and a client must never be able to fire one.
    if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"].contains(&method) {
        return Err(());
    }
    let line = |kind: &str, path: &str| if body.is_empty() { format!("{kind} {method} {path}") } else { format!("{kind} {method} {path} {body}") };
    // A manifest's routes: by host, then path (the query string isn't part of it).
    if path == "/_combd/nodes" || path == "/_combd/ping" {
        let r = if path == "/_combd/ping" { Req::Ping } else { Req::Nodes };
        rbuf.drain(..hlen + clen);
        return Ok(Some(r));
    }
    if !router.legacy && path != "/_combd/stats" {
        let r = match router.route(host, path.split('?').next().unwrap_or("")) {
            Some(Hit::Object(name, sub)) => {
                if name.len() > MAX_NAME {
                    return Err(());
                }
                match (upgrade, ws_key) {
                    (true, Some(key)) if method == "GET" && ws_ok && clen == 0 && !sub.contains(char::is_whitespace) => {
                        rbuf.drain(..hlen);
                        return Ok(Some(Req::Upgrade(name, sub, key)));
                    }
                    (true, _) => return Err(()),
                    _ => Req::Object(name, line("do", &sub)),
                }
            }
            Some(Hit::Cell(_)) if upgrade => return Err(()),
            Some(Hit::Cell(p)) => Req::Cell(p, line("api", path)),
            None => Req::NotFound,
        };
        rbuf.drain(..hlen + clen);
        return Ok(Some(r));
    }
    // The object and the path inside it, for `/do/<name>/...` and `/do/id/<uuid>/...`.
    let object = |path: &str| -> Result<Option<(String, String)>, ()> {
        if let Some(rest) = path.strip_prefix("/do/id/") {
            // `/do/id/<uuid>/<rest>`: an object made by `/do/new`. Its name gets an
            // `id:` prefix, so it can never meet an object named the same way.
            let (id, sub) = rest.split_once('/').map_or((rest, "/".to_string()), |(n, s)| (n, format!("/{s}")));
            return Ok(Some((format!("id:{}", parse_uuid(id).ok_or(())?), sub)));
        }
        if let Some(rest) = path.strip_prefix("/do/") {
            // `/do/<name>/<rest>`: a Durable Object request.
            let (name, sub) = rest.split_once('/').map_or((rest, "/".to_string()), |(n, s)| (n, format!("/{s}")));
            if name.is_empty() || name.len() > MAX_NAME {
                return Err(());
            }
            return Ok(Some((name.to_string(), sub)));
        }
        Ok(None)
    };
    if upgrade {
        // A WebSocket handshake: only for objects, only version 13, no body.
        let (Some((name, sub)), Some(key)) = (object(path)?, ws_key) else { return Err(()) };
        if method != "GET" || !ws_ok || clen != 0 || sub.contains(char::is_whitespace) {
            return Err(());
        }
        rbuf.drain(..hlen);
        return Ok(Some(Req::Upgrade(name, sub, key)));
    }
    let r = if path == "/_combd/stats" {
        Req::Stats
    } else if path == "/do/new" {
        Req::NewId
    } else if let Some((name, sub)) = object(path)? {
        Req::Object(name, line("do", &sub))
    } else {
        Req::Cell(0, line("api", path))
    };
    rbuf.drain(..hlen + clen);
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
                if c.rbuf.len() > if c.ws.is_some() { MAX_WS_READ } else { MAX_REQUEST } {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `buf` to the parser as the event loop does: requests until it
    /// needs more bytes or refuses. Err(()) if it panicked.
    fn drain(buf: &[u8]) -> Result<usize, ()> {
        let mut rbuf = buf.to_vec();
        let mut close = false;
        // Both ways: the old fixed routes, and a manifest's.
        let routed = Router::from(&[("shop.test/api/*", "api", false), ("/room/:id/*", "room", true), ("/a/:x/b", "x", false), ("/", "root", false)]);
        let mut rbuf2 = rbuf.clone();
        let mut close2 = false;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut n = 0;
            while let Ok(Some(_)) = parse_buf(&mut rbuf, &mut close, &Router::legacy()) {
                n += 1;
            }
            while let Ok(Some(_)) = parse_buf(&mut rbuf2, &mut close2, &routed) {
                n += 1;
            }
            n
        }))
        .map_err(|_| ())
    }

    /// Any client could send this: `hlen + clen` wrapped past usize::MAX,
    /// and slicing the body panicked the event loop.
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

    #[test]
    fn a_huge_content_length_is_refused() {
        for v in ["18446744073709551615", "18446744073709551000", "99999999999999999999999", "-1"] {
            let req = format!("POST /do/x/incr HTTP/1.1\r\ncontent-length: {v}\r\n\r\nhi");
            assert!(drain(req.as_bytes()).is_ok(), "content-length {v}");
        }
    }

    /// A manifest's routes: specific beats general, and ids are path segments.
    #[test]
    fn routes_pick_the_most_specific() {
        let r = Router::from(&[("/*", "any", false), ("/room/:id/*", "room", true), ("shop.test/*", "shop", false), ("/room/new", "new", false)]);
        let hit = |host: &str, path: &str| match r.route(host, path) {
            Some(Hit::Cell(p)) => format!("cell {p}"),
            Some(Hit::Object(n, sub)) => format!("{n} {sub}"),
            None => "none".into(),
        };
        assert_eq!(hit("x", "/room/r1/count"), "room/r1 /count");
        assert_eq!(hit("x", "/room/r1"), "room/r1 /");
        assert_eq!(hit("x", "/room/new"), "cell 3", "a literal beats a capture");
        assert_eq!(hit("shop.test", "/room/r1"), "cell 2", "a host beats any host");
        assert_eq!(hit("x", "/room//x"), "cell 0", "an empty id isn't one");
        assert_eq!(hit("x", "/"), "cell 0");
    }

    /// Valid requests, damaged at random: the parser must never panic.
    #[test]
    fn mangled_requests_never_panic_the_parser() {
        let base: &[&[u8]] = &[
            b"GET /api/users/42 HTTP/1.1\r\nhost: x\r\n\r\n",
            b"POST /do/room/msg HTTP/1.1\r\ncontent-length: 5\r\n\r\nhello",
            b"GET /do/id/0190b5a2-7c3e-7000-8000-000000000000/count HTTP/1.1\r\n\r\n",
            b"POST /do/new HTTP/1.1\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
            b"GET /_combd/stats HTTP/1.1\r\n\r\nGET / HTTP/1.1\r\n\r\n",
            b"GET /api/x?y=1 HTTP/1.1\r\nhost: SHOP.test:443\r\n\r\n",
            b"GET /room/r1/ws HTTP/1.1\r\nhost: x\r\nupgrade: websocket\r\nsec-websocket-version: 13\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            b"POST /room//x HTTP/1.1\r\nhost: x\r\ncontent-length: 2\r\n\r\nhi",
            b"GET /a/q/b HTTP/1.1\r\nhost: x\r\n\r\n",
        ];
        let lens: &[&[u8]] = &[b"18446744073709551615", b"0", b"1", b"4294967296", b"-5", b" 3 ", b"3,3", b"999999999999"];
        let mut rng = crate::sim::Rng::new(1);
        for i in 0..200_000u64 {
            let mut b = base[rng.below(base.len() as u64) as usize].to_vec();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(b.len() as u64 + 1) as usize;
                match rng.below(7) {
                    0 if !b.is_empty() => {
                        let k = at.min(b.len() - 1);
                        b[k] ^= 1 << rng.below(8);
                    }
                    1 => b.insert(at, rng.next() as u8),
                    2 => b.truncate(at),
                    3 => b.splice(at..at, b"\r\n".iter().copied()).for_each(drop),
                    4 => {
                        let mut h = b"content-length: ".to_vec();
                        h.extend_from_slice(lens[rng.below(lens.len() as u64) as usize]);
                        h.extend_from_slice(b"\r\n");
                        let p = b.windows(2).position(|w| w == b"\r\n").map_or(0, |p| p + 2);
                        b.splice(p..p, h).for_each(drop);
                    }
                    5 => b.extend((0..rng.below(64)).map(|_| rng.next() as u8)),
                    _ => b.extend_from_slice(&b.clone()),
                }
            }
            assert!(drain(&b).is_ok(), "case {i}: {:?}", String::from_utf8_lossy(&b));
        }
    }
}
