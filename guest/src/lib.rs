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
        let out = super::run(core::str::from_utf8(&req).unwrap_or(""), || unsafe { counter_incr() });
        let out = Box::into_raw(out.into_bytes().into_boxed_slice());
        let len = out.len() as u64;
        ((out as *mut u8 as u64) << 32) | len
    }
}
