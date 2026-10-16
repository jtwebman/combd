//! Bring your own database: Postgres, through a pool the host runs.
//!
//! Each pool in the manifest is one for the whole fleet: one **manager**
//! keeps the books (`pool.rs` decides), so the database never sees more than
//! `max` connections however many nodes there are. A **member** is one
//! connection on its own thread, started on the node of the caller who
//! asked. A cell never holds a socket or a password; it asks the pool for a
//! **lease** on a member.
//! - Every node runs an agent per pool: it sends asks to the manager, runs
//!   its members, and serves other nodes' calls on them. At max, a free
//!   member on the nearest node is leased, and its calls go over the wire.
//! - The catalog names the manager (`<org>/<app>#pool/<pool>`: node and
//!   epoch). A claim is "epoch e + 1, if it's still e", so one node wins. A
//!   node claims when there's none, or the manager's node is quiet for a
//!   lease. Members of an older epoch finish their lease and stop
//!   (`on_manager_loss = "finish"`) or stop now (`"stop"`).
//! - Once a second each node tells the manager what its members are doing,
//!   so a lost message can't leak a member.
//! - The pool starts a member when it's asked and none is free, up to `max`.
//!   At max, an ask waits in line, and fails after `wait_ms`.
//! - A lease is the member's alone: everything session-scoped works on it
//!   (transactions over many calls, row locks, advisory locks, `SET`).
//! - On release the member resets (`RESET`: a rollback, then what `DISCARD
//!   ALL` does, in one round trip), which rolls back and releases every
//!   lock, and tells the pool it's free.
//! - A lease past `lease_ms` is taken back by the member itself: it resets,
//!   and the holder's next call gets an error. It doesn't need the pool for
//!   that.
//! - A member free for `idle_ms` stops, and its connection closes.
//! - "Too many clients" from the database: the ask waits instead of failing.
//! - A pooled statement is a lease for one statement.
//!
//! Every connection has a statement timeout (a lock wait counts). The answer
//! to a statement is the same JSON as the object's own SQLite:
//! `{"rows": [[...]], "changes": n}` or `{"error": "...", "code": "SQLSTATE"}`.

use crate::pool;
use postgres::error::SqlState;
use postgres::types::{ToSql, Type};
use tokio_postgres::{Config, NoTls};
use serde_json::{json, Value as Json};
use crate::fleet::Frame;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A session back to new: no transaction, settings, cursors, temp tables,
/// advisory locks, LISTENs, or prepared statements of its last holder's own
/// left (checked in an open, an aborted, and no transaction). The member's
/// own prepared statements stay: they're its cache (`Stmts`).
const RESET: &str = "ROLLBACK; CLOSE ALL; SET SESSION AUTHORIZATION DEFAULT; RESET ALL; UNLISTEN *; SELECT pg_advisory_unlock_all(); DISCARD TEMP; DISCARD SEQUENCES; \
    DO $$DECLARE r record; BEGIN FOR r IN SELECT name FROM pg_prepared_statements WHERE from_sql LOOP EXECUTE format('DEALLOCATE %I', r.name); END LOOP; END$$";

/// The biggest answer a cell can get back (as its SQLite's).
const MAX_ANSWER: usize = crate::store::MAX_RESULT;

pub type Reply = Box<dyn FnOnce(Vec<u8>) + Send>;
/// A lease, or why there isn't one.
pub type LeaseReply = Box<dyn FnOnce(Result<Lease, String>) + Send>;

/// Every Postgres pool in the app's manifest.
#[derive(Clone, Default)]
pub struct PgCfg {
    pub pools: Vec<PoolCfg>,
}

/// One pool and its member process's settings (`manifest.rs` checked them).
/// (No `Debug`: its URL holds a password.)
#[derive(Clone, PartialEq)]
pub struct PoolCfg {
    pub name: String,
    pub url: String,
    pub max: usize,
    pub wait_ms: u64,
    pub lease_ms: u64,
    pub idle_ms: u64,
    /// An object's lease unused this long is released.
    pub session_idle_ms: u64,
    pub statement_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    /// Connect attempts after the first, with backoff.
    pub retries: u32,
    /// When the manager changes, older members stop now (`stop`), not once
    /// their lease is done (`finish`).
    pub on_manager_loss_stop: bool,
}

impl PoolCfg {
    /// The manifest's defaults, for tests.
    #[cfg(test)]
    pub fn new(name: &str, url: &str) -> PoolCfg {
        PoolCfg { name: name.into(), url: url.into(), max: 8, wait_ms: 5000, lease_ms: 60_000, idle_ms: 60_000, session_idle_ms: 60_000, statement_timeout_ms: 5000, connect_timeout_ms: 5000, retries: 3, on_manager_loss_stop: false }
    }
}

impl PgCfg {
    /// The Postgres pools of a resolved manifest (`None`: there are none).
    /// Named `<prefix>/<pool>` (`<org>/<app>`), so no two apps' pools meet.
    pub fn from_manifest(r: &crate::manifest::Resolved, prefix: &str) -> Option<PgCfg> {
        let pools: Vec<PoolCfg> = r
            .pools
            .iter()
            .filter_map(|(name, p)| {
                let member = r.processes.get(p.settings["member"].as_str()?)?;
                (member.kind == "combd:postgres").then_some(())?;
                let (p, m) = (&p.settings, &member.settings);
                let n = |v: &serde_json::Value| v.as_u64().unwrap_or(0);
                Some(PoolCfg {
                    name: format!("{prefix}/{name}"),
                    url: m["url"].as_str()?.to_string(),
                    max: n(&p["max"]) as usize,
                    wait_ms: n(&p["wait_ms"]),
                    lease_ms: n(&p["lease_ms"]),
                    idle_ms: n(&p["idle_ms"]),
                    session_idle_ms: n(&p["session_idle_ms"]),
                    statement_timeout_ms: n(&m["statement_timeout_ms"]),
                    connect_timeout_ms: n(&m["connection.timeout_ms"]),
                    retries: n(&m["connection.retries"]) as u32,
                    on_manager_loss_stop: p["on_manager_loss"].as_str() == Some("stop"),
                })
            })
            .collect();
        (!pools.is_empty()).then_some(PgCfg { pools })
    }
}

