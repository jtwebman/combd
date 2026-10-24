//! Host functions a cell can import: combd's own, plus a small, safe slice of
//! WASI preview 1. Every engine calls these same functions.
//!
//! No files, no network, no sleeping. Output goes to a capped buffer. The clock
//! is coarse on purpose (timing attacks). Anything else answers "not supported".

use crate::engine::Import;
use crate::store::{Db, MAX_RESULT};
use std::sync::{Arc, Mutex};
use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What the host keeps for one cell.
#[derive(Default, Clone)]
pub struct HostState {
    pub counter: i64,
    /// The app's env vars (`NAME=value`), for WASI's `environ_get`.
    pub environ: Arc<Vec<Vec<u8>>>,
    /// stdout and stderr, capped at `MAX_OUT`.
    pub out: Vec<u8>,
    /// A Durable Object's storage. Cells without it get an error from `sql`.
    pub db: Option<Arc<Mutex<Db>>>,
    /// The answer to the last `sql` call, until `sql_read` takes it.
    pub sql_out: Vec<u8>,
    /// The cell changed its alarm in this request.
    pub alarm_dirty: bool,
    /// `tick_every`: the beat this request asked for (0: stop).
    pub tick: Option<u64>,
    /// Host time this request has used so far (`MAX_HOST_TIME`).
    pub host_time: Duration,
    /// WebSockets: the object's sockets this request may use, and what it
    /// asked to send or close (the host does it after the output gate).
    pub ws_ids: Vec<u64>,
    pub ws_out: Vec<WsOut>,
    /// The binary message this event carries (`ws_msg` copies it out).
    pub ws_in: Vec<u8>,
    /// Postgres: a call the event loop must do while the cell waits, and the
    /// answer to the last one, until `pg_read` takes it. Only engines that
    /// can pause a cell inside a host call set `can_park`.
    pub pending: Option<DbOp>,
    pub pg_out: Vec<u8>,
    pub can_park: bool,
    /// How many results the waiting import declared (0 or 1).
    pub pending_results: u32,
    /// Event cells: the requests this instance may act for, what this
    /// callback asked for (the loop does it when the callback returns), and
    /// the data that came with the event being handled.
    pub ev_reqs: Vec<u64>,
    pub ev_out: Vec<EvAction>,
    /// The bytes `ev_out` holds: capped as a classic cell's sends are
    /// (`MAX_WS_OUT`), or one callback could queue gigabytes.
    pub ev_bytes: usize,
    pub ev_payload: Vec<u8>,
    pub next_ticket: u64,
}

