//! One Million Checkboxes, a combd example. One module, two processes (it
//! asks `COMBD_PROCESS` which one it is):
//! - `web`, a worker: the page;
//! - `chunk`, durable (one per 15,625 boxes, 64 in all): its boxes' bits,
//!   in memory and in its SQLite, and the WebSockets of everyone looking at
//!   it. A box set over one socket goes out to all of them.

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;

const INDEX: &str = include_str!("../../client/index.html");

/// Boxes in a chunk: 64 chunks make a million.
const CHUNK: usize = 15_625;

/// Sets a socket may make a second. A socket, not a person: anyone can open
/// more. ponytail: per socket; per client address if bots get bad.
const MAX_SETS_PER_S: u32 = 20;

#[link(wasm_import_module = "combd")]
extern "C" {
    fn now_ms() -> i64;
    fn ws_send(id: u64, ptr: *const u8, len: u32) -> u32;
    fn ws_list(ptr: *mut u64, cap: u32) -> u32;
    fn ws_close(id: u64, code: u32) -> u32;
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

fn send(sock: u64, msg: &str) {
    unsafe { ws_send(sock, msg.as_ptr(), msg.len() as u32) };
}

/// The object's open sockets, on every node.
fn sockets() -> Vec<u64> {
    let mut ids = vec![0u64; 256];
    let n = unsafe { ws_list(ids.as_mut_ptr(), ids.len() as u32) } as usize;
    if n > ids.len() {
        ids = vec![0u64; n];
        unsafe { ws_list(ids.as_mut_ptr(), n as u32) };
    }
    ids.truncate(n);
    ids
}

fn err(code: u16, msg: &str) -> String {
    format!("{code} {}", json!({ "error": msg }))
}

/// One request: `api METHOD PATH BODY` (a worker) or `do METHOD PATH BODY`
/// (an object).
pub fn run(req: &str) -> String {
    let mut parts = req.splitn(4, ' ');
    let (_, method, path, body) = (parts.next(), parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    match env("COMBD_PROCESS").as_deref() {
        Some("web") => match (method, path.split('?').next().unwrap_or("")) {
            ("GET", "/") => format!("200\ncontent-type: text/html; charset=utf-8\ncache-control: no-cache\n\n{INDEX}"),
            _ => err(404, "not found"),
        },
        Some("chunk") => chunk(method, path, body),
        other => err(500, &format!("million: no process {other:?}")),
    }
}

struct Chunk {
    bits: Vec<u8>,
    /// Per socket: the second it's in, and its sets in it.
    rate: HashMap<u64, (i64, u32)>,
}

thread_local! {
    static CH: RefCell<Option<Chunk>> = const { RefCell::new(None) };
}

fn load() -> Chunk {
    query("CREATE TABLE IF NOT EXISTS on_ (i INTEGER PRIMARY KEY)", &[]);
    let mut bits = vec![0u8; CHUNK.div_ceil(8)];
    for r in query("SELECT i FROM on_", &[]) {
        let i = r[0].as_u64().unwrap_or(0) as usize;
        bits[i / 8] |= 1 << (i % 8);
    }
    Chunk { bits, rate: HashMap::new() }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn chunk(method: &str, path: &str, body: &str) -> String {
    CH.with(|cell| {
        let mut cell = cell.borrow_mut();
        let c = cell.get_or_insert_with(load);
        let sock: u64 = path.trim_start_matches('/').parse().unwrap_or(0);
        match method {
            "WSOPEN" => "101 {}".into(),
            "WSCLOSE" => {
                c.rate.remove(&sock);
                "200 {}".into()
            }
            "WSMSG" => {
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                match v["t"].as_str() {
                    // Every box in the chunk, once the socket is open.
                    Some("hi") => send(sock, &json!({ "t": "all", "bits": hex(&c.bits) }).to_string()),
                    // `v` is what the box should be, not a toggle: two people
                    // clicking at once both get what they clicked.
                    Some("set") => {
                        let (Some(i), Some(on)) = (v["i"].as_u64().map(|i| i as usize).filter(|&i| i < CHUNK), v["v"].as_bool()) else {
                            unsafe { ws_close(sock, 4000) };
                            return "200 {}".into();
                        };
                        let now = unsafe { now_ms() } / 1000;
                        let r = c.rate.entry(sock).or_insert((now, 0));
                        if r.0 != now {
                            *r = (now, 0);
                        }
                        r.1 += 1;
                        if r.1 > MAX_SETS_PER_S {
                            send(sock, &json!({ "t": "slow", "i": i, "v": c.bits[i / 8] & (1 << (i % 8)) != 0 }).to_string());
                            return "200 {}".into();
                        }
                        if (c.bits[i / 8] & (1 << (i % 8)) != 0) != on {
                            c.bits[i / 8] ^= 1 << (i % 8);
                            if on {
                                query("INSERT INTO on_ (i) VALUES (?)", &[json!(i)]);
                            } else {
                                query("DELETE FROM on_ WHERE i = ?", &[json!(i)]);
                            }
                        }
                        // To everyone, the clicker too (it confirms). The host
                        // sends them once the write is on disk.
                        // ponytail: one message per set; batch per beat if
                        // fan-out gets heavy.
                        let msg = json!({ "t": "set", "i": i, "v": on }).to_string();
                        for s in sockets() {
                            send(s, &msg);
                        }
                    }
                    _ => {}
                }
                "200 {}".into()
            }
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