/// A member, leased: calls on it are checked against the lease id, so a
/// holder whose lease ran out (or was released) can't reach the next one's.
/// The member may be on this node, or on another (at max, the nearest free
/// one is reused): then each call goes over the wire.
#[derive(Clone)]
pub struct Lease {
    id: u64,
    to: To,
    /// Its pool's `session_idle_ms`.
    pub session_idle: Duration,
}

#[derive(Clone)]
enum To {
    Here(Sender<Job>),
    There { agent: Sender<Msg>, node: u32, key: Key },
}

fn lost() -> Vec<u8> {
    json!({ "error": "the database session was lost" }).to_string().into_bytes()
}

impl Lease {
    pub fn query(&self, req: Vec<u8>, reply: Reply) {
        self.run(req, reply, false);
    }

    fn run(&self, req: Vec<u8>, reply: Reply, release: bool) {
        // A member that's gone can't answer: say so, or the caller waits forever.
        match &self.to {
            To::Here(m) => {
                if let Err(std::sync::mpsc::SendError(Job::Query { reply, .. })) = m.send(Job::Query { lease: self.id, req, reply, release }) {
                    reply(lost());
                }
            }
            To::There { agent, node, key } => {
                if let Err(std::sync::mpsc::SendError(Msg::RemoteQuery { reply, .. })) = agent.send(Msg::RemoteQuery { node: *node, key: *key, lease: self.id, req, reply, release }) {
                    reply(lost());
                }
            }
        }
    }

    /// Done with it: the member resets and goes back to the pool.
    pub fn release(&self) {
        match &self.to {
            To::Here(m) => {
                let _ = m.send(Job::Release(self.id));
            }
            To::There { agent, node, key } => {
                let _ = agent.send(Msg::RemoteRelease { node: *node, key: *key, lease: self.id });
            }
        }
    }
}

enum Job {
    Lease(u64),
    Query { lease: u64, req: Vec<u8>, reply: Reply, release: bool },
    Release(u64),
    Stop,
}

/// A member: (its manager's epoch, its id there).
type Key = (u32, u64);

/// What a pool's agent hears.
enum Msg {
    Ask(LeaseReply),
    /// From its own members.
    Started(Key, Result<(), bool>),
    Resetting(Key),
    Free(Key),
    Gone(Key),
    /// From another node (or itself).
    Net(Env),
    /// A call on a lease of another node's member.
    RemoteQuery { node: u32, key: Key, lease: u64, req: Vec<u8>, reply: Reply, release: bool },
    RemoteRelease { node: u32, key: Key, lease: u64 },
    /// A claim on the manager's epoch went in, or didn't.
    Claimed(u32, bool),
    /// A deploy replaced this pool: let its members finish, then go.
    Retire,
}

/// A message between two nodes' agents of one pool, over the wire.
#[derive(Serialize, Deserialize)]
struct Env {
    pool: String,
    /// The manager's epoch it's about (a member's, for member calls).
    epoch: u32,
    from: u32,
    #[serde(flatten)]
    m: Net,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Net {
    // To the manager.
    Ask { ask: u64, near: Vec<(u32, u64)> },
    Started { member: u64, ok: bool, full: bool },
    Resetting { member: u64 },
    Free { member: u64 },
    Gone { member: u64 },
    /// The asker's own node served it: drop it from the line.
    Cancel { ask: u64 },
    /// Once a second: this node's members, and which are busy (or kept).
    Report { members: Vec<(u64, bool)> },
    // From the manager.
    Start { ask: u64, member: u64, lease: u64 },
    /// To a free member's node: lease it, then tell the asker.
    Grant { ask: u64, asker: u32, member: u64, lease: u64 },
    Fail { ask: u64 },
    Stop { member: u64 },
    /// Hand a kept member back once its lease is done.
    Recall { member: u64 },
    // Between the asker's node and the member's.
    Granted { ask: u64, member: u64, lease: u64 },
    Query { member: u64, lease: u64, id: u64, req: String, release: bool },
    Answer { id: u64, body: String },
    Release { member: u64, lease: u64 },
}

/// This node in a fleet, for its pools: who it is, the wire to the others,
/// and the catalog, which names each pool's manager.
pub struct PoolFleet {
    pub me: u32,
    pub wire: crate::wire::Wire,
    pub cat: Arc<crate::catnode::CatNode>,
    /// When each node was last heard from (its heartbeats).
    pub heard: Arc<Mutex<HashMap<u32, (u64, Instant)>>>,
    pub lease_ms: u64,
}

/// Every pool on this node, by name. A deploy can add pools (`ensure`).
pub struct Pools {
    pools: Mutex<HashMap<String, (PoolCfg, Sender<Msg>)>>,
    fleet: Option<Arc<PoolFleet>>,
    stop: Arc<AtomicBool>,
    /// Round trips (ms) from here to other nodes, for picking the nearest.
    pub rtt: Arc<Mutex<HashMap<u32, u64>>>,
}

impl Pools {
    /// With `fleet`, each pool is one for the whole fleet (one manager, its
    /// members on every node); without, one on this node.
    pub fn start(cfg: &PgCfg, fleet: Option<PoolFleet>, stop: Arc<AtomicBool>) -> Pools {
        let p = Pools { pools: Mutex::new(HashMap::new()), fleet: fleet.map(Arc::new), stop, rtt: Arc::default() };
        p.ensure(&cfg.pools);
        p
    }

