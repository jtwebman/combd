//! Write a combd app in Rust. A process gets a request and gives back a
//! response; a durable object also has its own SQLite database, which
//! commits with the request (an answer goes out only once its writes are
//! safe).
//!
//! ```ignore
//! use combd_sdk::{cell, sql, Request, Response};
//!
//! fn handle(req: Request) -> Response {
//!     match (req.method, req.path) {
//!         ("POST", "/incr") => {
//!             sql::run("CREATE TABLE IF NOT EXISTS n (v INTEGER)", &[]).unwrap();
//!             sql::run("INSERT INTO n VALUES (1)", &[]).unwrap();
//!             let rows = sql::run("SELECT count(*) FROM n", &[]).unwrap();
//!             Response::json(200, &combd_sdk::json!({ "count": rows[0][0] }))
//!         }
//!         _ => Response::text(404, "not found"),
//!     }
//! }
//!
//! cell!(handle);
//! ```
//!
//! Build it for `wasm32-unknown-unknown` (`combd build` does), and name its
//! processes in `combd.toml`.

pub use serde_json::{json, Value};

/// JSON text, written straight into a buffer: `Value::to_string()` goes
/// through `Display` (a `fmt` call and an io adapter per piece), several
/// times slower.
pub fn to_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// A request, as the host hands it over: `<kind> <METHOD> <path>[ <body>]`.
/// `kind` is `api` for a stateless process and `do` for a durable object
/// (then `path` is what's after the object's id in the route). Events come
/// the same way, with their own methods: `ALARM` (an alarm fired), `TICK`,
/// and for WebSockets `WSOPEN`, `WSMSG`, `WSCLOSE`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request<'a> {
    pub kind: &'a str,
    pub method: &'a str,
    /// With its query string, if any (`query` reads it).
    pub path: &'a str,
    pub body: &'a str,
}

impl<'a> Request<'a> {
    pub fn parse(line: &'a str) -> Request<'a> {
        let mut parts = line.splitn(4, ' ');
        let mut next = || parts.next().unwrap_or("");
        Request { kind: next(), method: next(), path: next(), body: next() }
    }

    /// The path without its query string.
    pub fn route(&self) -> &'a str {
        self.path.split('?').next().unwrap_or("")
    }

    /// A query parameter (`?name=value`), as written (not decoded).
    pub fn query(&self, name: &str) -> Option<&'a str> {
        let q = self.path.split_once('?')?.1;
        q.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| v))
    }

    /// The body as JSON (`Value::Null` if it isn't).
    pub fn json(&self) -> Value {
        if self.body.is_empty() {
            return Value::Null; // a GET's: no parse error built to say so
        }
        serde_json::from_str(self.body).unwrap_or(Value::Null)
    }
}

/// A response: a status, a few headers (the host passes on only safe
/// ones: content-type, cache-control, location, set-cookie, etag,
/// last-modified, vary, access-control-*), and a body.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn text(status: u16, body: &str) -> Response {
        Response { status, headers: vec![], body: body.into() }.header("content-type", "text/plain; charset=utf-8")
    }

    pub fn json(status: u16, v: &Value) -> Response {
        Response { status, headers: vec![], body: to_json(v) }
    }

    pub fn html(body: &str) -> Response {
        Response { status: 200, headers: vec![], body: body.into() }.header("content-type", "text/html; charset=utf-8")
    }

    pub fn header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// As the host reads it: `<status> <body>`, or with headers,
    /// `<status>\n<name>: <value>...\n\n<body>`.
    pub fn into_answer(self) -> String {
        if self.headers.is_empty() {
            return format!("{} {}", self.status, self.body);
        }
        let head: String = self.headers.iter().map(|(n, v)| format!("{n}: {v}\n")).collect();
        format!("{}\n{head}\n{}", self.status, self.body)
    }
}

