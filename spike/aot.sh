#!/bin/sh
# Compile the Rust cell ahead of time, then build the spike with the AOT tier.
# Needs guest/ built first.
set -e
cd "$(dirname "$0")"
cargo build --release
./target/release/combd-spike aot-gen ../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm aot/src/gen_rust.rs
cargo build --release --features aot