    /// Start every pool in `cfgs` that isn't running, or runs with other
    /// settings (a deploy changed them: the new one takes the name, and the
    /// old one's members finish their leases and stop).
    pub fn ensure(&self, cfgs: &[PoolCfg]) {
        let mut pools = self.pools.lock().unwrap();
        for p in cfgs {
            if pools.get(&p.name).is_some_and(|(c, _)| c == p) {
                continue;
            }
            let mut pg: Config = match p.url.parse() {
                Ok(c) => c,
                Err(_) => {
                    // Never print the URL: it holds a password.
                    eprintln!("combd     pools.{}: its url isn't a Postgres URL", p.name);
                    continue;
                }
            };
            // Startup options are what RESET (and so DISCARD ALL) goes back
            // to. A session left in a transaction (its node died) ends on the
            // server soon after its lease would have.
            pg.options(&format!("-c statement_timeout={} -c idle_in_transaction_session_timeout={}", p.statement_timeout_ms, p.lease_ms + 5000));
            pg.connect_timeout(Duration::from_millis(p.connect_timeout_ms));
            pg.keepalives(true).keepalives_idle(Duration::from_secs(10));
            if pg.get_application_name().is_none() {
                pg.application_name("combd");
            }
            let (tx, rx) = channel();
            let agent = Agent::new(p.clone(), pg, self.fleet.clone(), tx.clone(), self.stop.clone(), self.rtt.clone());
            std::thread::spawn(move || agent.run(rx));
            if let Some((_, old)) = pools.insert(p.name.clone(), (p.clone(), tx)) {
                let _ = old.send(Msg::Retire);
            }
        }
    }

    pub fn has(&self, binding: &str) -> bool {
        self.pools.lock().unwrap().contains_key(binding)
    }

    /// One statement on a member leased just for it.
    pub fn query(&self, binding: &str, req: Vec<u8>, reply: Reply) {
        self.lease(
            binding,
            Box::new(move |l| match l {
                Ok(l) => l.run(req, reply, true),
                Err(e) => reply(json!({ "error": e }).to_string().into_bytes()),
            }),
        );
    }

    /// Lease a member: now, once one's started, or once one's free.
    pub fn lease(&self, binding: &str, reply: LeaseReply) {
        let tx = self.pools.lock().unwrap().get(binding).map(|(_, tx)| tx.clone());
        match tx {
            Some(p) => {
                let _ = p.send(Msg::Ask(reply));
            }
            None => reply(Err(format!("no database binding {binding:?}"))),
        }
    }

    /// A pool message from another node.
    pub fn deliver(&self, body: &[u8]) {
        let Ok(env) = serde_json::from_slice::<Env>(body) else { return };
        if let Some((_, tx)) = self.pools.lock().unwrap().get(&env.pool) {
            let _ = tx.send(Msg::Net(env));
        }
    }
}

/// The catalog entry naming a pool's manager: `<org>/<app>#pool/<pool>`.
fn cat_key(pool: &str) -> String {
    match pool.rsplit_once('/') {
        Some((app, p)) => format!("{app}#pool/{p}"),
        None => format!("{pool}#pool/"),
    }
}

/// A manager's epoch for the catalog's check: (node, epoch).
pub fn manager_of(v: &[u8]) -> Option<(u32, u32)> {
    let v: Json = serde_json::from_slice(v).ok()?;
    Some((v["node"].as_u64()? as u32, v["epoch"].as_u64()? as u32))
}

/// The pool's books, on the node that manages it: `pool::Core` decides,
/// this turns its answers into messages.
struct Manager {
    epoch: u32,
    core: pool::Core,
    /// The core's asks: (the asker's node, its own ask id).
    asks: HashMap<u64, (u32, u64)>,
    /// Members starting, and the core ask each is for.
    starts: HashMap<u64, u64>,
    next_ask: u64,
    next_lease: u64,
    /// Each member's node.
    nodes: HashMap<u64, u32>,
}

impl Manager {
    fn new(epoch: u32, cfg: &PoolCfg) -> Manager {
        let core = pool::Core::new(pool::Cfg { max: cfg.max, idle_ms: cfg.idle_ms, wait_ms: cfg.wait_ms });
        Manager { epoch, core, asks: HashMap::new(), starts: HashMap::new(), next_ask: 0, next_lease: 0, nodes: HashMap::new() }
    }

    fn on(&mut self, now: u64, from: u32, m: Net) -> Vec<(u32, Net)> {
        let outs = match m {
            Net::Ask { ask, near } => {
                self.next_ask += 1;
                self.asks.insert(self.next_ask, (from, ask));
                self.core.ask(now, self.next_ask, from as u64, near.into_iter().map(|(n, ms)| (n as u64, ms)).collect())
            }
            Net::Started { member, ok, full } => {
                // Started, or failed for good: that ask is done. Full: it's
                // back in line, under the same id.
                if let Some(a) = self.starts.remove(&member).filter(|_| !full) {
                    self.asks.remove(&a);
                }
                if !ok {
                    self.nodes.remove(&member);
                }
                self.core.started(now, member, ok, full)
            }
            Net::Resetting { member } => {
                self.core.resetting(member);
                vec![]
            }
            Net::Free { member } => self.core.free(now, member),
            Net::Gone { member } => {
                self.nodes.remove(&member);
                self.core.gone(now, member)
            }
            Net::Report { members } => self.core.report(now, from as u64, &members),
            Net::Cancel { ask } => {
                if let Some(a) = self.asks.iter().find(|(_, v)| **v == (from, ask)).map(|(&a, _)| a) {
                    self.asks.remove(&a);
                    self.core.cancel(a);
                }
                vec![]
            }
            _ => vec![],
        };
        self.outs(outs, from)
    }

