//! One Million Checkboxes: the viewer, a `ws` process. One per WebSocket,
//! started when it connects. It keeps what its page is looking at (two or
//! so chunks), subscribes to those chunks, and passes the page's sets on.
//! It never sees the chunks' changes: they go straight to its socket. Idle,
//! it hibernates, and all that's left of it is `State`.

use serde_json::{json, Value};
use std::cell::RefCell;

/// Boxes in a chunk: 64 chunks make a million.
const CHUNK: u64 = 15_625;
const CHUNKS: u64 = 64;
/// The most chunks one page may watch at once.
const MAX_WATCH: u64 = 4;
/// Sets a page may make a second.
const MAX_SETS_PER_S: u32 = 20;

#[link(wasm_import_module = "combd")]
extern "C" {
    fn now_ms() -> i64;
    fn ws_send(id: u64, ptr: *const u8, len: u32) -> u32;
    fn ws_msg(ptr: *mut u8) -> u32;
    fn send(ptr: *const u8, len: u32) -> u32;
}

/// All a viewer is: its socket, the chunks it watches, and its rate.
#[derive(Default)]
struct State {
    sock: u64,
    /// Chunks `lo..=hi`; none while `lo > hi`.
    lo: u64,
    hi: u64,
    /// The second it's in, and its sets in it.
    sec: i64,
    sets: u32,
}

thread_local! {
    static ME: RefCell<State> = RefCell::new(State { lo: 1, ..State::default() });
}

fn to_chunk(n: u64, path: &str, body: Value) {
    let m = json!({ "to": "chunk", "id": n.to_string(), "path": path, "body": body.to_string() }).to_string();
    unsafe { send(m.as_ptr(), m.len() as u32) };
}

fn watching(s: &State) -> std::ops::RangeInclusive<u64> {
    s.lo..=s.hi
}

pub fn run(req: &str) -> String {
    let mut parts = req.splitn(4, ' ');
    let (method, body) = (parts.nth(1).unwrap_or(""), parts.nth(1).unwrap_or(""));
    ME.with(|me| {
        let mut s = me.borrow_mut();
        match method {
            // `do WSOPEN /ws <socket id>`
            "WSOPEN" => {
                s.sock = body.trim().parse().unwrap_or(0);
                "101 {}".into()
            }
            "WSMSG" => {
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                match v["t"].as_str() {
                    // What the page shows, boxes [from, to). Sent again every
                    // 20 s: that renews the subscriptions.
                    Some("range") => {
                        let (from, to) = (v["from"].as_u64().unwrap_or(0), v["to"].as_u64().unwrap_or(0));
                        let (lo, hi) = (from / CHUNK, (to.max(from + 1) - 1) / CHUNK);
                        let (lo, hi) = (lo.min(CHUNKS - 1), hi.min(CHUNKS - 1).min(lo + MAX_WATCH - 1));
                        let sock = s.sock;
                        for n in watching(&s).filter(|n| !(lo..=hi).contains(n)) {
                            to_chunk(n, "/unsub", json!({ "sock": sock }));
                        }
                        for n in lo..=hi {
                            to_chunk(n, "/sub", json!({ "sock": sock, "n": n }));
                        }
                        (s.lo, s.hi) = (lo, hi);
                    }
                    Some("set") => {
                        let (Some(i), Some(on)) = (v["i"].as_u64().filter(|&i| i < CHUNK * CHUNKS), v["v"].as_bool()) else {
                            return "200 {}".into();
                        };
                        let now = unsafe { now_ms() } / 1000;
                        if s.sec != now {
                            (s.sec, s.sets) = (now, 0);
                        }
                        s.sets += 1;
                        if s.sets > MAX_SETS_PER_S {
                            let m = json!({ "t": "slow", "i": i }).to_string();
                            unsafe { ws_send(s.sock, m.as_ptr(), m.len() as u32) };
                        } else {
                            to_chunk(i / CHUNK, "/set", json!({ "n": i / CHUNK, "i": i % CHUNK, "v": on }));
                        }
                    }
                    _ => {}
                }
                "200 {}".into()
            }
            "WSCLOSE" => {
                let sock = s.sock;
                for n in watching(&s) {
                    to_chunk(n, "/unsub", json!({ "sock": sock }));
                }
                "200 {}".into()
            }
            // Asleep: this is all that's kept.
            "HIBERNATE" => format!("200 {}", json!([s.sock, s.lo, s.hi, s.sec, s.sets])),
            // Awake again, with what it kept (`do WAKE / <len>`).
            "WAKE" => {
                let mut buf = vec![0u8; body.trim().parse().unwrap_or(0)];
                unsafe { ws_msg(buf.as_mut_ptr()) };
                if let Ok([sock, lo, hi, sec, sets]) = serde_json::from_slice::<[i64; 5]>(&buf) {
                    *s = State { sock: sock as u64, lo: lo as u64, hi: hi as u64, sec, sets: sets as u32 };
                }
                "200 {}".into()
            }
            _ => "404 {}".into(),
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
