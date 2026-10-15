//! An event cell: one instance serves many requests at once, like a Node
//! process. The host calls `on_request` for each new request and `on_event`
//! when something the cell waited on is done. Each wait is a ticket, and the
//! cell keeps its own table of what to do next: a tiny event loop.
//!
//! `respond` answers a request. Work may go on after it (waitUntil), but that
//! work is best effort: if the instance crashes, it's gone and nothing retries.

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;

#[link(wasm_import_module = "combd")]
extern "C" {
    fn respond(req: u64, ptr: *const u8, len: u32) -> u32;
    fn timer_async(req: u64, ms: u64) -> u64;
    fn pg_open_async(req: u64, ptr: *const u8, len: u32) -> u64;
    fn pg_query_async(req: u64, handle: u64, ptr: *const u8, len: u32) -> u64;
    fn pg_close_async(req: u64, handle: u64) -> u32;
    fn ev_read(ptr: *mut u8, cap: u32) -> u32;
    fn call_async(req: u64, ptr: *const u8, len: u32) -> u64;
    fn send_async(req: u64, ptr: *const u8, len: u32) -> u32;
}

// The app's env vars, the way any WASI program gets them.
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn environ_sizes_get(count: *mut u32, size: *mut u32) -> u32;
    fn environ_get(ptrs: *mut *mut u8, buf: *mut u8) -> u32;
}

fn env_var(name: &str) -> Option<String> {
    let (mut n, mut size) = (0u32, 0u32);
    unsafe { environ_sizes_get(&mut n, &mut size) };
    let mut ptrs = vec![core::ptr::null_mut::<u8>(); n as usize];
    let mut buf = vec![0u8; size as usize];
    unsafe { environ_get(ptrs.as_mut_ptr(), buf.as_mut_ptr()) };
    let want = format!("{name}=");
    buf.split(|&b| b == 0).filter_map(|e| core::str::from_utf8(e).ok()).find_map(|e| e.strip_prefix(want.as_str()).map(String::from))
}

type Then = Box<dyn FnOnce(Vec<u8>)>;

thread_local! {
    /// ticket -> what to do when it's back.
    static WAITING: RefCell<HashMap<u64, Then>> = RefCell::new(HashMap::new());
    /// State every request on this instance shares.
    static HITS: RefCell<u64> = const { RefCell::new(0) };
    static LATER: RefCell<u64> = const { RefCell::new(0) };
}

fn wait(ticket: u64, then: impl FnOnce(Vec<u8>) + 'static) {
    if ticket != 0 {
        WAITING.with(|w| w.borrow_mut().insert(ticket, Box::new(then)));
    }
}

fn answer(req: u64, a: &str) {
    unsafe { respond(req, a.as_ptr(), a.len() as u32) };
}

fn sleep(req: u64, ms: u64, then: impl FnOnce(Vec<u8>) + 'static) {
    wait(unsafe { timer_async(req, ms) }, then);
}

fn query(req: u64, h: u64, q: Value, then: impl FnOnce(Value) + 'static) {
    let s = q.to_string();
    wait(unsafe { pg_query_async(req, h, s.as_ptr(), s.len() as u32) }, move |b| then(serde_json::from_slice(&b).unwrap_or(Value::Null)));
}

/// Run statements one after another on handle `h`, then call `then` with the last answer.
fn chain(req: u64, h: u64, mut qs: Vec<Value>, then: impl FnOnce(Value) + 'static) {
    if qs.is_empty() {
        return then(Value::Null);
    }
    let q = qs.remove(0);
    query(req, h, q, move |a| if qs.is_empty() || a.get("error").is_some() { then(a) } else { chain(req, h, qs, then) });
}

#[no_mangle]
pub extern "C" fn alloc(len: u32) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len as usize);
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn on_event(ticket: u64, len: u32) {
    let mut buf = vec![0u8; len as usize];
    let n = unsafe { ev_read(buf.as_mut_ptr(), len) } as usize;
    buf.truncate(n);
    if let Some(then) = WAITING.with(|w| w.borrow_mut().remove(&ticket)) {
        then(buf);
    }
}

