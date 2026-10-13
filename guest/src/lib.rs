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

/// One request: "hello", "print", "spin", "panic", "fib N", "sieve N", or "sort N".
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
