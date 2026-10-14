#!/bin/sh
# Differential fuzzing of the AOT tier against our interpreter. Writes N fuzz
# modules into the AOT crate as generated Rust, builds it (guard pages, the
# way the cluster runs, then bounds-checked), runs each module on both, and
# clears the fuzz modules out again.
#   sh aot-fuzz.sh [N] [START]
set -e
cd "$(dirname "$0")"
N=${1:-200}
START=${2:-0}
cargo build --release
./target/release/combd-spike fuzz-gen "$N" "$START"
trap './target/release/combd-spike fuzz-gen 0 >/dev/null' EXIT
for f in aot-guard aot; do
    echo "== AOT ($f) vs the interpreter"
    cargo build --release --features "$f"
    ./target/release/combd-spike fuzz-aot "$N" "$START"
done
