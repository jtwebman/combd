#!/bin/sh
# Build the game module and put it where the manifest looks for it.
set -e
cd "$(dirname "$0")"
(cd game && cargo build --release --target wasm32-unknown-unknown)
mkdir -p app/modules
cp game/target/wasm32-unknown-unknown/release/arena_game.wasm app/modules/game.wasm
echo "built app/modules/game.wasm"
