#!/bin/sh
# Compile the Rust cell and the JS (QuickJS) cell ahead of time, then build the
# spike with the AOT tier. Needs guest/ and langs/out built first.
set -e
cd "$(dirname "$0")"
cargo build --release
./target/release/combd-spike aot-gen ../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm aot/src/gen_rust.rs
./target/release/combd-spike aot-gen ../langs/out/js.wasm aot/src/gen_js.rs
cargo build --release --features aot
