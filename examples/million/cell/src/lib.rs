//! One Million Checkboxes, a combd example. This module runs three
//! processes (it asks `COMBD_PROCESS` which one it is); the fourth, the
//! viewer (one per WebSocket), is its own small module in `viewer/`.
//! - `web`, http: the page, and the count of checked boxes;
//! - `chunk`, durable (`0` to `63`, 15,625 boxes each): its bits, in memory
//!   and in its SQLite, and the viewers watching it. A change goes straight
//!   to their sockets, without waking them;
//! - `boxes`, durable (one, `all`): each chunk's count, for the total.

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;

const INDEX: &str = include_str!("../../client/index.html");

/// Boxes in a chunk: 64 chunks make a million.
const CHUNK: usize = 15_625;
/// A subscription lasts this long unless the viewer renews it (every 20 s),
/// so a viewer lost with its node leaves nothing behind.
const SUB_MS: i64 = 60_000;

#[link(wasm_import_module = "combd")]
extern "C" {
    fn now_ms() -> i64;
    fn tick_every(ms: u64) -> u32;
    fn call(ptr: *const u8, len: u32) -> u32;
    fn send(ptr: *const u8, len: u32) -> u32;
    fn pg_read(ptr: *mut u8, cap: u32) -> u32;
    fn sql(ptr: *const u8, len: u32) -> u32;
    fn sql_read(ptr: *mut u8);
}

#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn environ_sizes_get(count: *mut u32, size: *mut u32) -> u32;
    fn environ_get(ptrs: *mut *mut u8, buf: *mut u8) -> u32;
}

fn env(name: &str) -> Option<String> {
    let (mut n, mut size) = (0u32, 0u32);
    unsafe { environ_sizes_get(&mut n, &mut size) };
    let mut ptrs = vec![core::ptr::null_mut::<u8>(); n as usize];
    let mut buf = vec![0u8; size as usize];
    unsafe { environ_get(ptrs.as_mut_ptr(), buf.as_mut_ptr()) };
    let want = format!("{name}=");
    buf.split(|&b| b == 0).filter_map(|e| core::str::from_utf8(e).ok()).find_map(|e| e.strip_prefix(want.as_str()).map(String::from))
}

/// One statement on the object's SQLite. An error panics: let it crash
/// (the host rolls the request back).
fn query(q: &str, p: &[Value]) -> Vec<Value> {
    let req = json!({ "q": q, "p": p }).to_string();
    let n = unsafe { sql(req.as_ptr(), req.len() as u32) } as usize;
    let mut out = vec![0u8; n];
    unsafe { sql_read(out.as_mut_ptr()) };
    let v: Value = serde_json::from_slice(&out).expect("bad sql answer");
    if let Some(e) = v.get("error") {
        panic!("sql: {e}");
    }
    v["rows"].as_array().cloned().unwrap_or_default()
}

/// Another process, and its answer: `{"status", "body"}` or `{"error"}`.
fn ask(msg: &Value) -> Value {
    let m = msg.to_string();
    let n = unsafe { call(m.as_ptr(), m.len() as u32) } as usize;
    let mut out = vec![0u8; n];
    let got = unsafe { pg_read(out.as_mut_ptr(), n as u32) } as usize;
    out.truncate(got);
    serde_json::from_slice(&out).unwrap_or(Value::Null)
}

fn tell(msg: &Value) {
    let m = msg.to_string();
    unsafe { send(m.as_ptr(), m.len() as u32) };
}

/// Straight to a viewer's socket: the viewer doesn't run.
fn push(sock: u64, msg: &str) {
    tell(&json!({ "to": "viewer", "id": sock.to_string(), "ws": msg }));
}

fn err(code: u16, msg: &str) -> String {
    format!("{code} {}", json!({ "error": msg }))
}