    fn outs(&mut self, outs: Vec<pool::Out>, from: u32) -> Vec<(u32, Net)> {
        let mut sends = vec![];
        for o in outs {
            match o {
                pool::Out::Grant { ask, member } => {
                    let (Some((asker, their)), Some(&node)) = (self.asks.remove(&ask), self.nodes.get(&member)) else { continue };
                    self.next_lease += 1;
                    sends.push((node, Net::Grant { ask: their, asker, member, lease: self.next_lease }));
                }
                pool::Out::Start { ask, member, node } => {
                    let Some(&(_, their)) = self.asks.get(&ask) else { continue };
                    self.next_lease += 1;
                    self.nodes.insert(member, node as u32);
                    self.starts.insert(member, ask);
                    sends.push((node as u32, Net::Start { ask: their, member, lease: self.next_lease }));
                }
                pool::Out::Fail { ask } => {
                    if let Some((asker, their)) = self.asks.remove(&ask) {
                        sends.push((asker, Net::Fail { ask: their }));
                    }
                }
                pool::Out::Stop { member } => sends.push((self.nodes.get(&member).copied().unwrap_or(from), Net::Stop { member })),
                pool::Out::Recall { member } => {
                    if let Some(&node) = self.nodes.get(&member) {
                        sends.push((node, Net::Recall { member }));
                    }
                }
            }
        }
        sends
    }
}

struct Mine {
    tx: Sender<Job>,
    /// In a lease.
    busy: bool,
    /// Released, cleaning up: free in a moment.
    resetting: bool,
    /// Kept by this node for its own asks (to the manager it's busy), and
    /// since when it's been free here.
    kept: bool,
    free_since: Instant,
    /// The manager wants it back.
    recalled: bool,
}

/// How long a node keeps a member no ask of its own has used, before it
/// goes back to the pool.
const KEEP: Duration = Duration::from_secs(5);
/// Lease ids a node gives out itself, apart from the manager's.
const LOCAL_LEASE: u64 = 1 << 62;

/// A pool's agent on one node: it runs this node's members, sends asks and
/// member news to the manager, serves other nodes' calls on its members,
/// and, if this node is the manager, runs the books too.
struct Agent {
    cfg: PoolCfg,
    pg: Config,
    fleet: Option<Arc<PoolFleet>>,
    me: u32,
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    rtt: Arc<Mutex<HashMap<u32, u64>>>,
    t0: Instant,
    /// The manager, as the catalog names it: (node, epoch).
    mgr: Option<(u32, u32)>,
    managing: Option<Manager>,
    /// Epochs this process claimed (and so may manage).
    claimed: std::collections::HashSet<u32>,
    claiming: bool,
    retiring: bool,
    /// This node's asks, oldest first: (reply, deadline, sent to the manager).
    asks: std::collections::BTreeMap<u64, (LeaseReply, Instant, bool)>,
    next_ask: u64,
    next_lease: u64,
    members: HashMap<Key, Mine>,
    /// Members starting: (their ask, their first lease).
    starting: HashMap<Key, (u64, u64)>,
    /// Calls out on other nodes' members.
    queries: HashMap<u64, (Reply, Instant)>,
    next_query: u64,
    last_tick: Instant,
    last_report: Instant,
    local: std::collections::VecDeque<Env>,
}

impl Agent {
    fn new(cfg: PoolCfg, pg: Config, fleet: Option<Arc<PoolFleet>>, tx: Sender<Msg>, stop: Arc<AtomicBool>, rtt: Arc<Mutex<HashMap<u32, u64>>>) -> Agent {
        let me = fleet.as_ref().map_or(0, |f| f.me);
        let mut a = Agent {
            cfg,
            pg,
            fleet,
            me,
            tx,
            stop,
            rtt,
            t0: Instant::now(),
            mgr: None,
            managing: None,
            claimed: Default::default(),
            claiming: false,
            retiring: false,
            asks: Default::default(),
            next_ask: 0,
            next_lease: LOCAL_LEASE,
            members: HashMap::new(),
            starting: HashMap::new(),
            queries: HashMap::new(),
            next_query: 0,
            last_tick: Instant::now(),
            last_report: Instant::now(),
            local: Default::default(),
        };
        // On its own, this node manages its pools.
        if a.fleet.is_none() {
            a.mgr = Some((0, 1));
            a.managing = Some(Manager::new(1, &a.cfg));
        }
        a
    }

    fn now(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            if self.stop.load(Relaxed) || (self.retiring && self.members.is_empty()) {
                for m in self.members.values() {
                    let _ = m.tx.send(Job::Stop);
                }
                return;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(m) => self.on(m),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if self.last_tick.elapsed() >= Duration::from_millis(100) {
                self.last_tick = Instant::now();
                self.tick();
            }
            while let Some(env) = self.local.pop_front() {
                self.net(env);
            }
        }
    }

    fn send(&mut self, node: u32, epoch: u32, m: Net) {
        let env = Env { pool: self.cfg.name.clone(), epoch, from: self.me, m };
        if node == self.me {
            return self.local.push_back(env);
        }
        if let Some(f) = &self.fleet {
            f.wire.push(node, Frame::Pool { body: serde_json::to_vec(&env).unwrap() });
        }
    }

    /// To the manager of `epoch`, if it's still the one.
    fn to_mgr(&mut self, epoch: u32, m: Net) {
        if let Some((node, e)) = self.mgr.filter(|g| g.1 == epoch) {
            self.send(node, e, m);
        }
    }

    fn near(&self) -> Vec<(u32, u64)> {
        self.rtt.lock().unwrap().iter().map(|(&n, &ms)| (n, ms)).collect()
    }

    fn old(&self, epoch: u32) -> bool {
        self.retiring || self.mgr.is_some_and(|g| epoch < g.1)
    }

    fn session_idle(&self) -> Duration {
        Duration::from_millis(self.cfg.session_idle_ms)
    }

