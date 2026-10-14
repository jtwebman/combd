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
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// What the host keeps for one cell.
#[derive(Default, Clone)]
pub struct HostState {
    pub counter: i64,
    /// stdout and stderr, capped at `MAX_OUT`.
    pub out: Vec<u8>,
    /// A Durable Object's storage. Cells without it get an error from `sql`.
    pub db: Option<Arc<Mutex<Db>>>,
    /// The answer to the last `sql` call, until `sql_read` takes it.
    pub sql_out: Vec<u8>,
    /// The cell changed its alarm in this request.
    pub alarm_dirty: bool,
}

const MAX_OUT: usize = 64 * 1024;
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
pub fn call(imp: &Import, args: &[u64], mem: &mut dyn Mem, st: &mut HostState) -> Result<Vec<u64>, String> {
    let r = match (imp.module.as_str(), imp.name.as_str()) {
        ("combd", "counter_incr") => {
            st.counter += 1;
            st.counter as u64
        }
        // Storage: `sql(ptr, len)` runs one statement and returns the answer's
        // length; `sql_read(ptr)` copies the answer into the cell.
        ("combd", "sql") => {
            let len = (args[1] as u32).min(MAX_RESULT as u32 + 1);
            let req = mem.read(args[0] as u32, len).ok_or("sql: bad pointer")?;
            st.sql_out = match &st.db {
                Some(db) => db.lock().unwrap().exec_json(&req),
                None => br#"{"error":"this cell has no storage"}"#.to_vec(),
            };
            st.sql_out.len() as u64
        }
        ("combd", "sql_read") => {
            let out = std::mem::take(&mut st.sql_out);
            if !mem.write(args[0] as u32, &out) {
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
            let at = args[0] as i64;
            db.lock().unwrap().set_alarm((at > 0).then_some(at)).map_err(|e| format!("alarm_set: {e}"))?;
            st.alarm_dirty = true;
            0
        }
        ("combd", "alarm_get") => {
            let db = st.db.as_ref().ok_or("alarm_get: this cell has no storage")?;
            db.lock().unwrap().alarm().unwrap_or(-1) as u64
        }
        ("env", "abort") => return Err("abort".into()),
        ("wasi_snapshot_preview1", "proc_exit") => return Err(format!("exit {}", args[0] as i32)),
        ("wasi_snapshot_preview1", name) => wasi(name, args, mem, st),
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

fn wasi(name: &str, a: &[u64], mem: &mut dyn Mem, st: &mut HostState) -> u64 {
    let arg = |i: usize| a[i] as u32;
    match name {
        "fd_write" => {
            if arg(0) != 1 && arg(0) != 2 {
                return EBADF;
            }
            let Some(iovs) = arg(2).checked_mul(8).and_then(|n| mem.read(arg(1), n)) else { return EFAULT };
            let mut total = 0u32;
            for iov in iovs.chunks_exact(8) {
                let Some(bytes) = mem.read(u32_at(iov, 0), u32_at(iov, 4)) else { return EFAULT };
                let room = MAX_OUT.saturating_sub(st.out.len());
                st.out.extend_from_slice(&bytes[..bytes.len().min(room)]);
                total = total.saturating_add(bytes.len() as u32);
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
        "args_sizes_get" | "environ_sizes_get" => match put(mem, arg(0), &0u32.to_le_bytes()) {
            OK => put(mem, arg(1), &0u32.to_le_bytes()),
            err => err,
        },
        "args_get" | "environ_get" | "sched_yield" => OK,
        n if n.starts_with("path_") => ENOTCAPABLE,
        _ => ENOSYS,
    }
}
