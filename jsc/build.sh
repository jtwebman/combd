#!/bin/sh
# Compile langs/js/app.js to a Rust crate, then to a Wasm cell.
set -e
cd "$(dirname "$0")"
cargo build --release
./target/release/jsc ../langs/js/app.js out/app
# Pin the runtime's dependencies to the versions in rt/Cargo.lock.
[ -f out/app/Cargo.lock ] || cp rt/Cargo.lock out/app/Cargo.lock
cd out/app
cargo build --release --target wasm32-unknown-unknown
echo "jsc      -> $(pwd)/target/wasm32-unknown-unknown/release/combd_js_app.wasm"