    fn on(&mut self, m: Msg) {
        match m {
            Msg::Ask(reply) => {
                self.next_ask += 1;
                let ask = self.next_ask;
                self.asks.insert(ask, (reply, Instant::now() + Duration::from_millis(self.cfg.wait_ms + 1000), false));
                self.top_up();
            }
            Msg::Started(key, r) => {
                let (ask, lease) = self.starting.remove(&key).unwrap_or_default();
                match r {
                    Ok(()) => match (self.asks.remove(&ask), self.members.get(&key)) {
                        (Some((reply, ..)), Some(m)) => reply(Ok(Lease { id: lease, to: To::Here(m.tx.clone()), session_idle: self.session_idle() })),
                        // Its asker gave up: it goes back to the pool.
                        (None, Some(m)) => {
                            let _ = m.tx.send(Job::Release(lease));
                        }
                        _ => {}
                    },
                    Err(full) => {
                        self.members.remove(&key);
                        // Full: the ask waits in line again.
                        if !full {
                            if let Some((reply, ..)) = self.asks.remove(&ask) {
                                reply(Err("the database is unreachable".into()));
                            }
                        }
                    }
                }
                self.to_mgr(key.0, Net::Started { member: key.1, ok: r.is_ok(), full: r == Err(true) });
            }
            Msg::Resetting(key) => {
                let kept = self.members.get_mut(&key).map(|m| {
                    m.resetting = true;
                    m.kept
                });
                if kept == Some(false) {
                    self.to_mgr(key.0, Net::Resetting { member: key.1 });
                }
            }
            Msg::Free(key) => {
                let old = self.old(key.0);
                let Some(m) = self.members.get_mut(&key) else { return };
                (m.busy, m.resetting, m.free_since) = (false, false, Instant::now());
                // From an older manager, done with its lease: it stops.
                if old {
                    let _ = m.tx.send(Job::Stop);
                    return;
                }
                // Kept: this node's next ask gets it, or it waits for one.
                if m.kept && !m.recalled {
                    return self.top_up();
                }
                m.kept = false;
                self.to_mgr(key.0, Net::Free { member: key.1 });
                self.top_up();
            }
            Msg::Gone(key) => {
                self.members.remove(&key);
                self.to_mgr(key.0, Net::Gone { member: key.1 });
                self.top_up();
            }
            Msg::Net(env) => self.net(env),
            Msg::RemoteQuery { node, key, lease, req, reply, release } => {
                self.next_query += 1;
                let id = self.next_query;
                self.queries.insert(id, (reply, Instant::now() + Duration::from_millis(self.cfg.statement_timeout_ms + 5000)));
                self.send(node, key.0, Net::Query { member: key.1, lease, id, req: String::from_utf8_lossy(&req).into_owned(), release });
            }
            Msg::RemoteRelease { node, key, lease } => self.send(node, key.0, Net::Release { member: key.1, lease }),
            Msg::Claimed(e, ok) => {
                self.claiming = false;
                if ok {
                    self.claimed.insert(e);
                }
            }
            Msg::Retire => {
                self.retiring = true;
                self.managing = None;
                for m in self.members.values().filter(|m| !m.busy) {
                    let _ = m.tx.send(Job::Stop);
                }
            }
        }
    }

    fn net(&mut self, env: Env) {
        let Env { epoch, from, m, .. } = env;
        let cur = self.mgr.map_or(0, |g| g.1);
        match m {
            Net::Ask { .. } | Net::Started { .. } | Net::Resetting { .. } | Net::Free { .. } | Net::Gone { .. } | Net::Cancel { .. } | Net::Report { .. } => {
                let now = self.now();
                let Some(g) = self.managing.as_mut().filter(|g| g.epoch == epoch) else { return };
                let sends = g.on(now, from, m);
                for (node, m) in sends {
                    self.send(node, epoch, m);
                }
            }
            // From a manager that's been replaced: ignore it.
            Net::Start { .. } | Net::Grant { .. } | Net::Fail { .. } if epoch < cur || self.retiring => {}
            Net::Start { ask, member, lease } => {
                if !self.asks.contains_key(&ask) {
                    return self.send(from, epoch, Net::Gone { member });
                }
                let key = (epoch, member);
                let (tx, jobs) = channel();
                self.members.insert(key, Mine { tx, busy: true, resetting: false, kept: true, free_since: Instant::now(), recalled: false });
                self.starting.insert(key, (ask, lease));
                let (pg, me, lease_ms, retries) = (self.pg.clone(), self.tx.clone(), self.cfg.lease_ms, self.cfg.retries);
                std::thread::spawn(move || run_member(key, pg, retries, lease, lease_ms, jobs, me));
            }
            Net::Grant { ask, asker, member, lease } => match self.members.get_mut(&(epoch, member)) {
                Some(m) => {
                    m.busy = true;
                    // Granted to an ask of this node's own: it keeps it after.
                    m.kept = asker == self.me;
                    let _ = m.tx.send(Job::Lease(lease));
                    self.send(asker, epoch, Net::Granted { ask, member, lease });
                }
                None => self.send(from, epoch, Net::Gone { member }),
            },
            Net::Fail { ask } => {
                if let Some((reply, ..)) = self.asks.remove(&ask) {
                    reply(Err("no database connection free in time".into()));
                }
            }
            Net::Stop { member } => {
                if let Some(m) = self.members.get(&(epoch, member)) {
                    let _ = m.tx.send(Job::Stop);
                }
            }
            Net::Recall { member } => {
                let Some(m) = self.members.get_mut(&(epoch, member)) else { return };
                m.recalled = true;
                // Free here now: back it goes. Busy: when its lease is done.
                if m.kept && !m.busy && !m.resetting {
                    m.kept = false;
                    self.to_mgr(epoch, Net::Free { member });
                }
            }
            Net::Granted { ask, member, lease } => {
                let key = (epoch, member);
                let Some((reply, ..)) = self.asks.remove(&ask) else {
                    // Its asker gave up (or got another): hand it back.
                    return self.send(from, epoch, Net::Release { member, lease });
                };
                let to = if from == self.me {
                    match self.members.get(&key) {
                        Some(m) => To::Here(m.tx.clone()),
                        None => return reply(Err("the database session was lost".into())),
                    }
                } else {
                    To::There { agent: self.tx.clone(), node: from, key }
                };
                reply(Ok(Lease { id: lease, to, session_idle: self.session_idle() }));
            }
            Net::Query { member, lease, id, req, release } => {
                let answer = self.answer_to(from, epoch, id);
                match self.members.get(&(epoch, member)) {
                    Some(m) => {
                        if let Err(std::sync::mpsc::SendError(Job::Query { reply, .. })) = m.tx.send(Job::Query { lease, req: req.into_bytes(), reply: answer, release }) {
                            reply(lost());
                        }
                    }
                    None => answer(lost()),
                }
            }
            Net::Answer { id, body } => {
                if let Some((reply, _)) = self.queries.remove(&id) {
                    reply(body.into_bytes());
                }
            }
            Net::Release { member, lease } => {
                if let Some(m) = self.members.get(&(epoch, member)) {
                    let _ = m.tx.send(Job::Release(lease));
                }
            }
        }
    }

