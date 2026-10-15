//! Bring your own database: Postgres, through a pool the host runs.
//!
//! Each binding (`COMBD_PG_<NAME>=postgres://...`) is a pool (`pool.rs`
//! decides). A **member** is one connection on its own thread. A cell never
//! holds a socket or a password; it asks the pool for a **lease** on a member.
//! - The pool starts a member when it's asked and none is free, up to `max`.
//!   At max, an ask waits in line, and fails after `wait_ms`.
//! - A lease is the member's alone: everything session-scoped works on it
//!   (transactions over many calls, row locks, advisory locks, `SET`).
//! - On release the member resets (`ROLLBACK; DISCARD ALL`), which rolls
//!   back and releases every lock, and tells the pool it's free.
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
use postgres::{Client, Config, NoTls};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// The biggest answer a cell can get back (as its SQLite's).
const MAX_ANSWER: usize = crate::store::MAX_RESULT;

pub type Reply = Box<dyn FnOnce(Vec<u8>) + Send>;
/// A lease, or why there isn't one.
pub type LeaseReply = Box<dyn FnOnce(Result<Lease, String>) + Send>;

/// Every Postgres pool in the app's manifest.
#[derive(Clone)]
pub struct PgCfg {
    pub pools: Vec<PoolCfg>,
}

/// One pool and its member process's settings (`manifest.rs` checked them).
#[derive(Clone)]
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
}

impl PoolCfg {
    /// The manifest's defaults, for tests.
    #[cfg(test)]
    pub fn new(name: &str, url: &str) -> PoolCfg {
        PoolCfg { name: name.into(), url: url.into(), max: 8, wait_ms: 5000, lease_ms: 60_000, idle_ms: 60_000, session_idle_ms: 60_000, statement_timeout_ms: 5000, connect_timeout_ms: 5000, retries: 3 }
    }
}

impl PgCfg {
    /// The Postgres pools of a resolved manifest (`None`: there are none).
    pub fn from_manifest(r: &crate::manifest::Resolved) -> Option<PgCfg> {
        let pools: Vec<PoolCfg> = r
            .pools
            .iter()
            .filter_map(|(name, p)| {
                let member = r.processes.get(p.settings["member"].as_str()?)?;
                (member.kind == "combd:postgres").then_some(())?;
                let (p, m) = (&p.settings, &member.settings);
                let n = |v: &serde_json::Value| v.as_u64().unwrap_or(0);
                Some(PoolCfg {
                    name: name.clone(),
                    url: m["url"].as_str()?.to_string(),
                    max: n(&p["max"]) as usize,
                    wait_ms: n(&p["wait_ms"]),
                    lease_ms: n(&p["lease_ms"]),
                    idle_ms: n(&p["idle_ms"]),
                    session_idle_ms: n(&p["session_idle_ms"]),
                    statement_timeout_ms: n(&m["statement_timeout_ms"]),
                    connect_timeout_ms: n(&m["connection.timeout_ms"]),
                    retries: n(&m["connection.retries"]) as u32,
                })
            })
            .collect();
        (!pools.is_empty()).then_some(PgCfg { pools })
    }
}

/// A member, leased: calls on it are checked against the lease id, so a
/// holder whose lease ran out (or was released) can't reach the next one's.
#[derive(Clone)]
pub struct Lease {
    id: u64,
    member: Sender<Job>,
    /// Its pool's `session_idle_ms`.
    pub session_idle: Duration,
}

impl Lease {
    pub fn query(&self, req: Vec<u8>, reply: Reply) {
        // A member that's gone can't answer: say so, or the caller waits forever.
        if let Err(std::sync::mpsc::SendError(Job::Query { reply, .. })) = self.member.send(Job::Query { lease: self.id, req, reply, release: false }) {
            reply(json!({ "error": "the database session was lost" }).to_string().into_bytes());
        }
    }

    /// Done with it: the member resets and goes back to the pool.
    pub fn release(&self) {
        let _ = self.member.send(Job::Release(self.id));
    }
}

enum Job {
    Lease(u64),
    Query { lease: u64, req: Vec<u8>, reply: Reply, release: bool },
    Release(u64),
    Stop,
}

/// What the pool's thread hears.
enum Msg {
    Ask(LeaseReply),
    Started(u64, Result<(), bool>),
    Resetting(u64),
    Free(u64),
    Gone(u64),
}

/// Every binding's pool, shared by a node's event loops.
pub struct Pools {
    pools: HashMap<String, Sender<Msg>>,
}

