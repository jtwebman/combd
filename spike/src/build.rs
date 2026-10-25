//! `aot-build`: a module's build, the way a node loads it (`combd-load`):
//! its generated Rust (`aot-gen`) with combd-aot, as a library that hands
//! out its pin and its module by the C ABI.

use std::path::Path;

/// What the library is, around the generated code.
const LIB: &str = r#"//! A compiled module, made by `combd aot-build`. A node reads its pin,
//! and only if that's its own, its module.
#![allow(improper_ctypes_definitions)]
pub use combd_aot::{rt, Compiled};
mod gen;
static MODULE: Compiled = gen::MODULE;
#[no_mangle]
pub extern "C" fn combd_pin() -> *const u8 {
    combd_aot::PIN_NUL.as_ptr()
}
#[no_mangle]
pub extern "C" fn combd_module() -> *const Compiled {
    &MODULE
}
"#;

/// Build `wasm` into `out`, against the combd-aot sources at `aot` (the
/// node's own), in `work` (kept between builds: combd-aot compiles once).
/// `target` and `cpu`: for another kind of machine (`x86_64-unknown-linux-gnu`,
/// `x86-64-v3`); else this one.
pub fn build(wasm: &[u8], aot: &Path, work: &Path, out: &Path, target: Option<&str>, cpu: Option<&str>) -> Result<(), String> {
    let hash = format!("{:016x}", combd_aot::hash(wasm));
    let src = crate::aot_gen::generate(wasm, &format!("module {hash}"))?;
    let aot = aot.canonicalize().map_err(|e| format!("{}: {e}", aot.display()))?;
    let dir = work.join(format!("m-{hash}"));
    std::fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
    let toml = format!(
        "[package]\nname = \"combd-module\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\ncombd-aot = {{ path = {:?} }}\n\n[profile.release]\ncodegen-units = 16\n\n[workspace]\n",
        aot.display().to_string()
    );
    std::fs::write(dir.join("Cargo.toml"), toml).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("src/lib.rs"), LIB).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("src/gen.rs"), src).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&dir).args(["build", "--release", "--quiet"]).env("CARGO_TARGET_DIR", work.join("target"));
    if let Some(t) = target {
        cmd.args(["--target", t]);
    }
    if let Some(c) = cpu {
        cmd.env("RUSTFLAGS", format!("-C target-cpu={c}"));
    }
    let o = cmd.output().map_err(|e| format!("cargo: {e}"))?;
    if !o.status.success() {
        return Err(format!("cargo build failed:\n{}", String::from_utf8_lossy(&o.stderr)));
    }
    let lib = if cfg!(target_os = "macos") && target.is_none() { "libcombd_module.dylib" } else { "libcombd_module.so" };
    let built = target.map_or(work.join("target/release"), |t| work.join("target").join(t).join("release")).join(lib);
    std::fs::copy(&built, out).map_err(|e| format!("{}: {e}", built.display()))?;
    Ok(())
}