    /// Serve this node's asks from the members it keeps, and send the rest
    /// to the manager, but for those a kept member about to be free will
    /// serve (a reset beats a connect).
    fn top_up(&mut self) {
        let Some((_, epoch)) = self.mgr else { return };
        let ready = |m: &Mine| m.kept && !m.recalled && !m.busy && !m.resetting;
        while !self.asks.is_empty() {
            // The one freed last: its reset goes out with this statement, and
            // its statements are the warmest.
            let Some(key) = self.members.iter().filter(|(k, m)| k.0 == epoch && ready(m)).max_by_key(|(_, m)| m.free_since).map(|(k, _)| *k) else { break };
            self.lend(key);
        }
        let soon = self.members.iter().filter(|(k, m)| k.0 == epoch && m.kept && !m.recalled && m.resetting).count();
        let unsent: Vec<u64> = self.asks.iter().filter(|(_, a)| !a.2).map(|(&k, _)| k).skip(soon).collect();
        if unsent.is_empty() {
            return;
        }
        let near = self.near();
        for ask in unsent {
            self.asks.get_mut(&ask).unwrap().2 = true;
            self.to_mgr(epoch, Net::Ask { ask, near: near.clone() });
        }
    }

    /// Lend kept member `key` to this node's oldest ask, with no word to the
    /// manager (but to drop that ask, if it had it).
    fn lend(&mut self, key: Key) {
        let Some(ask) = self.asks.keys().next().copied() else { return };
        let (reply, _, sent) = self.asks.remove(&ask).unwrap();
        self.next_lease += 1;
        let lease = self.next_lease;
        let m = self.members.get_mut(&key).unwrap();
        m.busy = true;
        let tx = m.tx.clone();
        let _ = tx.send(Job::Lease(lease));
        if sent {
            self.to_mgr(key.0, Net::Cancel { ask });
        }
        reply(Ok(Lease { id: lease, to: To::Here(tx), session_idle: self.session_idle() }));
    }

    /// Where a member's answer to another node's call goes: back over the
    /// wire, from the member's own thread.
    fn answer_to(&self, to: u32, epoch: u32, id: u64) -> Reply {
        let (wire, pool, me) = (self.fleet.as_ref().map(|f| f.wire.clone()), self.cfg.name.clone(), self.me);
        Box::new(move |b| {
            let env = Env { pool, epoch, from: me, m: Net::Answer { id, body: String::from_utf8_lossy(&b).into_owned() } };
            if let Some(w) = wire {
                w.push(to, Frame::Pool { body: serde_json::to_vec(&env).unwrap() });
            }
        })
    }

    fn tick(&mut self) {
        let t = Instant::now();
        let late: Vec<u64> = self.asks.iter().filter(|(_, (_, d, _))| t >= *d).map(|(&a, _)| a).collect();
        for a in late {
            let (reply, _, sent) = self.asks.remove(&a).unwrap();
            if sent {
                if let Some(e) = self.mgr.map(|g| g.1) {
                    self.to_mgr(e, Net::Cancel { ask: a });
                }
            }
            reply(Err("no database connection free in time".into()));
        }
        let late: Vec<u64> = self.queries.iter().filter(|(_, (_, d))| t >= *d).map(|(&q, _)| q).collect();
        for q in late {
            let (reply, _) = self.queries.remove(&q).unwrap();
            reply(lost());
        }
        if let Some(f) = self.fleet.clone() {
            self.watch(&f);
        }
        // Kept and unused for a while: back to the pool, for other nodes.
        let back: Vec<Key> = self.members.iter().filter(|(_, m)| m.kept && !m.busy && !m.resetting && m.free_since.elapsed() >= KEEP).map(|(k, _)| *k).collect();
        for key in back {
            self.members.get_mut(&key).unwrap().kept = false;
            self.to_mgr(key.0, Net::Free { member: key.1 });
        }
        self.top_up();
        let now = self.now();
        if let Some(g) = &mut self.managing {
            let (epoch, outs) = (g.epoch, g.core.tick(now));
            let me = self.me;
            let sends = g.outs(outs, me);
            for (node, m) in sends {
                self.send(node, epoch, m);
            }
        }
        // The books catch up with lost messages: once a second, tell the
        // manager what this node's members are doing.
        if self.last_report.elapsed() >= Duration::from_secs(1) {
            self.last_report = Instant::now();
            if let Some((node, e)) = self.mgr {
                let members = self.members.iter().filter(|(k, _)| k.0 == e).map(|(k, m)| (k.1, m.busy || m.kept)).collect();
                self.send(node, e, Net::Report { members });
            }
        }
    }

