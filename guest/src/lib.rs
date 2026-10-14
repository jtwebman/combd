//! The phase 0 test cell. The same workloads also run natively as the baseline.

pub fn fib(n: u32) -> u64 {
    if n < 2 {
        n as u64
    } else {
        fib(n - 1) + fib(n - 2)
    }
}

pub fn sieve(n: usize) -> usize {
    let mut is_prime = vec![true; n + 1];
    let mut count = 0;
    for i in 2..=n {
        if is_prime[i] {
            count += 1;
            if i > n / i {
                continue; // i * i would overflow a 32-bit usize on wasm32
            }
            let mut j = i * i;
            while j <= n {
                is_prime[j] = false;
                j += i;
            }
        }
    }
    count
}

pub fn sort(n: usize) -> u64 {
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut v: Vec<u64> = (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        })
        .collect();
    v.sort_unstable();
    v.iter().step_by(97).fold(0, |a, &b| a.wrapping_mul(31).wrapping_add(b))
}

/// The same Hono-style app as `langs/js/app.js`, written the way a Rust
/// developer would: a router on path segments and serde for JSON. Answers are
/// byte-for-byte the same as the JS version.
pub mod api {
    use serde::{Deserialize, Serialize};
    use std::sync::OnceLock;

    #[derive(Serialize)]
    struct User {
        id: u32,
        name: String,
        email: String,
        tags: &'static [&'static str],
        score: u32,
    }

    #[derive(Serialize)]
    struct UserOut<'a> {
        #[serde(flatten)]
        user: &'a User,
        greeting: String,
    }

    #[derive(Serialize)]
    struct Brief<'a> {
        id: u32,
        name: &'a str,
        score: u32,
    }

    #[derive(Serialize)]
    struct List<'a> {
        count: usize,
        users: Vec<Brief<'a>>,
    }

    #[derive(Deserialize)]
    struct Item {
        sku: String,
        price: f64,
        qty: f64,
    }

    #[derive(Deserialize)]
    struct Order {
        id: String,
        items: Vec<Item>,
    }

    /// JSON numbers the way JavaScript prints them: no ".0" on whole numbers.
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Num {
        Int(i64),
        Float(f64),
    }

    fn num(x: f64) -> Num {
        if x.fract() == 0.0 && x.abs() < 9e15 { Num::Int(x as i64) } else { Num::Float(x) }
    }

    #[derive(Serialize)]
    struct Receipt<'a> {
        id: &'a str,
        total: Num,
        count: usize,
        skus: Vec<String>,
    }

    const TAGS: [&str; 3] = ["alpha", "beta", "gamma"];

    fn users() -> &'static [User] {
        static USERS: OnceLock<Vec<User>> = OnceLock::new();
        USERS.get_or_init(|| {
            (0..100)
                .map(|i| User {
                    id: i,
                    name: format!("user{i}"),
                    email: format!("user{i}@example.com"),
                    tags: &TAGS[..(i as usize % 3) + 1],
                    score: (i * 7) % 13,
                })
                .collect()
        })
    }

    /// Build long-lived data before any request arena is active.
    pub fn warm() {
        users();
    }

    pub fn handle(method: &str, path: &str, body: &str) -> String {
        let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method, seg.as_slice()) {
            ("GET", ["api", "users", id]) => {
                let u = &users()[id.parse::<usize>().unwrap_or(0) % 100];
                let out = UserOut { user: u, greeting: format!("Hello, {}!", u.name) };
                format!("200 {}", serde_json::to_string(&out).unwrap())
            }
            ("GET", ["api", "users"]) => {
                let list = List {
                    count: users().len(),
                    users: users()[..20].iter().map(|u| Brief { id: u.id, name: &u.name, score: u.score }).collect(),
                };
                format!("200 {}", serde_json::to_string(&list).unwrap())
            }
            ("POST", ["api", "orders"]) => {
                let Ok(o) = serde_json::from_str::<Order>(body) else { return "400 {\"error\":\"bad json\"}".into() };
                let total: f64 = o.items.iter().map(|it| it.price * it.qty).sum();
                let r = Receipt {
                    id: &o.id,
                    total: num((total * 100.0).round() / 100.0),
                    count: o.items.len(),
                    skus: o.items.iter().map(|it| it.sku.to_uppercase()).collect(),
                };
                format!("201 {}", serde_json::to_string(&r).unwrap())
            }
            // CPU-heavy on purpose: the bad neighbor in fairness tests.
            ("GET", ["api", "fib", n]) => {
                let n: u32 = n.parse::<u32>().unwrap_or(0).min(40);
                format!("200 {{\"n\":{n},\"fib\":{}}}", super::fib(n))
            }
            _ => "404 {\"error\":\"not found\"}".into(),
        }
    }
}

