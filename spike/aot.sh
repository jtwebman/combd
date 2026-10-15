#!/bin/sh
# Compile the Rust cell ahead of time, then build the spike with the AOT tier.
# Needs guest/ built first. Adds the JS cell too if jsc/build.sh made it.
set -e
cd "$(dirname "$0")"
cargo build --release
./target/release/combd-spike aot-gen ../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm aot/src/gen_rust.rs
js=../jsc/out/app/target/wasm32-unknown-unknown/release/combd_js_app.wasm
if [ -f "$js" ]; then ./target/release/combd-spike aot-gen "$js" aot/src/gen_js.rs; fi
ev=../guest-ev/target/wasm32-unknown-unknown/release/combd_guest_ev.wasm
if [ -f "$ev" ]; then ./target/release/combd-spike aot-gen "$ev" aot/src/gen_ev.rs; fi
cargo build --release --features aot