impl Pools {
    pub fn start(cfg: &PgCfg) -> Pools {
        let mut pools = HashMap::new();
        for p in &cfg.pools {
            let mut pg: Config = match p.url.parse() {
                Ok(c) => c,
                Err(_) => {
                    // Never print the URL: it holds a password.
                    eprintln!("combd     pools.{}: its url isn't a Postgres URL", p.name);
                    std::process::exit(1);
                }
            };
            // Startup options are what RESET (and so DISCARD ALL) goes back
            // to. A session left in a transaction (its node died) ends on the
            // server soon after its lease would have.
            pg.options(&format!("-c statement_timeout={} -c idle_in_transaction_session_timeout={}", p.statement_timeout_ms, p.lease_ms + 5000));
            pg.connect_timeout(Duration::from_millis(p.connect_timeout_ms));
            pg.keepalives(true).keepalives_idle(Duration::from_secs(10));
            pg.application_name("combd");
            let (tx, rx) = channel();
            let (me, p) = (tx.clone(), p.clone());
            let name = p.name.clone();
            std::thread::spawn(move || run_pool(pg, p, rx, me));
            pools.insert(name, tx);
        }
        Pools { pools }
    }

    pub fn has(&self, binding: &str) -> bool {
        self.pools.contains_key(binding)
    }

    /// One statement on a member leased just for it.
    pub fn query(&self, binding: &str, req: Vec<u8>, reply: Reply) {
        self.lease(
            binding,
            Box::new(move |l| match l {
                Ok(l) => {
                    if let Err(std::sync::mpsc::SendError(Job::Query { reply, .. })) = l.member.send(Job::Query { lease: l.id, req, reply, release: true }) {
                        reply(json!({ "error": "the database session was lost" }).to_string().into_bytes());
                    }
                }
                Err(e) => reply(json!({ "error": e }).to_string().into_bytes()),
            }),
        );
    }

    /// Lease a member: now, once one's started, or once one's free.
    pub fn lease(&self, binding: &str, reply: LeaseReply) {
        match self.pools.get(binding) {
            Some(p) => {
                let _ = p.send(Msg::Ask(reply));
            }
            None => reply(Err(format!("no database binding {binding:?}"))),
        }
    }
}

/// The pool's thread: `pool::Core` decides, this does what it says.
fn run_pool(pg: Config, cfg: PoolCfg, rx: Receiver<Msg>, me: Sender<Msg>) {
    let session_idle = Duration::from_millis(cfg.session_idle_ms);
    let mut core = pool::Core::new(pool::Cfg { max: cfg.max, idle_ms: cfg.idle_ms, wait_ms: cfg.wait_ms });
    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let (mut asks, mut next_ask, mut next_lease) = (HashMap::<u64, LeaseReply>::new(), 0u64, 0u64);
    let mut members: HashMap<u64, Sender<Job>> = HashMap::new();
    // Members starting: (their ask, their first lease).
    let mut starting: HashMap<u64, (u64, u64)> = HashMap::new();
    loop {
        let outs = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Msg::Ask(reply)) => {
                next_ask += 1;
                asks.insert(next_ask, reply);
                core.ask(now(), next_ask, 0, vec![])
            }
            Ok(Msg::Started(m, r)) => {
                let (ask, lease) = starting.remove(&m).unwrap_or_default();
                match r {
                    Ok(()) => {
                        if let Some(reply) = asks.remove(&ask) {
                            reply(Ok(Lease { id: lease, member: members[&m].clone(), session_idle }));
                        }
                    }
                    // Full: the core puts the ask back in line.
                    Err(true) => {}
                    Err(false) => {
                        if let Some(reply) = asks.remove(&ask) {
                            reply(Err("the database is unreachable".into()));
                        }
                    }
                }
                if r.is_err() {
                    members.remove(&m);
                }
                core.started(now(), m, r.is_ok(), r == Err(true))
            }
            Ok(Msg::Resetting(m)) => {
                core.resetting(m);
                vec![]
            }
            Ok(Msg::Free(m)) => core.free(now(), m),
            Ok(Msg::Gone(m)) => {
                members.remove(&m);
                core.gone(now(), m)
            }
            Err(RecvTimeoutError::Timeout) => core.tick(now()),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        for o in outs {
            match o {
                pool::Out::Grant { ask, member } => {
                    next_lease += 1;
                    let tx = members[&member].clone();
                    let _ = tx.send(Job::Lease(next_lease));
                    if let Some(r) = asks.remove(&ask) {
                        r(Ok(Lease { id: next_lease, member: tx, session_idle }));
                    }
                }
                pool::Out::Start { ask, member, .. } => {
                    next_lease += 1;
                    let (tx, jobs) = channel();
                    members.insert(member, tx);
                    starting.insert(member, (ask, next_lease));
                    let (pg, me, lease_ms, lease, retries) = (pg.clone(), me.clone(), cfg.lease_ms, next_lease, cfg.retries);
                    std::thread::spawn(move || run_member(member, pg, retries, lease, lease_ms, jobs, me));
                }
                pool::Out::Fail { ask } => {
                    if let Some(r) = asks.remove(&ask) {
                        r(Err("no database connection free in time".into()));
                    }
                }
                pool::Out::Stop { member } => {
                    if let Some(tx) = members.get(&member) {
                        let _ = tx.send(Job::Stop);
                    }
                }
            }
        }
    }
}