/// Generates the exports the host calls (`alloc`, `release`, `handle`)
/// around your function, `fn(Request) -> Response` (any name: they're in a
/// module of their own).
#[macro_export]
macro_rules! cell {
    ($handler:ident) => {
        mod __combd_cell {
            #[no_mangle]
            pub extern "C" fn alloc(len: u32) -> *mut u8 {
                let mut v = ::std::vec::Vec::<u8>::with_capacity(len as usize);
                let p = v.as_mut_ptr();
                ::std::mem::forget(v);
                p
            }

            /// The host calls this once it has read an answer.
            #[no_mangle]
            pub extern "C" fn release(ptr: *mut u8, len: u32) {
                drop(unsafe { ::std::boxed::Box::from_raw(::std::ptr::slice_from_raw_parts_mut(ptr, len as usize)) });
            }

            /// The answer, as (ptr << 32) | len.
            #[no_mangle]
            pub extern "C" fn handle(ptr: *mut u8, len: u32) -> u64 {
                $crate::report_panics();
                let req = unsafe { ::std::vec::Vec::from_raw_parts(ptr, len as usize, len as usize) };
                let line = ::std::str::from_utf8(&req).unwrap_or("");
                let answer = super::$handler($crate::Request::parse(line)).into_answer();
                let out = ::std::boxed::Box::into_raw(answer.into_bytes().into_boxed_slice());
                ((out as *mut u8 as u64) << 32) | out.len() as u64
            }
        }
    };
}

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "combd")]
    extern "C" {
        pub fn sql(ptr: *const u8, len: u32) -> u32;
        pub fn sql_read(ptr: *mut u8);
        pub fn now_ms() -> i64;
        pub fn alarm_set(at_ms: i64);
        pub fn alarm_get() -> i64;
        pub fn call(ptr: *const u8, len: u32) -> u32;
        pub fn send(ptr: *const u8, len: u32) -> u32;
        pub fn pg_read(ptr: *mut u8, cap: u32) -> u32;
        pub fn pg_query(handle: u64, ptr: *const u8, len: u32) -> u32;
    }
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        pub fn environ_sizes_get(count: *mut u32, size: *mut u32) -> u32;
        pub fn environ_get(ptrs: *mut *mut u8, buf: *mut u8) -> u32;
        pub fn fd_write(fd: u32, iovs: *const [u32; 2], n: u32, written: *mut u32) -> u32;
    }
}

/// A panic's message goes to stderr, which combd puts in its log beside
/// the trap ("trapped: unreachable (panicked at src/lib.rs:12: ...)"):
/// with `panic = "abort"` a cell otherwise dies without a word. `cell!`
/// calls it; once is enough.
pub fn report_panics() {
    #[cfg(target_arch = "wasm32")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::panic::set_hook(Box::new(|info| {
                let msg = format!("{info}\n");
                let iov = [msg.as_ptr() as u32, msg.len() as u32];
                let mut written = 0u32;
                unsafe { host::fd_write(2, &iov, 1, &mut written) };
            }))
        });
    }
}

/// The object's own SQLite database (durable objects only). Each request
/// is one transaction: its writes commit together, or not at all.
pub mod sql {
    use super::Value;

