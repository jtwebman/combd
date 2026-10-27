#!/bin/sh
# Everything we test, on this machine, before a push (CI doesn't run tests):
# the suite with Postgres and a bucket (local containers, started if
# needed), combd-mem's own tests, the Wasm spec testsuite, and a short
# differential fuzz (freeze and thaw included). ./check.sh quick skips the
# last two.
set -e
cd "$(dirname "$0")"
start() { # name, image (and its command), port, env...
    name=$1 image=$2 port=$3; shift 3
    docker ps --format '{{.Names}}' | grep -qx "$name" && return
    docker start "$name" >/dev/null 2>&1 || docker run -d --name "$name" -p "127.0.0.1:$port" "$@" $image >/dev/null
}
env_of() { docker inspect "$1" --format '{{range .Config.Env}}{{println .}}{{end}}' | sed -n "s/^$2=//p"; }
if command -v docker >/dev/null && docker info >/dev/null 2>&1; then
    start combd-pg-test postgres:16-alpine 25432:5432 -e POSTGRES_PASSWORD="$(openssl rand -hex 12)"
    start combd-s3-test "pgsty/silo:RELEASE.2026-09-16T00-00-00Z server /data" 29000:9000 -e MINIO_ROOT_USER=combd -e MINIO_ROOT_PASSWORD="$(openssl rand -hex 12)"
    export COMBD_PG_TEST="postgres://postgres:$(env_of combd-pg-test POSTGRES_PASSWORD)@127.0.0.1:25432/postgres"
    export COMBD_S3=http://127.0.0.1:29000 COMBD_S3_REGION=us-east-1
    COMBD_S3_KEY=$(env_of combd-s3-test MINIO_ROOT_USER); COMBD_S3_SECRET=$(env_of combd-s3-test MINIO_ROOT_PASSWORD)
    export COMBD_S3_KEY COMBD_S3_SECRET
    sleep 2
else
    echo "check: no docker, so no Postgres or bucket: those tests skip"
fi
(cd ../guest && cargo build --release --target wasm32-unknown-unknown -q)
for ex in ../examples/arena ../examples/million; do "$ex/build.sh" >/dev/null 2>&1; done
out=$(mktemp) && trap 'rm -f "$out"' EXIT
cargo test --release -q >"$out" 2>&1 || true
grep -E "test result|FAILED|panicked" "$out"
grep -q "test result: ok" "$out" && ! grep -q FAILED "$out" || { grep -E "^error" -A8 "$out" | head -30; echo "check: tests failed"; exit 1; }
cargo test --release -q -p combd-mem -p combd-load >"$out" 2>&1 || true
grep -q "test result: ok" "$out" && ! grep -q FAILED "$out" || { grep -E "FAILED|panicked" "$out"; echo "check: combd-mem or combd-load failed"; exit 1; }
# The compiled tier too, when aot.sh has made its modules (they aren't in git).
if [ -f aot/src/gen_rust.rs ]; then
    cargo test --release -q --features aot >"$out" 2>&1 || true
    grep -q "test result: ok" "$out" && ! grep -q FAILED "$out" || { grep -E "FAILED|panicked" "$out"; echo "check: the compiled tier's tests failed"; exit 1; }
fi
# No secrets in what's about to be pushed (gitleaks; .gitleaks.toml), and no
# dependency with a known hole (cargo audit; .cargo/audit.toml says what's
# accepted, and why).
if command -v docker >/dev/null && docker info >/dev/null 2>&1; then
    range=$(git rev-parse -q --verify origin/main >/dev/null && echo "origin/main..HEAD" || echo "--all")
    docker run --rm -v "$(git rev-parse --show-toplevel)":/repo -w /repo ghcr.io/gitleaks/gitleaks:v8.30.1 git --no-banner --redact --log-opts="$range" . >"$out" 2>&1 || { grep -v "^$" "$out" | tail -20; echo "check: gitleaks found a secret"; exit 1; }
fi
if command -v cargo-audit >/dev/null; then
    cargo audit -q >"$out" 2>&1 || { tail -30 "$out"; echo "check: cargo audit found a vulnerable dependency"; exit 1; }
else
    echo "check: no cargo-audit (cargo install cargo-audit): dependencies not checked"
fi
[ "$1" = quick ] && { echo "check: ok (quick)"; exit 0; }
# Compiled modules (slow: they compile): a build loaded the way a node does,
# a deploy that waits for a builder, and builds' signatures.
cargo test --release -q --features aot-lite -- --include-ignored a_build_loads a_compiled_deploy builds:: 2>&1 | grep -q "test result: ok. 3 passed" || { echo "check: compiled modules failed"; exit 1; }
./spectest.sh | tail -1 | tee /dev/stderr | grep -q " 0 fail" || { echo "check: spec testsuite failed"; exit 1; }
./target/release/combd fuzz 3000 | grep -q "no mismatches" || { echo "check: fuzz found a mismatch"; exit 1; }
echo "check: ok"
