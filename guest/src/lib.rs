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
            _ => "404 {\"error\":\"not found\"}".into(),
        }
    }
}

/// One request: "hello", "print", "spin", "panic", "fib N", "sieve N", "sort N",
/// or "api METHOD PATH BODY".
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

    /// Returns the response as (ptr << 32) | len. The response is leaked on purpose.
    #[no_mangle]
    pub extern "C" fn handle(ptr: *mut u8, len: u32) -> u64 {
        let req = unsafe { Vec::from_raw_parts(ptr, len as usize, len as usize) };
        super::api::warm(); // statics live outside the request arena
        // Everything the request allocates dies with it, except the answer,
        // which is copied out before the arena resets.
        let answer = super::arena::scope(|| super::run(core::str::from_utf8(&req).unwrap_or(""), || unsafe { counter_incr() }));
        // The arena is off now and nothing has reused it yet: copy the answer out.
        let out: Box<[u8]> = answer.as_bytes().into();
        drop(answer);
        let out = Box::into_raw(out);
        let len = out.len() as u64;
        ((out as *mut u8 as u64) << 32) | len
    }
}