/// One member: a connection on its own thread, leased to one holder at a
/// time, from its first lease (the ask it started for).
fn run_member(id: u64, pg: Config, retries: u32, first: u64, lease_ms: u64, jobs: Receiver<Job>, pool: Sender<Msg>) {
    // A failed connect is tried again, 100 ms then doubling, `retries`
    // times. "Too many clients" isn't: the pool puts the ask back in line.
    let mut tries = 0;
    let mut client = loop {
        match pg.connect(NoTls) {
            Ok(c) => break c,
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
    let _ = pool.send(Msg::Started(id, Ok(())));
    // A broken connection (its session, transaction, and locks are gone)
    // serves out its lease with errors, then the member stops.
    let mut broken = false;
    let end_lease = |client: &mut Client, lease: &mut Option<(u64, Instant)>, broken: bool| -> bool {
        *lease = None;
        let _ = pool.send(Msg::Resetting(id));
        if broken {
            return false;
        }
        // Two statements: in one string they'd share an implicit
        // transaction, which DISCARD ALL refuses to run in.
        if client.batch_execute("ROLLBACK").is_err() || client.batch_execute("DISCARD ALL").is_err() {
            return false;
        }
        let _ = pool.send(Msg::Free(id));
        true
    };
    loop {
        let job = match lease {
            Some((_, until)) => match jobs.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(j) => Some(j),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match jobs.recv() {
                Ok(j) => Some(j),
                Err(_) => break,
            },
        };
        // Past its time: take it back.
        if lease.is_some_and(|(_, until)| Instant::now() >= until) && !end_lease(&mut client, &mut lease, broken) {
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
                let answer = if broken { json!({ "error": "the database session was lost" }) } else { run(&mut client, &req) };
                broken |= client.is_closed();
                let mut bytes = serde_json::to_vec(&answer).unwrap_or_default();
                if bytes.len() > MAX_ANSWER {
                    bytes = br#"{"error":"result too large"}"#.to_vec();
                }
                if release {
                    let _ = pool.send(Msg::Resetting(id));
                }
                reply(bytes);
                if release && !end_lease(&mut client, &mut lease, broken) {
                    break;
                }
            }
            Some(Job::Release(l)) => {
                if lease.map(|x| x.0) == Some(l) && !end_lease(&mut client, &mut lease, broken) {
                    break;
                }
            }
            Some(Job::Stop) => break,
        }
    }
    let _ = pool.send(Msg::Gone(id));
}

/// One statement: `{"q": sql, "p": [params]}`.
fn run(c: &mut Client, req: &[u8]) -> Json {
    match exec(c, req) {
        Ok(v) => v,
        Err(e) => e,
    }
}

fn err(e: postgres::Error) -> Json {
    let code = e.code().map(|c| c.code().to_string());
    let msg = e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string());
    json!({ "error": msg, "code": code })
}

fn exec(c: &mut Client, req: &[u8]) -> Result<Json, Json> {
    let req: Json = serde_json::from_slice(req).map_err(|e| json!({ "error": format!("bad request: {e}") }))?;
    let q = req["q"].as_str().ok_or_else(|| json!({ "error": "missing q" }))?;
    let empty = vec![];
    let ps = match &req["p"] {
        Json::Null => &empty,
        Json::Array(a) => a,
        _ => return Err(json!({ "error": "p must be an array" })),
    };
    let stmt = c.prepare(q).map_err(err)?;
    if stmt.params().len() != ps.len() {
        return Err(json!({ "error": format!("the statement takes {} parameters, got {}", stmt.params().len(), ps.len()) }));
    }
    let owned: Vec<Box<dyn ToSql + Sync>> = stmt.params().iter().zip(ps).enumerate().map(|(i, (t, v))| param(i + 1, t, v)).collect::<Result<_, _>>()?;
    let args: Vec<&(dyn ToSql + Sync)> = owned.iter().map(|b| b.as_ref()).collect();
    if stmt.columns().is_empty() {
        let n = c.execute(&stmt, &args).map_err(err)?;
        return Ok(json!({ "rows": [], "changes": n }));
    }
    let rows = c.query(&stmt, &args).map_err(err)?;
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
    Ok(json!({ "rows": out, "changes": rows.len() }))
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
fn column(row: &postgres::Row, i: usize, t: &Type) -> Result<Json, Json> {
    let get_err = |e: postgres::Error| json!({ "error": e.to_string() });
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