/// What an event cell's callback asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum EvAction {
    /// Answer request `req` (`"200 {...}"`, as a classic cell returns).
    Respond(u64, String),
    /// Start something for request `req`; `on_event(ticket, len)` comes back.
    Async { req: u64, ticket: u64, op: AsyncOp },
    /// Done with a Postgres handle of request `req`.
    PgClose(u64, u64),
    /// `send` to another process: fire and forget.
    Send(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AsyncOp {
    PgOpen(Vec<u8>),
    PgQuery(u64, Vec<u8>),
    Timer(u64),
    /// `call` another process (`DbOp::Call`'s JSON).
    Call(Vec<u8>),
}

/// What one callback may ask for, so a loop in a cell can't flood the host.
const MAX_EV_OUT: usize = 10_000;

/// A Postgres call. The cell waits (paused, no CPU) while the host does it.
#[derive(Clone, Debug, PartialEq)]
pub enum DbOp {
    /// Call another process: `{"to", "id"?, "method"?, "path"?, "body"?,
    /// "timeout_ms"?}`. The result is the answer's length, for `pg_read`:
    /// `{"status", "body"}`, or `{"error"}` (denied, timeout).
    Call(Vec<u8>),
    /// Pin a connection: `{"db": binding}`. The result is a handle (0: no;
    /// why is in `pg_read`).
    Open(Vec<u8>),
    /// One statement, `{"db", "q", "p"}`: on a pinned handle, or pooled
    /// (handle 0). The result is the answer's length, for `pg_read`.
    Query(u64, Vec<u8>),
    /// Done with a handle: it rolls back, releases its locks, and goes back.
    Close(u64),
}

/// A WebSocket action a cell asked for.
#[derive(Clone, Debug, PartialEq)]
/// What a request sends out, once it has succeeded (for an object: once
/// its writes are on disk): WebSocket messages, and messages to processes.
pub enum WsOut {
    Send(u64, String),
    Binary(u64, Vec<u8>),
    Close(u64, u16),
    /// `send` to another process (its JSON).
    Message(Vec<u8>),
}

impl WsOut {
    pub fn id(&self) -> u64 {
        match self {
            WsOut::Send(id, _) | WsOut::Binary(id, _) | WsOut::Close(id, _) => *id,
            WsOut::Message(_) => 0,
        }
    }
    fn len(&self) -> usize {
        match self {
            WsOut::Send(_, m) => m.len(),
            WsOut::Binary(_, b) | WsOut::Message(b) => b.len(),
            WsOut::Close(..) => 0,
        }
    }
}

/// What one request may queue to send, in all. A broadcast loop copies its
/// message once per socket; this keeps that bounded.
pub const MAX_WS_OUT: usize = 16 << 20;

/// The host time one request may use. The cell's fuel counts its own
/// instructions, not ours, so a cell looping over `sql` would otherwise cost
/// it almost nothing; past this, its host calls crash it.
pub const MAX_HOST_TIME: Duration = Duration::from_secs(1);

const MAX_OUT: usize = 64 * 1024;
/// Buffers per `fd_write`, as POSIX's usual IOV_MAX.
const MAX_IOVS: u32 = 1024;
const MAX_RANDOM: u32 = 64 * 1024;
/// Clocks are rounded down to this many nanoseconds (1 ms).
const CLOCK_GRAIN: u64 = 1_000_000;

/// Guest memory, as the host sees it.
pub trait Mem {
    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>>;
    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool;
}

const OK: u64 = 0;
const EBADF: u64 = 8;
const EFAULT: u64 = 21;
const EINVAL: u64 = 28;
const ENOSYS: u64 = 52;
const ENOTCAPABLE: u64 = 76;

/// Run import `imp`. `Err` means the cell must crash.
///
/// A cell declares the types of its own imports, so `args` may be shorter
/// than the function really takes: never index it directly (a cell that
/// imported `sql` with no parameters used to panic the host).
pub fn call(imp: &Import, args: &[u64], mem: &mut dyn Mem, st: &mut HostState) -> Result<Vec<u64>, String> {
    if st.host_time > MAX_HOST_TIME {
        return Err(format!("this request used more than {MAX_HOST_TIME:?} of host time"));
    }
    let t = Instant::now();
    let r = run(imp, args, mem, st);
    st.host_time += t.elapsed();
    r
}

fn run(imp: &Import, args: &[u64], mem: &mut dyn Mem, st: &mut HostState) -> Result<Vec<u64>, String> {
    let arg = |i: usize| args.get(i).copied().ok_or_else(|| format!("{}.{}: too few arguments", imp.module, imp.name));
    let r = match (imp.module.as_str(), imp.name.as_str()) {
        ("combd", "counter_incr") => {
            st.counter += 1;
            st.counter as u64
        }
        // Storage: `sql(ptr, len)` runs one statement and returns the answer's
        // length; `sql_read(ptr)` copies the answer into the cell.
        ("combd", "sql") => {
            let len = (arg(1)? as u32).min(MAX_RESULT as u32 + 1);
            let req = mem.read(arg(0)? as u32, len).ok_or("sql: bad pointer")?;
            st.sql_out = match &st.db {
                Some(db) => {
                    let db = db.lock().unwrap();
                    let out = db.exec_json(&req);
                    // SQLite rolls the whole transaction back on some errors
                    // (out of space, interrupted). The request's earlier
                    // writes are gone, so it can't go on: crash it, and it
                    // fails as a whole, as the database now says.
                    if !db.in_transaction() {
                        return Err("sql: the request's transaction was rolled back (out of space, or too slow)".into());
                    }
                    out
                }
                None => br#"{"error":"this cell has no storage"}"#.to_vec(),
            };
            st.sql_out.len() as u64
        }
        ("combd", "sql_read") => {
            let out = std::mem::take(&mut st.sql_out);
            if !mem.write(arg(0)? as u32, &out) {
                return Err("sql_read: bad pointer".into());
            }
            0
        }
        // Wall clock in ms, as coarse as the WASI clock.
        ("combd", "now_ms") => now_ms() as u64,
        // Alarms: `alarm_set(at_ms)` (0 or less clears it), `alarm_get()` (-1: none).
        // They change the object's storage, so they commit with the request.
        ("combd", "alarm_set") => {
            let db = st.db.as_ref().ok_or("alarm_set: this cell has no storage")?;
            let at = arg(0)? as i64;
            db.lock().unwrap().set_alarm((at > 0).then_some(at)).map_err(|e| format!("alarm_set: {e}"))?;
            st.alarm_dirty = true;
            0
        }
        // `tick_every(ms)`: an object's beat, in memory: `TICK / <n>` every
        // `ms` (10 ms to 60 s; 0 stops it). Nothing is written to disk, so
        // a crash or a move to another node stops it until the object asks
        // again. For game loops, not for work that must happen: that's
        // `alarm_set`.
        ("combd", "tick_every") => {
            if st.db.is_none() {
                return Err("tick_every: only an object can tick".into());
            }
            let ms = arg(0)?;
            st.tick = Some(if ms == 0 { 0 } else { ms.clamp(10, 60_000) });
            0
        }
        ("combd", "alarm_get") => {
            let db = st.db.as_ref().ok_or("alarm_get: this cell has no storage")?;
            db.lock().unwrap().alarm().unwrap_or(-1) as u64
        }
        // WebSockets: `ws_send(id, ptr, len)` queues a text message,
        // `ws_close(id, code)` closes one, `ws_list(ptr, cap)` writes the
        // object's socket ids (u64, little-endian) and returns how many there
        // are. 0 is OK; 1: no such socket; 2: too big, or not UTF-8.
        // (No early `return Ok(..)` here: the result must go through the
        // check at the end, which matches the import's declared type.)
        // `ws_send_binary` is the same, for bytes; `ws_msg(ptr)` copies this
        // event's binary message (its length is in the event) into the cell.
        ("combd", name @ ("ws_send" | "ws_send_binary")) => {
            let (id, len) = (arg(0)?, arg(2)? as u32);
            let queued: usize = st.ws_out.iter().map(WsOut::len).sum();
            if !st.ws_ids.contains(&id) {
                1
            } else if len as usize > crate::ws::MAX_MESSAGE || queued + len as usize > MAX_WS_OUT {
                2
            } else {
                let bytes = mem.read(arg(1)? as u32, len).ok_or("ws_send: bad pointer")?;
                if name == "ws_send_binary" {
                    st.ws_out.push(WsOut::Binary(id, bytes));
                    0
                } else {
                    match String::from_utf8(bytes) {
                        Ok(m) => {
                            st.ws_out.push(WsOut::Send(id, m));
                            0
                        }
                        Err(_) => 2,
                    }
                }
            }
        }
        ("combd", "ws_msg") => {
            let msg = std::mem::take(&mut st.ws_in);
            if !mem.write(arg(0)? as u32, &msg) {
                return Err("ws_msg: bad pointer".into());
            }
            0
        }
        ("combd", "ws_close") => {
            let id = arg(0)?;
            // Codes a server may send: 1000, or 3000 to 4999 (RFC 6455).
            let code = arg(1)? as u32;
            let code = if code == 1000 || (3000..5000).contains(&code) { code as u16 } else { 1000 };
            if st.ws_ids.contains(&id) {
                st.ws_out.push(WsOut::Close(id, code));
                0
            } else {
                1
            }
        }
        ("combd", "ws_list") => {
            let (ptr, cap) = (arg(0)? as u32, arg(1)? as u32);
            let ids: Vec<u8> = st.ws_ids.iter().take(cap as usize).flat_map(|i| i.to_le_bytes()).collect();
            if !mem.write(ptr, &ids) {
                return Err("ws_list: bad pointer".into());
            }
            st.ws_ids.len() as u64
        }
        // Postgres (`pg.rs`): `pg_open(ptr, len)`, `pg_query(handle, ptr,
        // len)`, `pg_close(handle)` wait for the host; `pg_read(ptr)` copies
        // the last answer out.
        // Processes: `call(ptr, len)` waits for the answer (its length is
        // the result; `pg_read` copies it); `send(ptr, len)` queues a message
        // that goes out only if this request succeeds (1: queued, 0: too much).
        ("combd", "send") => {
            let len = arg(1)? as usize;
            let queued: usize = st.ws_out.iter().map(WsOut::len).sum();
            if len > 1 << 20 || queued + len > MAX_WS_OUT {
                0
            } else {
                st.ws_out.push(WsOut::Message(mem.read(arg(0)? as u32, len as u32).ok_or("send: bad pointer")?));
                1
            }
        }
        ("combd", name @ ("pg_open" | "pg_query" | "pg_close" | "call")) => {
            if !st.can_park {
                return Err(format!("{name}: this engine can't wait on the host yet"));
            }
            let req = |p: u64, l: u64| -> Result<Vec<u8>, String> {
                if l > 1 << 20 {
                    return Err(format!("{name}: request too large"));
                }
                mem.read(p as u32, l as u32).ok_or_else(|| format!("{name}: bad pointer"))
            };
            st.pending = Some(match name {
                "pg_open" => DbOp::Open(req(arg(0)?, arg(1)?)?),
                "pg_query" => DbOp::Query(arg(0)?, req(arg(1)?, arg(2)?)?),
                "call" => DbOp::Call(req(arg(0)?, arg(1)?)?),
                _ => DbOp::Close(arg(0)?),
            });
            0 // the real result comes when the host is done
        }
        // `pg_read(ptr, cap)`: at most `cap` bytes; returns how many.
        ("combd", "pg_read") => {
            let out = std::mem::take(&mut st.pg_out);
            let n = out.len().min(arg(1)? as usize);
            if !mem.write(arg(0)? as u32, &out[..n]) {
                return Err("pg_read: bad pointer".into());
            }
            n as u64
        }
        // Event cells (one instance, many requests at once, like Node):
        // `respond(req, ptr, len)` answers a request, now or later, and work
        // may go on after it (waitUntil). Anything that waits returns a
        // ticket at once, and `on_event(ticket, len)` comes back with its
        // data, read by `ev_read(ptr, cap)`. A ticket of 0: unknown request.
        ("combd", name @ ("respond" | "pg_open_async" | "pg_query_async" | "pg_close_async" | "timer_async" | "call_async" | "send_async")) => {
            let req = arg(0)?;
            if !st.ev_reqs.contains(&req) {
                0
            } else if st.ev_out.len() >= MAX_EV_OUT {
                return Err(format!("{name}: too many at once"));
            } else {
                let len = match name {
                    "pg_query_async" => arg(3)?,
                    "respond" | "pg_open_async" | "call_async" | "send_async" => arg(2)?,
                    _ => 0,
                };
                if st.ev_bytes.saturating_add(len as usize) > MAX_WS_OUT {
                    return Err(format!("{name}: too much at once"));
                }
                st.ev_bytes += len as usize;
                let read = |p: u64, l: u64, max: usize| -> Result<Vec<u8>, String> {
                    if l as usize > max {
                        return Err(format!("{name}: too large"));
                    }
                    mem.read(p as u32, l as u32).ok_or_else(|| format!("{name}: bad pointer"))
                };
                let ticket = |op: AsyncOp, st: &mut HostState| {
                    st.next_ticket += 1;
                    st.ev_out.push(EvAction::Async { req, ticket: st.next_ticket, op });
                    st.next_ticket
                };
                match name {
                    "respond" => {
                        let text = String::from_utf8(read(arg(1)?, arg(2)?, crate::store::MAX_RESULT)?).map_err(|_| "respond: not UTF-8")?;
                        st.ev_out.push(EvAction::Respond(req, text));
                        1
                    }
                    "pg_open_async" => ticket(AsyncOp::PgOpen(read(arg(1)?, arg(2)?, 1 << 20)?), st),
                    "pg_query_async" => ticket(AsyncOp::PgQuery(arg(1)?, read(arg(2)?, arg(3)?, 1 << 20)?), st),
                    "call_async" => ticket(AsyncOp::Call(read(arg(1)?, arg(2)?, 1 << 20)?), st),
                    "send_async" => {
                        st.ev_out.push(EvAction::Send(read(arg(1)?, arg(2)?, 1 << 20)?));
                        1
                    }
                    "pg_close_async" => {
                        st.ev_out.push(EvAction::PgClose(req, arg(1)?));
                        1
                    }
                    _ => ticket(AsyncOp::Timer(arg(1)?.min(30_000)), st),
                }
            }
        }
        ("combd", "ev_read") => {
            let n = st.ev_payload.len().min(arg(1)? as usize);
            if !mem.write(arg(0)? as u32, &st.ev_payload[..n]) {
                return Err("ev_read: bad pointer".into());
            }
            n as u64
        }
        ("env", "abort") => return Err("abort".into()),
        ("wasi_snapshot_preview1", "proc_exit") => return Err(format!("exit {}", arg(0)? as i32)),
        ("wasi_snapshot_preview1", name) => wasi(name, args, mem, st).ok_or_else(|| format!("wasi {name}: too few arguments"))?,
        (m, n) => return Err(format!("unknown import {m}.{n}")),
    };
    Ok(if imp.results == 0 { vec![] } else { vec![r] })
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn put(mem: &mut dyn Mem, ptr: u32, bytes: &[u8]) -> u64 {
    if mem.write(ptr, bytes) { OK } else { EFAULT }
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

/// `None`: too few arguments.
fn wasi(name: &str, a: &[u64], mem: &mut dyn Mem, st: &mut HostState) -> Option<u64> {
    // Every function below reads at most its first four arguments.
    let n = match name {
        "fd_write" | "fd_read" => 4,
        "clock_time_get" => 3,
        "random_get" | "clock_res_get" | "args_sizes_get" | "environ_sizes_get" | "environ_get" => 2,
        _ => 0,
    };
    (a.len() >= n).then(|| wasi_run(name, a, mem, st))
}

fn wasi_run(name: &str, a: &[u64], mem: &mut dyn Mem, st: &mut HostState) -> u64 {
    let arg = |i: usize| a[i] as u32;
    match name {
        "fd_write" => {
            if arg(0) != 1 && arg(0) != 2 {
                return EBADF;
            }
            if arg(2) > MAX_IOVS {
                return EINVAL;
            }
            let Some(iovs) = mem.read(arg(1), arg(2) * 8) else { return EFAULT };
            let mut total = 0u32;
            for iov in iovs.chunks_exact(8) {
                // Copy only what still fits under the cap: the cell's fuel
                // doesn't pay for host time, and a buffer can be all of memory.
                let room = MAX_OUT.saturating_sub(st.out.len()) as u32;
                let len = u32_at(iov, 4);
                let Some(bytes) = mem.read(u32_at(iov, 0), len.min(room)) else { return EFAULT };
                st.out.extend_from_slice(&bytes);
                total = total.saturating_add(len);
            }
            put(mem, arg(3), &total.to_le_bytes())
        }
        // stdin is always empty.
        "fd_read" if arg(0) == 0 => put(mem, arg(3), &0u32.to_le_bytes()),
        "random_get" => {
            if arg(1) > MAX_RANDOM {
                return EINVAL;
            }
            let mut b = vec![0u8; arg(1) as usize];
            match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)) {
                Ok(()) => put(mem, arg(0), &b),
                Err(_) => ENOSYS,
            }
        }
        "clock_time_get" => {
            static START: OnceLock<Instant> = OnceLock::new();
            let ns = match arg(0) {
                0 => SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64),
                1..=3 => START.get_or_init(Instant::now).elapsed().as_nanos() as u64,
                _ => return EINVAL,
            };
            put(mem, arg(2), &(ns / CLOCK_GRAIN * CLOCK_GRAIN).to_le_bytes())
        }
        "clock_res_get" => put(mem, arg(1), &CLOCK_GRAIN.to_le_bytes()),
        "args_sizes_get" => match put(mem, arg(0), &0u32.to_le_bytes()) {
            OK => put(mem, arg(1), &0u32.to_le_bytes()),
            err => err,
        },
        // Each var is `NAME=value\0`; `environ_get(ptrs, buf)` writes one
        // pointer per var into `ptrs`, and the strings into `buf`.
        "environ_sizes_get" => {
            let size: usize = st.environ.iter().map(|e| e.len() + 1).sum();
            match put(mem, arg(0), &(st.environ.len() as u32).to_le_bytes()) {
                OK => put(mem, arg(1), &(size as u32).to_le_bytes()),
                err => err,
            }
        }
        "environ_get" => {
            let (mut ptrs, mut buf) = (arg(0), arg(1));
            for e in st.environ.iter() {
                let mut z = e.clone();
                z.push(0);
                if put(mem, ptrs, &buf.to_le_bytes()) != OK || put(mem, buf, &z) != OK {
                    return EFAULT;
                }
                ptrs = ptrs.wrapping_add(4);
                buf = buf.wrapping_add(z.len() as u32);
            }
            OK
        }
        "sched_yield" => OK,
        "args_get" => OK,
        n if n.starts_with("path_") => ENOTCAPABLE,
        _ => ENOSYS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads give `len` zero bytes; writes land nowhere.
    struct Fake;
    impl Mem for Fake {
        fn read(&self, _: u32, len: u32) -> Option<Vec<u8>> {
            Some(vec![0; len as usize])
        }
        fn write(&mut self, _: u32, _: &[u8]) -> bool {
            true
        }
    }

    /// An event cell's callback can queue only as many bytes as a classic
    /// cell can send (`MAX_WS_OUT`), not 10,000 messages of a megabyte each
    /// (it once could: one callback took a node's memory).
    #[test]
    fn a_callback_queues_at_most_max_ws_out() {
        let send_async = Import { module: "combd".into(), name: "send_async".into(), results: 1 };
        let mut st = HostState { ev_reqs: vec![1], ..HostState::default() };
        let per = 1u64 << 20;
        let mut queued = 0;
        while call(&send_async, &[1, 0, per], &mut Fake, &mut st).is_ok() {
            queued += per as usize;
            assert!(queued <= MAX_WS_OUT, "queued {queued} bytes");
        }
        assert_eq!(queued, MAX_WS_OUT);
        // The next callback starts from nothing (the loop resets `ev_bytes`).
        st.ev_bytes = 0;
        assert!(call(&send_async, &[1, 0, per], &mut Fake, &mut st).is_ok());
    }
}
