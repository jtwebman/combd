//! Bring your own database: Postgres, with the host owning the connections.
//!
//! Each binding (`COMBD_PG_<NAME>=postgres://...`) gets a pool of worker
//! threads, one connection each. A cell never holds a socket or a password;
//! it names a binding. Two ways to use one:
//! - **pooled:** one statement on any free connection, then the connection
//!   is reset (`ROLLBACK; DISCARD ALL`) so nothing (a `SET`, a temp table, a
//!   lock) reaches the next user;
//! - **pinned:** a connection reserved for one handle. Everything
//!   session-scoped works on it: transactions over many calls, row locks,
//!   advisory locks, `SET`. When the handle closes, the connection is reset
//!   (which rolls back and releases every lock) and goes back to the pool.
//!
//! Every connection has a statement timeout (a lock wait counts), and at most
//! `max_pinned` connections per binding can be pinned, so pooled queries
//! always find room. Requests that find no free connection wait in order.
//!
//! The answer to a statement is the same JSON as the object's own SQLite:
//! `{"rows": [[...]], "changes": n}` or `{"error": "...", "code": "SQLSTATE"}`.

use postgres::types::{ToSql, Type};
use postgres::{Client, Config, NoTls};
use serde_json::{json, Value as Json};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// The biggest answer a cell can get back (as its SQLite's).
const MAX_ANSWER: usize = crate::store::MAX_RESULT;

pub type Reply = Box<dyn FnOnce(Vec<u8>) + Send>;
/// A pinned connection, or why there isn't one.
pub type PinReply = Box<dyn FnOnce(Result<usize, String>) + Send>;

#[derive(Clone)]
pub struct PgCfg {
    /// (binding, connection URL).
    pub bindings: Vec<(String, String)>,
    pub pool: usize,
    pub max_pinned: usize,
    pub statement_timeout_ms: u64,
}

impl PgCfg {
    /// `COMBD_PG_<NAME>=postgres://...` per binding (the name is lowercased),
    /// `COMBD_PG_POOL` (connections per binding per node, default 8),
    /// `COMBD_PG_TIMEOUT_MS` (per statement, default 5 s).
    pub fn from_env() -> Option<PgCfg> {
        let reserved = ["COMBD_PG_POOL", "COMBD_PG_TIMEOUT_MS", "COMBD_PG_SESSION_IDLE_MS"];
        let bindings: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| k.starts_with("COMBD_PG_") && !reserved.contains(&k.as_str()))
            .map(|(k, v)| (k["COMBD_PG_".len()..].to_lowercase(), v))
            .collect();
        if bindings.is_empty() {
            return None;
        }
        let num = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let pool = num("COMBD_PG_POOL", 8).max(2) as usize;
        Some(PgCfg { bindings, pool, max_pinned: pool - 1, statement_timeout_ms: num("COMBD_PG_TIMEOUT_MS", 5000) })
    }
}

enum Job {
    /// Run a statement; `pinned`: on a reserved connection (no reset after).
    Query { req: Vec<u8>, pinned: bool, reply: Reply },
    /// A pinned connection is done: reset it and give it back.
    Unpin,
}

enum Waiter {
    Query(Vec<u8>, Reply),
    Pin(PinReply),
}

struct PoolSt {
    idle: Vec<usize>,
    pinned: usize,
    waiting: VecDeque<Waiter>,
}

struct Pool {
    workers: Vec<Sender<Job>>,
    st: Mutex<PoolSt>,
    max_pinned: usize,
}

impl Pool {
    /// A worker is free again: give it the next waiter it can take.
    fn release(&self, w: usize) {
        let mut st = self.st.lock().unwrap();
        let can_pin = st.pinned < self.max_pinned;
        let next = st.waiting.iter().position(|x| matches!(x, Waiter::Query(..)) || can_pin);
        match next.and_then(|i| st.waiting.remove(i)) {
            Some(Waiter::Query(req, reply)) => {
                drop(st);
                let _ = self.workers[w].send(Job::Query { req, pinned: false, reply });
            }
            Some(Waiter::Pin(reply)) => {
                st.pinned += 1;
                drop(st);
                reply(Ok(w));
            }
            None => st.idle.push(w),
        }
    }
}

/// Every binding's pool, shared by a node's event loops.
pub struct Pools {
    pools: HashMap<String, Arc<Pool>>,
}

