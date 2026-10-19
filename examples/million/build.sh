#!/bin/sh
# Build the module and put it where the manifest looks for it.
set -e
cd "$(dirname "$0")"
(cd cell && cargo build --release --target wasm32-unknown-unknown)
mkdir -p app/modules
cp cell/target/wasm32-unknown-unknown/release/million_cell.wasm app/modules/million.wasm
echo "built app/modules/million.wasm"