/// A Durable Object: a counter and a chat room, with state kept in memory
/// between requests and saved in the object's SQLite storage.
///
/// The host wraps each request in a transaction and commits it before the
/// answer leaves. If this cell crashes, the host rolls the request back and
/// starts a fresh cell, and the state is loaded again from storage.
pub mod room {
    use serde_json::{json, Value};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    const KEEP: usize = 20;

    struct Room {
        count: i64,
        msgs: VecDeque<String>,
    }

    thread_local! {
        static ROOM: RefCell<Option<Room>> = const { RefCell::new(None) };
    }

    #[cfg(target_arch = "wasm32")]
    fn raw_sql(req: &[u8]) -> Vec<u8> {
        #[link(wasm_import_module = "combd")]
        extern "C" {
            fn sql(ptr: *const u8, len: u32) -> u32;
            fn sql_read(ptr: *mut u8);
        }
        let n = unsafe { sql(req.as_ptr(), req.len() as u32) } as usize;
        let mut out = vec![0u8; n];
        unsafe { sql_read(out.as_mut_ptr()) };
        out
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn raw_sql(_: &[u8]) -> Vec<u8> {
        br#"{"error":"storage needs a cell"}"#.to_vec()
    }

    #[cfg(target_arch = "wasm32")]
    mod host {
        #[link(wasm_import_module = "combd")]
        extern "C" {
            pub fn now_ms() -> i64;
            pub fn alarm_set(at_ms: i64);
            pub fn alarm_get() -> i64;
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod host {
        pub unsafe fn now_ms() -> i64 {
            0
        }
        pub unsafe fn alarm_set(_: i64) {}
        pub unsafe fn alarm_get() -> i64 {
            -1
        }
    }

    fn kv(k: &str) -> i64 {
        sql("SELECT v FROM kv WHERE k = ?", &[json!(k)]).first().and_then(|r| r[0].as_i64()).unwrap_or(0)
    }

    fn set_kv(k: &str, v: i64) {
        sql("INSERT INTO kv (k, v) VALUES (?, ?) ON CONFLICT (k) DO UPDATE SET v = excluded.v", &[json!(k), json!(v)]);
    }

    /// One statement. Returns the rows, or panics on an error: let it crash.
    fn sql(q: &str, p: &[Value]) -> Vec<Value> {
        let out: Value = serde_json::from_slice(&raw_sql(json!({ "q": q, "p": p }).to_string().as_bytes())).expect("bad sql answer");
        if let Some(e) = out.get("error") {
            panic!("sql: {e}");
        }
        out["rows"].as_array().cloned().unwrap_or_default()
    }

    fn load() -> Room {
        sql("CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v INTEGER)", &[]);
        sql("CREATE TABLE IF NOT EXISTS msgs (id INTEGER PRIMARY KEY, body TEXT)", &[]);
        let count = sql("SELECT v FROM kv WHERE k = 'count'", &[]).first().and_then(|r| r[0].as_i64()).unwrap_or(0);
        let mut msgs: VecDeque<String> = sql("SELECT body FROM msgs ORDER BY id DESC LIMIT ?", &[json!(KEEP)])
            .iter()
            .filter_map(|r| r[0].as_str().map(String::from))
            .collect();
        msgs.make_contiguous().reverse();
        Room { count, msgs }
    }

    pub fn handle(method: &str, path: &str, body: &str) -> String {
        ROOM.with(|r| {
            let mut r = r.borrow_mut();
            let room = r.get_or_insert_with(load);
            match (method, path) {
                ("GET", "/count") => format!("200 {}", json!({ "count": room.count })),
                ("POST", "/incr") => {
                    room.count += 1;
                    sql("INSERT INTO kv (k, v) VALUES ('count', ?) ON CONFLICT (k) DO UPDATE SET v = excluded.v", &[json!(room.count)]);
                    format!("200 {}", json!({ "count": room.count }))
                }
                ("GET", "/msgs") => format!("200 {}", json!({ "msgs": room.msgs })),
                ("POST", "/msg") => {
                    let id = sql("INSERT INTO msgs (body) VALUES (?) RETURNING id", &[json!(body)])[0][0].clone();
                    room.msgs.push_back(body.to_string());
                    if room.msgs.len() > KEEP {
                        room.msgs.pop_front();
                    }
                    format!("201 {}", json!({ "id": id }))
                }
                // Alarms: run `ALARM` in `body` ms. `/alarm-crash`'s handler
                // crashes on its first try, so the host must retry it.
                ("POST", "/alarm") | ("POST", "/alarm-crash") => {
                    let ms: i64 = body.trim().parse().unwrap_or(0);
                    set_kv("crash_alarm", (path == "/alarm-crash") as i64);
                    let at = unsafe { host::now_ms() } + ms;
                    unsafe { host::alarm_set(at) };
                    format!("200 {}", json!({ "at": at }))
                }
                ("GET", "/alarms") => format!("200 {}", json!({ "fired": kv("alarms"), "pending": unsafe { host::alarm_get() } > 0 })),
                ("ALARM", _) => {
                    let retries: i64 = body.trim().parse().unwrap_or(0);
                    if kv("crash_alarm") == 1 && retries == 0 {
                        panic!("alarm crash on the first try");
                    }
                    set_kv("crash_alarm", 0);
                    set_kv("alarms", kv("alarms") + 1);
                    "200 {}".into()
                }
                // Write, then crash: the host must roll the write back.
                ("POST", "/crash") => {
                    sql("INSERT INTO msgs (body) VALUES ('never saved')", &[]);
                    panic!("crash after a write");
                }
                _ => "404 {\"error\":\"not found\"}".into(),
            }
        })
    }
}

/// One request: "hello", "print", "spin", "panic", "fib N", "sieve N", "sort N",
/// "api METHOD PATH BODY", or "do METHOD PATH BODY" (a Durable Object).
pub fn run(req: &str, counter: impl FnOnce() -> i64) -> String {
    let (cmd, arg) = req.split_once(' ').unwrap_or((req, "0"));
    let n: u64 = arg.trim().parse().unwrap_or(0);
    match cmd {
        "hello" => format!("hello #{}", counter()),
        "spin" => {
            let mut x = 0u64;
            loop {
                x = core::hint::black_box(x.wrapping_add(1));
            }
        }
        "panic" => panic!("boom"),
        // A read far past the cell's memory. Every engine must trap.
        #[cfg(target_arch = "wasm32")]
        "oob" => unsafe { core::ptr::read_volatile(0xFFFF_FFF0usize as *const u8) }.to_string(),
        // wasm32-unknown-unknown has no stdout, so "print" just answers.
        "print" => "printed".into(),
        "fib" => fib(n as u32).to_string(),
        "sieve" => sieve(n as usize).to_string(),
        "sort" => sort(n as usize).to_string(),
        "api" => {
            let mut parts = arg.splitn(3, ' ');
            let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            api::handle(method, path, parts.next().unwrap_or(""))
        }
        "do" => {
            let mut parts = arg.splitn(3, ' ');
            let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            room::handle(method, path, parts.next().unwrap_or(""))
        }
        _ => "unknown".into(),
    }
}

/// A per-request arena, as the combd Rust SDK could offer it. Inside
/// `scope`, every allocation is a pointer bump in one region and every free is
/// free. The region resets when the scope ends. Anything that must outlive the
/// request has to be copied out, and statics must be warmed outside a scope.
/// A mistake here can only hurt this cell's own (sandboxed) memory.
#[cfg(all(target_arch = "wasm32", feature = "arena"))]
mod arena {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    const SIZE: usize = 4 << 20;

    pub struct Arena {
        on: Cell<bool>,
        base: Cell<usize>,
        next: Cell<usize>,
    }

    // A Wasm cell runs on one thread.
    unsafe impl Sync for Arena {}

    impl Arena {
        fn owns(&self, p: *mut u8) -> bool {
            let (p, b) = (p as usize, self.base.get());
            b != 0 && p >= b && p < b + SIZE
        }
    }

    unsafe impl GlobalAlloc for Arena {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            if self.on.get() {
                let p = (self.next.get() + l.align() - 1) & !(l.align() - 1);
                if p + l.size() <= self.base.get() + SIZE {
                    self.next.set(p + l.size());
                    return p as *mut u8;
                }
            }
            System.alloc(l)
        }

        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            if !self.owns(p) {
                System.dealloc(p, l);
            }
        }

        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            // The newest allocation can grow in place.
            if self.on.get() && p as usize + l.size() == self.next.get() && p as usize + new <= self.base.get() + SIZE {
                self.next.set(p as usize + new);
                return p;
            }
            let q = self.alloc(Layout::from_size_align_unchecked(new, l.align()));
            if !q.is_null() {
                std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                self.dealloc(p, l);
            }
            q
        }
    }

