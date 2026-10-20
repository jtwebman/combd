fn main() {
    println!("cargo:rerun-if-changed=src/trap.c");
    if std::env::var_os("CARGO_FEATURE_GUARD").is_some() {
        cc::Build::new().file("src/trap.c").opt_level(2).compile("combd_trap");
    }
    // The pin (`PIN`): rustc's version, a hash of the sources whose types a
    // loaded build shares with the node, and the target.
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = std::process::Command::new(rustc).arg("-V").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for f in ["src/lib.rs", "src/rt.rs"] {
        println!("cargo:rerun-if-changed={f}");
        for b in std::fs::read(f).expect("source") {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=COMBD_AOT_PIN={version} | combd-aot {h:016x} | {target}");
    // The guest (aot.sh): in the build only when it's been generated.
    println!("cargo:rustc-check-cfg=cfg(has_rust)");
    println!("cargo:rerun-if-changed=src/gen_rust.rs");
    if std::path::Path::new("src/gen_rust.rs").exists() {
        println!("cargo:rustc-cfg=has_rust");
    }
    // The JS cell is in the build only when aot.sh made it (jsc built it first).
    println!("cargo:rustc-check-cfg=cfg(has_js)");
    println!("cargo:rerun-if-changed=src/gen_js.rs");
    if std::path::Path::new("src/gen_js.rs").exists() {
        println!("cargo:rustc-cfg=has_js");
    }
    // The event cell (guest-ev), when aot.sh made it.
    println!("cargo:rustc-check-cfg=cfg(has_ev)");
    println!("cargo:rerun-if-changed=src/gen_ev.rs");
    if std::path::Path::new("src/gen_ev.rs").exists() {
        println!("cargo:rustc-cfg=has_ev");
    }
    // Fuzz modules, only while `aot-fuzz.sh` runs (`combd-spike fuzz-gen`).
    println!("cargo:rustc-check-cfg=cfg(has_fuzz)");
    println!("cargo:rerun-if-changed=src/gen_fuzz.rs");
    if std::path::Path::new("src/gen_fuzz.rs").exists() {
        println!("cargo:rustc-cfg=has_fuzz");
    }
}