/// One request: `api METHOD PATH BODY` (http) or `do METHOD PATH BODY`
/// (an object).
pub fn run(req: &str) -> String {
    let mut parts = req.splitn(4, ' ');
    let (_, method, path, body) = (parts.next(), parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match env("COMBD_PROCESS").as_deref() {
        Some("web") => match (method, path.split('?').next().unwrap_or("")) {
            ("GET", "/") => format!("200\ncontent-type: text/html; charset=utf-8\ncache-control: no-cache\n\n{INDEX}"),
            ("GET", "/api/count") => {
                let r = ask(&json!({ "to": "boxes", "id": "all", "method": "GET", "path": "/total" }));
                format!("{} {}", r["status"].as_u64().unwrap_or(503), r["body"].as_str().unwrap_or("{}"))
            }
            _ => err(404, "not found"),
        },
        Some("chunk") => chunk(method, path, &v),
        Some("boxes") => boxes(method, path, &v),
        other => err(500, &format!("million: no process {other:?}")),
    }
}

// ---------------------------------------------------------------- chunk

struct Chunk {
    /// Which chunk it is (0 to 63): the viewers say, as they're the only
    /// ones that can reach it.
    n: u64,
    bits: Vec<u8>,
    checked: u64,
    /// Viewers watching, by socket, and until when.
    subs: HashMap<u64, i64>,
    /// The count changed since it last told `boxes`.
    dirty: bool,
}

thread_local! {
    static CH: RefCell<Option<Chunk>> = const { RefCell::new(None) };
}

fn load() -> Chunk {
    query("CREATE TABLE IF NOT EXISTS on_ (i INTEGER PRIMARY KEY)", &[]);
    let mut bits = vec![0u8; CHUNK.div_ceil(8)];
    let rows = query("SELECT i FROM on_", &[]);
    for r in &rows {
        let i = r[0].as_u64().unwrap_or(0) as usize;
        bits[i / 8] |= 1 << (i % 8);
    }
    // The total hears from a chunk with boxes checked as soon as it wakes,
    // not only after its next change.
    let dirty = !rows.is_empty();
    if dirty {
        unsafe { tick_every(1000) };
    }
    Chunk { n: 0, bits, checked: rows.len() as u64, subs: HashMap::new(), dirty }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn chunk(method: &str, path: &str, v: &Value) -> String {
    CH.with(|cell| {
        let mut cell = cell.borrow_mut();
        let c = cell.get_or_insert_with(load);
        let now = unsafe { now_ms() };
        c.subs.retain(|_, until| *until > now);
        if let Some(n) = v["n"].as_u64() {
            c.n = n;
        }
        match (method, path) {
            // Watch (or keep watching): every box first, then each change.
            ("POST", "/sub") => {
                let Some(sock) = v["sock"].as_u64() else { return err(400, "sock") };
                if c.subs.insert(sock, now + SUB_MS).is_none() {
                    push(sock, &json!({ "t": "all", "n": c.n, "bits": hex(&c.bits) }).to_string());
                }
            }
            ("POST", "/unsub") => {
                c.subs.remove(&v["sock"].as_u64().unwrap_or(0));
            }
            // `v` is what the box should be, not a toggle: two people
            // clicking at once both get what they clicked.
            ("POST", "/set") => {
                let (Some(i), Some(on)) = (v["i"].as_u64().map(|i| i as usize).filter(|&i| i < CHUNK), v["v"].as_bool()) else {
                    return err(400, "want {\"i\", \"v\"}");
                };
                if (c.bits[i / 8] & (1 << (i % 8)) != 0) == on {
                    return "200 {}".into();
                }
                c.bits[i / 8] ^= 1 << (i % 8);
                if on {
                    query("INSERT INTO on_ (i) VALUES (?)", &[json!(i)]);
                    c.checked += 1;
                } else {
                    query("DELETE FROM on_ WHERE i = ?", &[json!(i)]);
                    c.checked -= 1;
                }
                // Out once the write is on disk (the host holds them).
                // ponytail: one push per watcher per set; one per node is next.
                let msg = json!({ "t": "set", "n": c.n, "i": i, "v": on }).to_string();
                for &sock in c.subs.keys() {
                    push(sock, &msg);
                }
                // The total hears at most once a second.
                if !c.dirty {
                    c.dirty = true;
                    unsafe { tick_every(1000) };
                }
            }
            ("TICK", _) => {
                if c.dirty {
                    tell(&json!({ "to": "boxes", "id": "all", "path": "/count", "body": json!({ "n": c.n, "checked": c.checked }).to_string() }));
                    c.dirty = false;
                } else {
                    unsafe { tick_every(0) };
                }
            }
            _ => return err(404, "not found"),
        }
        "200 {}".into()
    })
}

// ---------------------------------------------------------------- boxes

thread_local! {
    static COUNTS: RefCell<Option<HashMap<u64, u64>>> = const { RefCell::new(None) };
}

fn boxes(method: &str, path: &str, v: &Value) -> String {
    COUNTS.with(|cell| {
        let mut cell = cell.borrow_mut();
        let counts = cell.get_or_insert_with(|| {
            query("CREATE TABLE IF NOT EXISTS counts (n INTEGER PRIMARY KEY, checked INTEGER)", &[]);
            query("SELECT n, checked FROM counts", &[]).iter().map(|r| (r[0].as_u64().unwrap_or(0), r[1].as_u64().unwrap_or(0))).collect()
        });
        match (method, path) {
            ("POST", "/count") => {
                let (Some(n), Some(checked)) = (v["n"].as_u64().filter(|&n| n < 64), v["checked"].as_u64()) else { return err(400, "want {\"n\", \"checked\"}") };
                query("INSERT INTO counts (n, checked) VALUES (?, ?) ON CONFLICT (n) DO UPDATE SET checked = excluded.checked", &[json!(n), json!(checked)]);
                counts.insert(n, checked);
                "200 {}".into()
            }
            ("GET", "/total") => format!("200 {}", json!({ "checked": counts.values().sum::<u64>() })),
            _ => err(404, "not found"),
        }
    })
}

#[no_mangle]
pub extern "C" fn alloc(len: u32) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len as usize);
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn release(ptr: *mut u8, len: u32) {
    drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr, len as usize)) });
}

/// The answer, as (ptr << 32) | len; the host frees it with `release`.
#[no_mangle]
pub extern "C" fn handle(ptr: *mut u8, len: u32) -> u64 {
    let req = unsafe { Vec::from_raw_parts(ptr, len as usize, len as usize) };
    let answer = run(core::str::from_utf8(&req).unwrap_or(""));
    let out = Box::into_raw(answer.into_bytes().into_boxed_slice());
    let len = out.len() as u64;
    ((out as *mut u8 as u64) << 32) | len
}