#[no_mangle]
pub extern "C" fn on_request(req: u64, ptr: *mut u8, len: u32) {
    let text = unsafe { Vec::from_raw_parts(ptr, len as usize, len as usize) };
    let text = String::from_utf8_lossy(&text).into_owned();
    // "api METHOD PATH BODY"
    let mut parts = text.splitn(4, ' ').skip(1);
    let (method, path, body) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("").to_string());
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    let num = |i: usize| seg.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    match (method, seg.as_slice()) {
        ("GET", ["hello"]) => {
            let n = HITS.with(|h| {
                *h.borrow_mut() += 1;
                *h.borrow()
            });
            answer(req, &format!("200 {}", json!({ "hits": n })));
        }
        ("GET", ["sleep", _]) => {
            let ms = num(1);
            sleep(req, ms, move |_| answer(req, &format!("200 {}", json!({ "slept": ms }))));
        }
        // Answer now, then keep working (waitUntil).
        ("GET", ["later", _]) => {
            answer(req, "200 {\"ok\":true}");
            sleep(req, num(1), |_| LATER.with(|l| *l.borrow_mut() += 1));
        }
        ("GET", ["env", name]) => match env_var(name) {
            Some(v) => answer(req, &format!("200 {}", json!({ "value": v }))),
            None => answer(req, "404 {\"error\":\"not set\"}"),
        },
        ("GET", ["later-count"]) => answer(req, &format!("200 {}", json!({ "later": LATER.with(|l| *l.borrow()) }))),
        // Crash after `ms`: every unanswered request on this instance fails.
        ("GET", ["crash", _]) => sleep(req, num(1), |_| core::arch::wasm32::unreachable()),
        // One pooled statement: body is {"q", "p"}.
        // Another process: the body is the call, the answer is its result.
        ("POST", ["call"]) => wait(unsafe { call_async(req, body.as_ptr(), body.len() as u32) }, move |b| answer(req, &format!("200 {}", String::from_utf8_lossy(&b)))),
        ("POST", ["send"]) => {
            let ok = unsafe { send_async(req, body.as_ptr(), body.len() as u32) } == 1;
            answer(req, if ok { "202 {}" } else { "507 {}" });
        }
        ("POST", ["pg", "q"]) => {
            let mut q: Value = serde_json::from_str(&body).unwrap_or(json!({ "q": body }));
            q["db"] = json!("main");
            query(req, 0, q, move |a| answer(req, &format!("{} {a}", if a.get("error").is_some() { 400 } else { 200 })));
        }
        // A transaction over a pinned connection: body is a list of {"q","p"},
        // run between BEGIN and COMMIT, with a pause in the middle so other
        // requests interleave. A failure rolls back (closing the handle does).
        ("POST", ["pg", "tx", _]) => {
            let pause = num(2);
            let mut qs: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
            let open = json!({ "db": "main" }).to_string();
            wait(unsafe { pg_open_async(req, open.as_ptr(), open.len() as u32) }, move |b| {
                let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                let Some(h) = v["handle"].as_u64() else { return answer(req, &format!("503 {v}")) };
                let rest = qs.split_off(qs.len() / 2);
                chain(req, h, [vec![json!({ "q": "BEGIN" })], qs].concat(), move |a| {
                    if a.get("error").is_some() {
                        unsafe { pg_close_async(req, h) };
                        return answer(req, &format!("400 {a}"));
                    }
                    sleep(req, pause, move |_| {
                        chain(req, h, [rest, vec![json!({ "q": "COMMIT" })]].concat(), move |a| {
                            unsafe { pg_close_async(req, h) };
                            answer(req, &format!("{} {a}", if a.get("error").is_some() { 400 } else { 200 }));
                        })
                    })
                })
            });
        }
        _ => answer(req, "404 {\"error\":\"not found\"}"),
    }
}