impl Pools {
    pub fn start(cfg: &PgCfg) -> Pools {
        let mut pools = HashMap::new();
        for (name, url) in &cfg.bindings {
            let mut pg: Config = match url.parse() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("combd     postgres binding {name:?}: bad URL: {e}");
                    std::process::exit(1);
                }
            };
            // Startup options are what RESET (and so DISCARD ALL) goes back to.
            pg.options(&format!("-c statement_timeout={}", cfg.statement_timeout_ms));
            pg.connect_timeout(std::time::Duration::from_secs(5));
            pg.application_name("combd");
            let mut senders = vec![];
            let mut rxs = vec![];
            for _ in 0..cfg.pool {
                let (tx, rx) = channel();
                senders.push(tx);
                rxs.push(rx);
            }
            let pool = Arc::new(Pool {
                workers: senders,
                st: Mutex::new(PoolSt { idle: (0..cfg.pool).collect(), pinned: 0, waiting: VecDeque::new() }),
                max_pinned: cfg.max_pinned,
            });
            for (w, rx) in rxs.into_iter().enumerate() {
                let (pool, pg) = (pool.clone(), pg.clone());
                std::thread::spawn(move || worker(w, pg, rx, pool));
            }
            pools.insert(name.clone(), pool);
        }
        Pools { pools }
    }

    pub fn has(&self, binding: &str) -> bool {
        self.pools.contains_key(binding)
    }

    /// One statement on any free connection.
    pub fn query(&self, binding: &str, req: Vec<u8>, reply: Reply) {
        let Some(p) = self.pools.get(binding) else { return reply(no_binding(binding)) };
        let mut st = p.st.lock().unwrap();
        match st.idle.pop() {
            Some(w) => {
                drop(st);
                let _ = p.workers[w].send(Job::Query { req, pinned: false, reply });
            }
            None => st.waiting.push_back(Waiter::Query(req, reply)),
        }
    }

    /// Reserve a connection (when one's free, and pins are under the cap).
    pub fn pin(&self, binding: &str, reply: PinReply) {
        let Some(p) = self.pools.get(binding) else { return reply(Err(String::from_utf8_lossy(&no_binding(binding)).into_owned())) };
        let mut st = p.st.lock().unwrap();
        if st.pinned < p.max_pinned {
            if let Some(w) = st.idle.pop() {
                st.pinned += 1;
                drop(st);
                return reply(Ok(w));
            }
        }
        st.waiting.push_back(Waiter::Pin(reply));
    }

    /// A statement on a pinned connection.
    pub fn query_on(&self, binding: &str, w: usize, req: Vec<u8>, reply: Reply) {
        match self.pools.get(binding) {
            Some(p) => {
                let _ = p.workers[w].send(Job::Query { req, pinned: true, reply });
            }
            None => reply(no_binding(binding)),
        }
    }

    /// Done with a pinned connection: it's reset (rolling back, releasing
    /// locks) and goes back to the pool.
    pub fn unpin(&self, binding: &str, w: usize) {
        if let Some(p) = self.pools.get(binding) {
            p.st.lock().unwrap().pinned -= 1;
            let _ = p.workers[w].send(Job::Unpin);
        }
    }
}

fn no_binding(b: &str) -> Vec<u8> {
    json!({ "error": format!("no database binding {b:?}") }).to_string().into_bytes()
}

/// One connection's thread. It reconnects when it must, except under a pin:
/// a pinned session that broke stays broken (its transaction and locks are
/// gone) until its handle closes.
fn worker(w: usize, cfg: Config, rx: std::sync::mpsc::Receiver<Job>, pool: Arc<Pool>) {
    let mut client: Option<Client> = None;
    let mut broken_pin = false;
    while let Ok(job) = rx.recv() {
        match job {
            Job::Query { req, pinned, reply } => {
                if pinned && broken_pin {
                    reply(json!({ "error": "the database session was lost" }).to_string().into_bytes());
                    continue;
                }
                if client.as_ref().is_none_or(|c| c.is_closed()) {
                    client = cfg.connect(NoTls).map_err(|e| eprintln!("combd     postgres: can't connect: {e}")).ok();
                }
                let answer = match client.as_mut() {
                    Some(c) => run(c, &req),
                    None => json!({ "error": "the database is unreachable" }),
                };
                if client.as_ref().is_some_and(|c| c.is_closed()) {
                    client = None;
                    broken_pin |= pinned;
                }
                let mut bytes = serde_json::to_vec(&answer).unwrap_or_default();
                if bytes.len() > MAX_ANSWER {
                    bytes = br#"{"error":"result too large"}"#.to_vec();
                }
                reply(bytes);
                if !pinned {
                    reset(&mut client);
                    pool.release(w);
                }
            }
            Job::Unpin => {
                broken_pin = false;
                reset(&mut client);
                pool.release(w);
            }
        }
    }
}

/// Clean before reuse: end any transaction, then drop everything a session
/// can carry (settings, temp tables, prepared statements, LISTENs, and
/// advisory locks). If that fails, the connection is closed instead.
fn reset(client: &mut Option<Client>) {
    if let Some(c) = client.as_mut() {
        if c.batch_execute("ROLLBACK; DISCARD ALL").is_err() {
            *client = None;
        }
    }
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