    /// Who manages the pool, from the catalog; take over if needed.
    fn watch(&mut self, f: &PoolFleet) {
        let key = cat_key(&self.cfg.name);
        // ponytail: a copy of the whole catalog per pool per tick; a cheaper
        // look-up if catalogs get big.
        let cur = f.cat.now().0.apps.get(&key).and_then(|v| manager_of(v));
        if cur != self.mgr {
            self.mgr = cur;
            // A new manager: members of older ones finish their lease (or
            // stop now), and every ask goes to the new one.
            let stop_now = self.cfg.on_manager_loss_stop;
            for (k, m) in &self.members {
                if self.retiring || cur.is_some_and(|g| k.0 < g.1) && (stop_now || !m.busy) {
                    let _ = m.tx.send(Job::Stop);
                }
            }
            for a in self.asks.values_mut() {
                a.2 = false;
            }
            self.top_up();
        }
        match cur {
            Some((n, e)) if n == self.me && self.claimed.contains(&e) && !self.retiring => {
                if self.managing.as_ref().map(|g| g.epoch) != Some(e) {
                    self.managing = Some(Manager::new(e, &self.cfg));
                }
            }
            _ => self.managing = None,
        }
        // Take over: none yet, ours from an earlier process, or one not
        // heard from for a lease. Only with something to do.
        let quiet = |n: u32| f.heard.lock().unwrap().get(&n).map_or(self.t0.elapsed(), |(_, t)| t.elapsed());
        let dead = match cur {
            None => true,
            Some((n, e)) if n == self.me => !self.claimed.contains(&e),
            Some((n, _)) => quiet(n) >= Duration::from_millis(f.lease_ms),
        };
        let busy = !self.asks.is_empty() || !self.members.is_empty();
        if dead && busy && !self.claiming && !self.retiring {
            self.claiming = true;
            let e = cur.map_or(1, |g| g.1 + 1);
            let (cat, tx, me) = (f.cat.clone(), self.tx.clone(), self.me);
            std::thread::spawn(move || {
                let ok = cat.submit(&key, json!({ "node": me, "epoch": e }).to_string().into_bytes()).is_ok();
                let _ = tx.send(Msg::Claimed(e, ok));
            });
        }
    }
}

/// After a lease, how long the reset may wait to go out with the next
/// holder's first statement (one round trip, not two). Quiet past this, it
/// goes alone, so nothing the last holder left (a transaction, a lock) is
/// held much longer than its lease.
const DEFER: Duration = Duration::from_millis(2);

/// One member: a connection on its own thread, leased to one holder at a
/// time, from its first lease (the ask it started for).
fn run_member(id: Key, pg: Config, retries: u32, first: u64, lease_ms: u64, jobs: Receiver<Job>, pool: Sender<Msg>) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(_) => return drop(pool.send(Msg::Started(id, Err(false)))),
    };
    // A failed connect is tried again, 100 ms then doubling, `retries`
    // times. "Too many clients" isn't: the pool puts the ask back in line.
    let mut tries = 0;
    let client = loop {
        match rt.block_on(pg.connect(NoTls)) {
            Ok((c, conn)) => {
                rt.spawn(async move {
                    let _ = conn.await;
                });
                break c;
            }
            Err(e) => {
                let full = e.code() == Some(&SqlState::TOO_MANY_CONNECTIONS);
                if !full && tries < retries {
                    std::thread::sleep(Duration::from_millis(100 << tries));
                    tries += 1;
                    continue;
                }
                if !full {
                    eprintln!("combd     postgres: can't connect: {e}");
                }
                let _ = pool.send(Msg::Started(id, Err(full)));
                return;
            }
        }
    };
    let mut lease = Some((first, Instant::now() + Duration::from_millis(lease_ms)));
    let mut stmts = Stmts::new();
    let _ = pool.send(Msg::Started(id, Ok(())));
    // A broken connection (its session, transaction, and locks are gone)
    // serves out its lease with errors, then the member stops.
    let mut broken = false;
    // The last holder's session still needs its reset, by this time.
    let mut dirty: Option<Instant> = None;
    // A lease is over: the member is free at once, and its reset waits a
    // moment for the next holder's first statement.
    let end_lease = |lease: &mut Option<(u64, Instant)>, dirty: &mut Option<Instant>, broken: bool| -> bool {
        *lease = None;
        if broken {
            return false;
        }
        *dirty = Some(Instant::now() + DEFER);
        let _ = pool.send(Msg::Free(id));
        true
    };
    loop {
        let wake = [lease.map(|l| l.1), dirty].into_iter().flatten().min();
        let job = match wake {
            Some(t) => match jobs.recv_timeout(t.saturating_duration_since(Instant::now())) {
                Ok(j) => Some(j),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match jobs.recv() {
                Ok(j) => Some(j),
                Err(_) => break,
            },
        };
        // No statement came: reset alone.
        if dirty.is_some_and(|t| Instant::now() >= t) {
            if rt.block_on(client.batch_execute(RESET)).is_err() {
                break;
            }
            dirty = None;
        }
        // Past its time: take it back.
        if lease.is_some_and(|(_, until)| Instant::now() >= until) && !end_lease(&mut lease, &mut dirty, broken) {
            break;
        }
        match job {
            None => {}
            Some(Job::Lease(l)) => lease = Some((l, Instant::now() + Duration::from_millis(lease_ms))),
            Some(Job::Query { lease: l, req, reply, release }) => {
                if lease.map(|x| x.0) != Some(l) {
                    reply(json!({ "error": "the database lease ran out or was released" }).to_string().into_bytes());
                    continue;
                }
                let answer = if broken {
                    json!({ "error": "the database session was lost" })
                } else if dirty.take().is_some() {
                    // The reset, then the statement, sent back to back: the
                    // server runs them in order, so it starts clean.
                    let (r, a) = rt.block_on(in_order(client.batch_execute(RESET), run(&client, &mut stmts, &req)));
                    if r.is_err() {
                        broken = true;
                        json!({ "error": "the database session was lost" })
                    } else {
                        a
                    }
                } else {
                    rt.block_on(run(&client, &mut stmts, &req))
                };
                broken |= client.is_closed();
                let mut bytes = serde_json::to_vec(&answer).unwrap_or_default();
                if bytes.len() > MAX_ANSWER {
                    bytes = br#"{"error":"result too large"}"#.to_vec();
                }
                reply(bytes);
                if release && !end_lease(&mut lease, &mut dirty, broken) {
                    break;
                }
            }
            Some(Job::Release(l)) => {
                if lease.map(|x| x.0) == Some(l) && !end_lease(&mut lease, &mut dirty, broken) {
                    break;
                }
            }
            Some(Job::Stop) => break,
        }
    }
    let _ = pool.send(Msg::Gone(id));
}

