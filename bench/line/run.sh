#!/bin/bash
# The line's budget (plan.md, the second wall): the arena and million at p50
# and p99, before and after cells move to a child process. Run it on a quiet
# machine, at the commit to measure. A laptop's numbers drift with what else
# runs, so to judge the budget, run both commits back to back; results/ is
# for a rough idea.
#
#   bench/line/run.sh             # both
#   ONLY=arena bench/line/run.sh  # or million
#
# Arena: one node (interpreted, as its manifest says) and BOTS bots for
# SECS seconds, each pinging its match every 100 ms over the match's socket,
# through the match object and back. Million: bench/click's request race
# on combd alone (GET / and GET /api/count from 10, 100, and 1000
# connections) and 1000 clicking pages. PIN=1 gives million's server one
# CPU of its own, as a 1 vCPU droplet has, not a quota of one CPU's time.
#
# Where a loaded server's time goes, from perf in a container beside it:
#   docker run -d --name perfbox --privileged --pid=container:srv debian:bookworm-slim sleep infinity
#   docker exec perfbox sh -c 'apt-get update && apt-get install -y linux-perf'
#   docker exec perfbox perf record -F 999 -g -p 1 -o /tmp/p.data -- sleep 8
#   docker exec perfbox perf report -i /tmp/p.data --no-children --sort comm,dso,symbol --stdio
set -u
cd "$(dirname "$0")/../.."
BOTS=${BOTS:-8}
SECS=${SECS:-120}
OUT=${OUT:-bench/line/results/$(date +%Y-%m-%d)-$(git rev-parse --short HEAD)$(git diff --quiet HEAD -- combd || echo -dirty).txt}
mkdir -p "$(dirname "$OUT")"
log=$(mktemp -d)
echo "line bench $(date '+%Y-%m-%d %H:%M'), $(git rev-parse --short HEAD), $(uname -sm), $(sysctl -n hw.ncpu 2>/dev/null || nproc) CPUs, load $(uptime | sed 's/.*averages*: //')" | tee "$OUT"

if [ "${ONLY:-arena}" = arena ]; then
    (cd examples/arena && ./build.sh >/dev/null) || exit 1
    (cd examples/arena/bots && cargo build -q --release) || exit 1
    (cd combd && cargo build -q --release) || exit 1
    combd/target/release/combd serve-app interp examples/arena/app 8099 > "$log/srv.log" 2>&1 &
    srv=$!
    trap 'kill $srv 2>/dev/null' EXIT
    until curl -sf localhost:8099/_combd/ping >/dev/null; do
        kill -0 $srv 2>/dev/null || { cat "$log/srv.log"; exit 1; }
        sleep 0.2
    done
    ARENA_PING_MS=100 examples/arena/bots/target/release/arena-bots http://localhost:8099 "$BOTS" mix "$SECS" > "$log/bots.log" 2>&1
    kill $srv
    grep '^all:' "$log/bots.log" | sed "s/^/arena $BOTS bots ${SECS}s  /" | tee -a "$OUT"
fi

if [ "${ONLY:-million}" = million ]; then
    for t in combd wrk; do
        docker build -q -f bench/click/Dockerfile --target $t -t bench-click-$t . >/dev/null || exit 1
    done
    PIN=${PIN:-0} BUILD=0 OUT=$log/click SERVERS=combd TESTS="req10 req100 req1000 1000" bench/click/run.sh > "$log/click.txt" 2>&1
    grep '^combd' "$log/click/summary.txt" | sed 's/^/million /' | tee -a "$OUT"
fi
echo "logs: $log"
