//! combd's AOT tier. `combd-spike aot-gen` turns a Wasm module into plain safe
//! Rust (`gen_*.rs`), and rustc + LLVM turn that into machine code for any CPU.
//! Memory safety comes from rustc: no `unsafe` anywhere, every memory access is
//! bounds-checked, and every trap is an `Err` passed back up the call chain.

#![cfg_attr(not(any(feature = "experiment-unchecked", feature = "guard")), forbid(unsafe_code))]
#![cfg_attr(feature = "guard", deny(unsafe_code))]

#[cfg(feature = "guard")]
#[allow(unsafe_code)]
pub mod fiber;
#[cfg(feature = "guard")]
#[allow(unsafe_code)]
pub mod guard;
pub mod rt;

#[cfg(has_js)]
mod gen_js;
mod gen_rust;
#[cfg(has_fuzz)]
mod gen_fuzz;
#[cfg(has_ev)]
mod gen_ev;

/// One precompiled module, found by the hash of its Wasm bytes.
pub struct Compiled {
    pub hash: u64,
    pub imports: &'static [(&'static str, &'static str, u32)],
    pub new: fn(Box<dyn rt::Host>) -> Result<(rt::Ctx, rt::Mem), rt::Trap>,
    pub export: fn(&str) -> Option<u32>,
    pub call: fn(&mut rt::Ctx, &mut rt::Mem, u32, &[u64]) -> Result<Vec<u64>, rt::Trap>,
}

#[cfg(has_js)]
pub static MODULES: &[Compiled] = &[gen_rust::MODULE, gen_js::MODULE];
#[cfg(not(has_js))]
pub static MODULES: &[Compiled] = &[gen_rust::MODULE];

/// FNV-1a, the cache key for "is there an AOT build of these bytes?".
pub fn hash(wasm: &[u8]) -> u64 {
    wasm.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

pub fn find(wasm: &[u8]) -> Option<&'static Compiled> {
    let h = hash(wasm);
    #[cfg(has_fuzz)]
    if let Some(m) = gen_fuzz::ALL.iter().find(|m| m.hash == h) {
        return Some(m);
    }
    #[cfg(has_ev)]
    if gen_ev::MODULE.hash == h {
        return Some(&gen_ev::MODULE);
    }
    MODULES.iter().find(|m| m.hash == h)
}
