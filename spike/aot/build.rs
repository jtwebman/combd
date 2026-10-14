fn main() {
    println!("cargo:rerun-if-changed=src/trap.c");
    if std::env::var_os("CARGO_FEATURE_GUARD").is_some() {
        cc::Build::new().file("src/trap.c").opt_level(2).compile("combd_trap");
    }
}
