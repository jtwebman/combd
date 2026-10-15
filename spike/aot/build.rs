fn main() {
    println!("cargo:rerun-if-changed=src/trap.c");
    if std::env::var_os("CARGO_FEATURE_GUARD").is_some() {
        cc::Build::new().file("src/trap.c").opt_level(2).compile("combd_trap");
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
