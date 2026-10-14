#!/bin/sh
# The HTTP race: the same app and requests on combd, workerd, celld, Bun, and
# Node, each limited to CPUS cores (default 1), loaded by wrk from its own
# container. Build first: the bench images (see Dockerfile) and combd-spike-aot
# (spike/Dockerfile with FEATURES=aot).
set -e
CPUS=${CPUS:-1}
CONNS=${CONNS:-32}
SECS=${SECS:-10}
docker network create combd-bench >/dev/null 2>&1 || true

run() { # name image command...
    name=$1 image=$2
    shift 2
    docker rm -f srv >/dev/null 2>&1 || true
    docker run -d --rm --name srv --network combd-bench --cpus="$CPUS" "$image" "$@" >/dev/null
    for i in $(seq 1 60); do
        docker run --rm --network combd-bench bench-wrk curl -sf http://srv:8080/api/users/42 >/dev/null 2>&1 && break
        sleep 1
    done
    for route in /api/users/42 /api/users orders; do
        if [ "$route" = orders ]; then url=http://srv:8080/api/orders; script="-s /orders.lua"; else url=http://srv:8080$route; script=""; fi
        docker run --rm --network combd-bench bench-wrk wrk -t2 -c"$CONNS" -d3s $script "$url" >/dev/null
        out=$(docker run --rm --network combd-bench bench-wrk wrk -t2 -c"$CONNS" -d"${SECS}s" --latency $script "$url")
        rps=$(echo "$out" | awk '/Requests\/sec/ {print $2}')
        p50=$(echo "$out" | awk '$1 == "50%" {print $2}')
        p99=$(echo "$out" | awk '$1 == "99%" {print $2}')
        errs=$(echo "$out" | awk '/Non-2xx|Socket errors/ {print}' | tr '\n' ' ')
        printf "%-11s %-15s %10s req/s   p50 %8s   p99 %8s   %s\n" "$name" "$route" "$rps" "$p50" "$p99" "$errs"
    done
    docker rm -f srv >/dev/null 2>&1 || true
}

echo "cores per server: $CPUS, connections: $CONNS, ${SECS}s per route"
run combd-rust combd-spike-aot env COMBD_THREADS="$CPUS" ./target/release/combd-spike serve aot rust 8080
run workerd bench-workerd
run celld bench-celld
run bun bench-bun
run node bench-node