/// Run two futures at once, polling `a` first each time, so whatever `a`
/// sends goes out before `b`'s (the client sends in the order it's asked).
async fn in_order<A: std::future::Future, B: std::future::Future>(a: A, b: B) -> (A::Output, B::Output) {
    use std::task::Poll;
    let (mut a, mut b) = (std::pin::pin!(a), std::pin::pin!(b));
    let (mut ra, mut rb) = (None, None);
    std::future::poll_fn(|cx| {
        if ra.is_none() {
            if let Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        if ra.is_some() && rb.is_some() {
            Poll::Ready((ra.take().unwrap(), rb.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

/// A member's prepared statements, by their SQL: a statement it has seen
/// before skips the prepare (a round trip, a third of a pooled statement's).
/// Apps run the same few statements over and over.
type Stmts = HashMap<String, tokio_postgres::Statement>;
const MAX_STMTS: usize = 256;

/// One statement: `{"q": sql, "p": [params]}`.
async fn run(c: &tokio_postgres::Client, stmts: &mut Stmts, req: &[u8]) -> Json {
    match exec(c, stmts, req).await {
        Ok(v) => v,
        Err(e) => e,
    }
}

/// A cached statement that's gone: deallocated by the user, or its plan
/// changed shape (a table it reads was altered).
fn stale(e: &tokio_postgres::Error) -> bool {
    e.code() == Some(&SqlState::INVALID_SQL_STATEMENT_NAME) || (e.code() == Some(&SqlState::FEATURE_NOT_SUPPORTED) && e.as_db_error().is_some_and(|d| d.message().contains("cached plan")))
}

fn err(e: tokio_postgres::Error) -> Json {
    let code = e.code().map(|c| c.code().to_string());
    let msg = e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string());
    json!({ "error": msg, "code": code })
}

async fn exec(c: &tokio_postgres::Client, stmts: &mut Stmts, req: &[u8]) -> Result<Json, Json> {
    let req: Json = serde_json::from_slice(req).map_err(|e| json!({ "error": format!("bad request: {e}") }))?;
    let q = req["q"].as_str().ok_or_else(|| json!({ "error": "missing q" }))?;
    let empty = vec![];
    let ps = match &req["p"] {
        Json::Null => &empty,
        Json::Array(a) => a,
        _ => return Err(json!({ "error": "p must be an array" })),
    };
    // Twice at most: a cached statement gone stale is prepared again, once.
    let mut again = true;
    loop {
        let cached = stmts.get(q).cloned();
        let stmt = match &cached {
            Some(s) => s.clone(),
            None => {
                let s = c.prepare(q).await.map_err(err)?;
                if stmts.len() >= MAX_STMTS {
                    stmts.clear();
                }
                stmts.insert(q.to_string(), s.clone());
                s
            }
        };
        if stmt.params().len() != ps.len() {
            return Err(json!({ "error": format!("the statement takes {} parameters, got {}", stmt.params().len(), ps.len()) }));
        }
        let owned: Vec<Box<dyn ToSql + Sync>> = stmt.params().iter().zip(ps).enumerate().map(|(i, (t, v))| param(i + 1, t, v)).collect::<Result<_, _>>()?;
        let args: Vec<&(dyn ToSql + Sync)> = owned.iter().map(|b| b.as_ref()).collect();
        let res = if stmt.columns().is_empty() { c.execute(&stmt, &args).await.map(|n| (vec![], n)) } else { c.query(&stmt, &args).await.map(|r| (r, 0)) };
        let (rows, n) = match res {
            Ok(r) => r,
            Err(e) if again && cached.is_some() && stale(&e) => {
                again = false;
                stmts.remove(q);
                continue;
            }
            Err(e) => return Err(err(e)),
        };
        if stmt.columns().is_empty() {
            return Ok(json!({ "rows": [], "changes": n }));
        }
        let mut out = Vec::with_capacity(rows.len());
        let mut size = 0usize;
        for row in &rows {
            let mut r = Vec::with_capacity(row.len());
            for (i, col) in row.columns().iter().enumerate() {
                let v = column(row, i, col.type_())?;
                size += match &v {
                    Json::String(s) => s.len() + 8,
                    _ => 24,
                };
                if size > MAX_ANSWER {
                    return Err(json!({ "error": "result too large" }));
                }
                r.push(v);
            }
            out.push(Json::Array(r));
        }
        return Ok(json!({ "rows": out, "changes": rows.len() }));
    }
}

/// A JSON parameter, as the type the statement wants.
fn param(n: usize, t: &Type, v: &Json) -> Result<Box<dyn ToSql + Sync>, Json> {
    let bad = || json!({ "error": format!("parameter ${n} is {t}: can't use {v}") });
    let text = || match v {
        Json::String(s) => Some(s.clone()),
        Json::Null => None,
        other => Some(other.to_string()),
    };
    Ok(match *t {
        Type::BOOL => Box::new(if v.is_null() { None } else { Some(v.as_bool().ok_or_else(bad)?) }),
        Type::INT2 => Box::new(if v.is_null() { None } else { Some(i16::try_from(v.as_i64().ok_or_else(bad)?).map_err(|_| bad())?) }),
        Type::INT4 => Box::new(if v.is_null() { None } else { Some(i32::try_from(v.as_i64().ok_or_else(bad)?).map_err(|_| bad())?) }),
        Type::INT8 => Box::new(if v.is_null() { None } else { Some(v.as_i64().ok_or_else(bad)?) }),
        Type::FLOAT4 => Box::new(if v.is_null() { None } else { Some(v.as_f64().ok_or_else(bad)? as f32) }),
        Type::FLOAT8 => Box::new(if v.is_null() { None } else { Some(v.as_f64().ok_or_else(bad)?) }),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => Box::new(text()),
        Type::JSON | Type::JSONB => Box::new(if v.is_null() { None } else { Some(v.clone()) }),
        _ => return Err(json!({ "error": format!("parameter ${n} is {t}, which we can't send yet: cast it from text (${n}::text::{t})") })),
    })
}

/// A column, as JSON.
fn column(row: &tokio_postgres::Row, i: usize, t: &Type) -> Result<Json, Json> {
    let get_err = |e: tokio_postgres::Error| json!({ "error": e.to_string() });
    Ok(match *t {
        Type::VOID => Json::Null,
        Type::BOOL => json!(row.try_get::<_, Option<bool>>(i).map_err(get_err)?),
        Type::INT2 => json!(row.try_get::<_, Option<i16>>(i).map_err(get_err)?),
        Type::INT4 => json!(row.try_get::<_, Option<i32>>(i).map_err(get_err)?),
        Type::INT8 => json!(row.try_get::<_, Option<i64>>(i).map_err(get_err)?),
        Type::OID => json!(row.try_get::<_, Option<u32>>(i).map_err(get_err)?),
        Type::FLOAT4 => json!(row.try_get::<_, Option<f32>>(i).map_err(get_err)?),
        Type::FLOAT8 => json!(row.try_get::<_, Option<f64>>(i).map_err(get_err)?),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => json!(row.try_get::<_, Option<String>>(i).map_err(get_err)?),
        Type::JSON | Type::JSONB => row.try_get::<_, Option<Json>>(i).map_err(get_err)?.unwrap_or(Json::Null),
        _ => return Err(json!({ "error": format!("column {} is {t}, which we can't read yet: cast it to text", i + 1) })),
    })
}
