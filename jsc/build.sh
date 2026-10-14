#!/bin/sh
# Compile langs/js/app.js to a Rust crate, then to a Wasm cell.
set -e
cd "$(dirname "$0")"
cargo build --release
./target/release/jsc ../langs/js/app.js out/app
cd out/app
cargo build --release --target wasm32-unknown-unknown
echo "jsc      -> $(pwd)/target/wasm32-unknown-unknown/release/combd_js_app.wasm"
