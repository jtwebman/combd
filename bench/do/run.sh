#!/bin/sh
# The Durable Object race: the same counter object on combd, workerd, and
# celld, each limited to CPUS cores (default 1), loaded by wrk from its own
# container. Build first: combd-spike-aot (spike/Dockerfile, FEATURES=aot-guard)
# and the images in bench/do/Dockerfile.
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
        docker run --rm --network combd-bench bench-do-wrk curl -sf -X POST http://srv:8080/do/room1/incr >/dev/null 2>&1 && break
        sleep 1
    done
    for test in hot-write hot-read many-write; do
        case $test in
            hot-write) url=http://srv:8080/do/room1/incr; script="-s /post.lua" ;;
            hot-read) url=http://srv:8080/do/room1/count; script="" ;;
            many-write) url=http://srv:8080/; script="-s /many.lua" ;;
        esac
        docker run --rm --network combd-bench bench-do-wrk wrk -t2 -c"$CONNS" -d3s $script "$url" >/dev/null
        out=$(docker run --rm --network combd-bench bench-do-wrk wrk -t2 -c"$CONNS" -d"${SECS}s" --latency $script "$url")
        rps=$(echo "$out" | awk '/Requests\/sec/ {print $2}')
        p50=$(echo "$out" | awk '$1 == "50%" {print $2}')
        p99=$(echo "$out" | awk '$1 == "99%" {print $2}')
        errs=$(echo "$out" | awk '/Non-2xx|Socket errors/ {print}' | tr '\n' ' ')
        printf "%-8s %-11s %10s req/s   p50 %8s   p99 %8s   %s\n" "$name" "$test" "$rps" "$p50" "$p99" "$errs"
    done
    count=$(docker run --rm --network combd-bench bench-do-wrk curl -s http://srv:8080/do/room1/count)
    echo "         room1 after the run: $count"
    docker rm -f srv >/dev/null 2>&1 || true
}

echo "cores per server: $CPUS, connections: $CONNS, ${SECS}s per test"
run combd combd-spike-aot env COMBD_THREADS="$CPUS" COMBD_DATA=/tmp/data ./target/release/combd-spike serve aot rust 8080
run workerd bench-do-workerd
run celld bench-do-celld