    #[global_allocator]
    pub static A: Arena = Arena { on: Cell::new(false), base: Cell::new(0), next: Cell::new(0) };

    pub fn scope<R>(f: impl FnOnce() -> R) -> R {
        if A.base.get() == 0 {
            let b = unsafe { System.alloc(Layout::from_size_align(SIZE, 16).unwrap()) } as usize;
            A.base.set(b);
            A.next.set(b);
        }
        A.on.set(A.base.get() != 0);
        let r = f();
        A.on.set(false);
        A.next.set(A.base.get());
        r
    }
}

#[cfg(not(all(target_arch = "wasm32", feature = "arena")))]
mod arena {
    #[allow(dead_code)] // only the Wasm build calls it
    pub fn scope<R>(f: impl FnOnce() -> R) -> R {
        f()
    }
}

#[cfg(target_arch = "wasm32")]
mod cell {
    #[link(wasm_import_module = "combd")]
    extern "C" {
        fn counter_incr() -> i64;
    }

    #[no_mangle]
    pub extern "C" fn alloc(len: u32) -> *mut u8 {
        let mut v = Vec::<u8>::with_capacity(len as usize);
        let p = v.as_mut_ptr();
        core::mem::forget(v);
        p
    }

    /// The host calls this after reading an answer, so the cell can free it.
    #[no_mangle]
    pub extern "C" fn release(ptr: *mut u8, len: u32) {
        drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr, len as usize)) });
    }

    /// Returns the response as (ptr << 32) | len. The host frees it with `release`.
    #[no_mangle]
    pub extern "C" fn handle(ptr: *mut u8, len: u32) -> u64 {
        let req = unsafe { Vec::from_raw_parts(ptr, len as usize, len as usize) };
        super::api::warm(); // statics live outside the request arena
        // Everything the request allocates dies with it, except the answer,
        // which is copied out before the arena resets.
        let req = core::str::from_utf8(&req).unwrap_or("");
        let run = || super::run(req, || unsafe { counter_incr() });
        // A Durable Object keeps state between requests, so its requests
        // can't use the request arena: that state would be freed with it.
        let answer = if req.starts_with("do ") { run() } else { super::arena::scope(run) };
        // The arena is off now and nothing has reused it yet: copy the answer out.
        let out: Box<[u8]> = answer.as_bytes().into();
        drop(answer);
        let out = Box::into_raw(out);
        let len = out.len() as u64;
        ((out as *mut u8 as u64) << 32) | len
    }
}
