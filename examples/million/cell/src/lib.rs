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
/// How often a chunk drops watchers whose subscriptions ran out.
const PRUNE_MS: i64 = 10_000;

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

/// Straight to viewers' sockets: the viewers don't run. One send for all
/// of them (`ids`): the host makes the frame once.
fn push(socks: impl Iterator<Item = u64>, msg: &str) {
    let mut m = String::from(r#"{"to":"viewer","ids":["#);
    for (k, sock) in socks.enumerate() {
        if k > 0 {
            m.push(',');
        }
        m.push_str(&sock.to_string());
    }
    m.push_str(r#"],"ws":"#);
    m.push_str(&Value::from(msg).to_string());
    m.push('}');
    unsafe { send(m.as_ptr(), m.len() as u32) };
}

fn err(code: u16, msg: &str) -> String {
    format!("{code} {}", json!({ "error": msg }))
}

/// One request: `api METHOD PATH BODY` (http) or `do METHOD PATH BODY`
/// (an object).
pub fn run(req: &str) -> String {
    let mut sets = vec![];
    let answer = run_in(req, &mut sets);
    tell_sets(&sets);
    answer
}

/// One request; a chunk's changes go on `sets` (box, on), for the watchers
/// once the request, or the whole batch, is done.
fn run_in(req: &str, sets: &mut Vec<(usize, bool)>) -> String {
    let mut parts = req.splitn(4, ' ');
    let (_, method, path, body) = (parts.next(), parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match PROCESS.with(|p| p.get_or_init(|| env("COMBD_PROCESS").unwrap_or_default()).clone()).as_str() {
        "web" => match (method, path.split('?').next().unwrap_or("")) {
            ("GET", "/") => format!("200\ncontent-type: text/html; charset=utf-8\ncache-control: no-cache\n\n{INDEX}"),
            ("GET", "/api/count") => {
                let r = ask(&json!({ "to": "boxes", "id": "all", "method": "GET", "path": "/total" }));
                format!("{} {}", r["status"].as_u64().unwrap_or(503), r["body"].as_str().unwrap_or("{}"))
            }
            _ => err(404, "not found"),
        },
        "chunk" => chunk(method, path, &v, sets),
        "boxes" => boxes(method, path, &v),
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
    /// When it last dropped expired watchers: not on every request, since
    /// with 100,000 watching, a scan per request was the whole node's time.
    pruned: i64,
    /// Every box, as a new watcher gets it: built again only after a change.
    all: Option<String>,
    /// Changes since the last beat, for the watchers at the next one.
    pending: Vec<(usize, bool)>,
    /// The beat is on (`BEAT_MS`), and when the total last heard.
    beat: bool,
    told: i64,
}

/// The watchers hear a chunk's changes on a beat: one message each per beat,
/// with every change since the last, however many clicks came. So a node's
/// sends grow with its watchers, not with their clicks.
const BEAT_MS: u64 = 150;
/// The total hears from a chunk at most this often.
const TOLD_MS: i64 = 1000;

/// The beat, on (if it isn't: setting it again would put it off).
fn arm(c: &mut Chunk) {
    if !c.beat {
        c.beat = true;
        unsafe { tick_every(BEAT_MS) };
    }
}

thread_local! {
    static CH: RefCell<Option<Chunk>> = const { RefCell::new(None) };
    /// Which process this is (`COMBD_PROCESS`), read once.
    static PROCESS: std::cell::OnceCell<String> = const { std::cell::OnceCell::new() };
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
    let mut c = Chunk { n: 0, bits, checked: rows.len() as u64, subs: HashMap::new(), dirty, pruned: 0, all: None, pending: vec![], beat: false, told: 0 };
    if dirty {
        arm(&mut c);
    }
    c
}

fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

/// The watchers hear a beat's changes: one message for one, and for more,
/// one with them all (`sets`).
fn tell_sets(sets: &[(usize, bool)]) {
    if sets.is_empty() {
        return;
    }
    CH.with(|cell| {
        let cell = cell.borrow();
        let Some(c) = cell.as_ref() else { return };
        let msg = match sets {
            [(i, on)] => json!({ "t": "set", "n": c.n, "i": i, "v": on }),
            _ => json!({ "t": "sets", "n": c.n, "c": sets.iter().map(|&(i, on)| json!([i, on])).collect::<Vec<_>>() }),
        };
        push(c.subs.keys().copied(), &msg.to_string());
    });
}

fn chunk(method: &str, path: &str, v: &Value, sets: &mut Vec<(usize, bool)>) -> String {
    CH.with(|cell| {
        let mut cell = cell.borrow_mut();
        let c = cell.get_or_insert_with(load);
        let now = unsafe { now_ms() };
        if now - c.pruned >= PRUNE_MS {
            c.subs.retain(|_, until| *until > now);
            c.pruned = now;
        }
        if let Some(n) = v["n"].as_u64() {
            c.n = n;
        }
        match (method, path) {
            // Watch (or keep watching): every box first, then each change.
            ("POST", "/sub") => {
                let Some(sock) = v["sock"].as_u64() else { return err(400, "sock") };
                if c.subs.insert(sock, now + SUB_MS).is_none() {
                    let (n, bits) = (c.n, &c.bits);
                    let all = c.all.get_or_insert_with(|| json!({ "t": "all", "n": n, "bits": hex(bits) }).to_string());
                    push(std::iter::once(sock), all);
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
                c.all = None;
                if on {
                    query("INSERT INTO on_ (i) VALUES (?)", &[json!(i)]);
                    c.checked += 1;
                } else {
                    query("DELETE FROM on_ WHERE i = ?", &[json!(i)]);
                    c.checked -= 1;
                }
                // To the watchers at the next beat (and out only once the
                // write is on disk: the host holds what a beat sends until then).
                c.pending.push((i, on));
                c.dirty = true;
                arm(c);
            }
            ("TICK", _) => {
                sets.append(&mut c.pending);
                if c.dirty && now - c.told >= TOLD_MS {
                    tell(&json!({ "to": "boxes", "id": "all", "path": "/count", "body": json!({ "n": c.n, "checked": c.checked }).to_string() }));
                    (c.dirty, c.told) = (false, now);
                }
                if !c.dirty {
                    c.beat = false;
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
/// Requests that waited, all at once (each its length, a newline, then
/// itself): one transaction, an answer each, framed the same way, and the
/// watchers hear every change in one message.
#[no_mangle]
pub extern "C" fn handle_batch(ptr: *mut u8, len: u32) -> u64 {
    let reqs = unsafe { Vec::from_raw_parts(ptr, len as usize, len as usize) };
    let mut rest = core::str::from_utf8(&reqs).unwrap_or("");
    let (mut out, mut sets) = (String::new(), vec![]);
    while let Some((n, after)) = rest.split_once('\n') {
        let (req, more) = after.split_at(n.parse::<usize>().unwrap_or(after.len()).min(after.len()));
        let answer = run_in(req, &mut sets);
        out.push_str(&answer.len().to_string());
        out.push('\n');
        out.push_str(&answer);
        rest = more;
    }
    tell_sets(&sets);
    let out = Box::into_raw(out.into_bytes().into_boxed_slice());
    let len = out.len() as u64;
    ((out as *mut u8 as u64) << 32) | len
}

#[no_mangle]
pub extern "C" fn handle(ptr: *mut u8, len: u32) -> u64 {
    let req = unsafe { Vec::from_raw_parts(ptr, len as usize, len as usize) };
    let answer = run(core::str::from_utf8(&req).unwrap_or(""));
    let out = Box::into_raw(answer.into_bytes().into_boxed_slice());
    let len = out.len() as u64;
    ((out as *mut u8 as u64) << 32) | len
}
