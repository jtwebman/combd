#!/bin/sh
# The BEAM test over HTTP. One core per server. Fast requests (GET
# /api/users/42, 16 connections) run alone, then next to a bad neighbor:
# 4 connections hammering GET /api/fib/32 (several ms of CPU each).
# The number to watch is the fast route's p99.
set -e
SECS=${SECS:-10}
docker network create combd-bench >/dev/null 2>&1 || true

up() { # image command...
    docker rm -f srv >/dev/null 2>&1 || true
    docker run -d --rm --name srv --network combd-bench --cpus=1 "$@" >/dev/null
    for i in $(seq 1 60); do
        docker run --rm --network combd-bench bench-wrk curl -sf http://srv:8080/api/users/42 >/dev/null 2>&1 && return
        sleep 1
    done
}

fast() { # label
    out=$(docker run --rm --network combd-bench bench-wrk wrk -t2 -c16 -d"${SECS}s" --latency http://srv:8080/api/users/42)
    printf "%-30s fast: %9s req/s  p50 %8s  p99 %8s" "$1" "$(echo "$out" | awk '/Requests\/sec/ {print $2}')" \
        "$(echo "$out" | awk '$1 == "50%" {print $2}')" "$(echo "$out" | awk '$1 == "99%" {print $2}')"
}

race() { # name image command...
    name=$1
    shift
    up "$@"
    docker run --rm --network combd-bench bench-wrk wrk -t2 -c16 -d3s http://srv:8080/api/users/42 >/dev/null
    fast "$name alone"
    echo
    docker rm -f neighbor >/dev/null 2>&1 || true
    docker run -d --name neighbor --network combd-bench bench-wrk wrk -t1 -c4 -d"$((SECS + 4))s" http://srv:8080/api/fib/32 >/dev/null
    sleep 2
    fast "$name + heavy neighbor"
    docker wait neighbor >/dev/null 2>&1 || true
    n=$(docker logs neighbor 2>/dev/null | awk '/Requests\/sec/ {print $2}')
    docker rm -f neighbor srv >/dev/null 2>&1 || true
    echo "   (neighbor: ${n:-?} fib/s)"
}

race combd-rust combd-aot env COMBD_THREADS=1 ./target/release/combd serve aot rust 8080
race combd-rust-no-yield combd-aot env COMBD_THREADS=1 COMBD_NO_YIELD=1 ./target/release/combd serve aot rust 8080
race combd-js combd-aot env COMBD_THREADS=1 ./target/release/combd serve aot js 8080
race bun bench-bun
race node bench-node
race celld bench-celld
race workerd bench-workerd
