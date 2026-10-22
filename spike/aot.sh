#!/bin/sh
# Compile the Rust cell ahead of time, then build the spike with the AOT tier.
# Needs guest/ built first. Adds the event cell too if guest-ev is built.
set -e
cd "$(dirname "$0")"
cargo build --release
./target/release/combd-spike aot-gen ../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm aot/src/gen_rust.rs
ev=../guest-ev/target/wasm32-unknown-unknown/release/combd_guest_ev.wasm
if [ -f "$ev" ]; then ./target/release/combd-spike aot-gen "$ev" aot/src/gen_ev.rs; fi
cargo build --release --features aot
