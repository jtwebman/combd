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
}
