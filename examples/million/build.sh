#!/bin/sh
# Build both modules and put them where the manifest looks for them.
set -e
cd "$(dirname "$0")"
(cd cell && cargo build --release --target wasm32-unknown-unknown)
(cd viewer && cargo build --release --target wasm32-unknown-unknown)
mkdir -p app/modules
cp cell/target/wasm32-unknown-unknown/release/million_cell.wasm app/modules/million.wasm
cp viewer/target/wasm32-unknown-unknown/release/million_viewer.wasm app/modules/viewer.wasm
echo "built app/modules/million.wasm and viewer.wasm"