    /// Run one statement, `?` for each of `params`; the rows it returns.
    pub fn run(q: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, String> {
        rows(&raw(&statement(None, q, params)))
    }

    /// An answer's rows, or its error (with its SQLSTATE first, if it has
    /// one: Postgres's do, SQLite's don't).
    pub(crate) fn rows(out: &[u8]) -> Result<Vec<Vec<Value>>, String> {
        let mut v: Value = serde_json::from_slice(out).map_err(|e| e.to_string())?;
        if let Some(e) = v.get("error") {
            let e = e.as_str().unwrap_or("sql error");
            return Err(v["code"].as_str().map_or(e.to_string(), |c| format!("{c} {e}")));
        }
        // Moved out, not copied.
        let rows = match v.get_mut("rows").map(Value::take) {
            Some(Value::Array(rows)) => rows,
            _ => return Ok(vec![]),
        };
        Ok(rows.into_iter().map(|row| if let Value::Array(r) = row { r } else { vec![] }).collect())
    }

    /// `{"db"?, "q", "p"}`, written out as it goes (no `Value` built first:
    /// that copies every parameter).
    pub(crate) fn statement(db: Option<&str>, q: &str, params: &[Value]) -> String {
        let s = |x: &str| serde_json::to_string(x).unwrap_or_default();
        let p = serde_json::to_string(params).unwrap_or_default();
        match db {
            Some(db) => format!("{{\"db\":{},\"q\":{},\"p\":{p}}}", s(db), s(q)),
            None => format!("{{\"q\":{},\"p\":{p}}}", s(q)),
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn raw(req: &str) -> Vec<u8> {
        let n = unsafe { super::host::sql(req.as_ptr(), req.len() as u32) } as usize;
        let mut out = vec![0u8; n];
        unsafe { super::host::sql_read(out.as_mut_ptr()) };
        out
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn raw(_: &str) -> Vec<u8> {
        unreachable!("combd-sdk calls the host: build for wasm32-unknown-unknown")
    }
}

/// Postgres, through a pool the host runs (`[pool.<name>]` in combd.toml,
/// with this process among its `callers`). The cell waits, paused, while
/// the statement runs; it never holds a socket or a password.
pub mod pg {
    use super::Value;

    /// One statement on a pooled connection of pool `db`, `$1`, `$2`... for
    /// `params`; the rows it returns. An error starts with its SQLSTATE
    /// (`23505 duplicate key value...`). Each statement is its own
    /// transaction. The connection isn't reset after it, as with most pools:
    /// what it changes in the session (a `SET`, a session advisory lock, a
    /// temp table) stays for whichever statement uses that connection next.
    /// Undo it yourself, or keep it to one request.
    pub fn query(db: &str, q: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, String> {
        super::sql::rows(&raw(&super::sql::statement(Some(db), q, params)))
    }

    #[cfg(target_arch = "wasm32")]
    fn raw(req: &str) -> Vec<u8> {
        let n = unsafe { super::host::pg_query(0, req.as_ptr(), req.len() as u32) } as usize;
        let mut out = vec![0u8; n];
        let got = unsafe { super::host::pg_read(out.as_mut_ptr(), n as u32) } as usize;
        out.truncate(got);
        out
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn raw(_: &str) -> Vec<u8> {
        unreachable!("combd-sdk calls the host: build for wasm32-unknown-unknown")
    }
}

/// An object's alarm: one at a time, in ms since the epoch. It commits with
/// the request, and fires as a request with the method `ALARM`.
pub mod alarm {
    #[cfg(target_arch = "wasm32")]
    pub fn set(at_ms: i64) {
        unsafe { super::host::alarm_set(at_ms) }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn clear() {
        unsafe { super::host::alarm_set(0) }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn get() -> Option<i64> {
        Some(unsafe { super::host::alarm_get() }).filter(|&t| t >= 0)
    }
}

/// The wall clock, in ms since the epoch (as coarse as the host's clock).
#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> i64 {
    unsafe { host::now_ms() }
}

/// Ask another process, and wait for its answer: `{"to": "<process>",
/// "id": "<object id, for a durable one>", "method", "path", "body"}` in,
/// `{"status", "body"}` back. Only processes whose `callers` include this
/// one answer.
#[cfg(target_arch = "wasm32")]
pub fn call(msg: &Value) -> Value {
    let m = to_json(msg);
    let n = unsafe { host::call(m.as_ptr(), m.len() as u32) } as usize;
    let mut out = vec![0u8; n];
    let got = unsafe { host::pg_read(out.as_mut_ptr(), n as u32) } as usize;
    out.truncate(got);
    serde_json::from_slice(&out).unwrap_or(Value::Null)
}

/// Tell another process, without waiting: it goes out only if this request
/// succeeds. False: too much queued already.
#[cfg(target_arch = "wasm32")]
pub fn send(msg: &Value) -> bool {
    let m = to_json(msg);
    unsafe { host::send(m.as_ptr(), m.len() as u32) == 1 }
}

/// An environment variable: the app's vars and secrets (`combd.toml`,
/// `combd vars`, `combd secret`), and `COMBD_PROCESS`, which process this is.
#[cfg(target_arch = "wasm32")]
pub fn env(name: &str) -> Option<String> {
    let (mut n, mut size) = (0u32, 0u32);
    unsafe { host::environ_sizes_get(&mut n, &mut size) };
    let mut ptrs = vec![core::ptr::null_mut::<u8>(); n as usize];
    let mut buf = vec![0u8; size as usize];
    unsafe { host::environ_get(ptrs.as_mut_ptr(), buf.as_mut_ptr()) };
    let want = format!("{name}=");
    buf.split(|&b| b == 0).filter_map(|e| core::str::from_utf8(e).ok()).find_map(|e| e.strip_prefix(want.as_str()).map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_answers_read_as_the_host_writes_them() {
        let r = Request::parse("do POST /incr?by=2 {\"a\":1}");
        assert_eq!((r.kind, r.method, r.route(), r.query("by")), ("do", "POST", "/incr", Some("2")));
        assert_eq!(r.json()["a"], 1);
        assert_eq!(Request::parse("api GET /").body, "");
        assert_eq!(Response::json(200, &json!({ "n": 1 })).into_answer(), "200 {\"n\":1}");
        assert_eq!(Response::html("<p>hi</p>").into_answer(), "200\ncontent-type: text/html; charset=utf-8\n\n<p>hi</p>");
    }

    #[test]
    fn answers_read_as_rows_or_errors() {
        assert_eq!(sql::rows(br#"{"rows":[[1,"a"]],"changes":0}"#), Ok(vec![vec![json!(1), json!("a")]]));
        assert_eq!(sql::rows(br#"{"error":"duplicate key","code":"23505"}"#), Err("23505 duplicate key".into()));
        assert_eq!(sql::rows(br#"{"error":"no such table: t"}"#), Err("no such table: t".into()));
        let st = sql::statement(Some("main"), "SELECT \"x\" WHERE a = $1", &[json!(1), json!("a\nb"), json!(null)]);
        assert_eq!(serde_json::from_str::<Value>(&st).unwrap(), json!({ "db": "main", "q": "SELECT \"x\" WHERE a = $1", "p": [1, "a\nb", null] }));
        assert_eq!(serde_json::from_str::<Value>(&sql::statement(None, "q", &[])).unwrap(), json!({ "q": "q", "p": [] }));
        assert_eq!(Request::parse("api GET /").json(), Value::Null);
    }
}
