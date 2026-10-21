#!/bin/sh
# The kernel TLS tests, which need a kernel with kTLS (not macOS, Docker
# Desktop, or WSL): built for x86-64 Linux here (in Docker), run on a node.
#   ./ktls-test.sh [host]        (default: n0.arena.combd.dev)
set -e
cd "$(dirname "$0")/.."
HOST=${1:-n0.arena.combd.dev}
docker run --rm --platform linux/amd64 -v "$PWD":/combd -v combd-amd64-target:/target -v combd-cargo-amd64:/usr/local/cargo/registry \
    -w /combd/spike -e CARGO_TARGET_DIR=/target rust:1.94.1-slim-bookworm sh -c \
    'cargo test -q --release -p combd-ktls --no-run && cp $(ls -t /target/release/deps/combd_ktls-* | grep -v "\.d$" | head -1) /combd/spike/target/ktls-test-amd64'
scp -q spike/target/ktls-test-amd64 "root@$HOST:/tmp/ktls-test"
ssh "root@$HOST" 'modprobe tls; /tmp/ktls-test --ignored; s=$?; rm -f /tmp/ktls-test; exit $s'
