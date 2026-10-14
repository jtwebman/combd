#!/bin/sh
# Network splits on a real fleet. Like run.sh (Silo and three combd nodes),
# but each kind of traffic has its own Docker network, so one kind of link
# can be cut at a time:
#   combd-bucket: Silo and the nodes
#   combd-wire:   node to node (aliases w0, w1, w2)
#   combd-client: the load driver and the nodes
# Under load, node1 is cut off for 4 s, three ways: from the bucket, from
# the other nodes, and from both (its clients still reach it). Then every
# object is checked: acknowledged <= count <= acknowledged + failed.
#
# REPLICAS=2 runs D3; the default, 0, runs D2. One-way splits need iptables
# in the image; the simulator covers those.
set -e
SECS=${SECS:-15}
THREADS=${THREADS:-32}
OBJECTS=${OBJECTS:-50}
IMG=${IMG:-combd-spike-aot}
SILO=pgsty/silo:RELEASE.2026-09-16T00-00-00Z
PEERS=0=w0:7100,1=w1:7100,2=w2:7100
NODES=node0:8080,node1:8080,node2:8080

cleanup() {
    docker rm -f s3 node0 node1 node2 >/dev/null 2>&1 || true
    docker network rm combd-bucket combd-wire combd-client >/dev/null 2>&1 || true
    docker volume rm -f combd-certs combd-data0 combd-data1 combd-data2 >/dev/null 2>&1 || true
}
cleanup
trap cleanup EXIT
for n in bucket wire client; do docker network create combd-$n >/dev/null; done

docker volume create combd-certs >/dev/null
docker run --rm -v combd-certs:/certs $IMG ./target/release/combd-spike wire-certs /certs 3 >/dev/null

node() { # n: create it, join all three networks, then start it
    docker create --name node$1 --network combd-client --cpus=1 -v combd-certs:/certs:ro -e COMBD_WIRE_CERTS=/certs -v combd-data$1:/data \
        -e COMBD_NODE=$1 -e COMBD_PEERS=$PEERS -e COMBD_THREADS=1 -e COMBD_DATA=/data -e COMBD_LEASE_MS=${LEASE_MS:-3000} \
        -e COMBD_REPLICAS=${REPLICAS:-0} \
        -e COMBD_S3=http://s3:9000 -e COMBD_S3_KEY=combd -e COMBD_S3_SECRET=combd-secret-key \
        $IMG ./target/release/combd-spike serve aot rust 8080 >/dev/null
    docker network connect combd-bucket node$1
    docker network connect --alias w$1 combd-wire node$1
    docker start node$1 >/dev/null
}
cut() { # what: bucket | wire | both
    case $1 in bucket|both) docker network disconnect combd-bucket node1;; esac
    case $1 in wire|both) docker network disconnect combd-wire node1;; esac
}
heal() {
    case $1 in bucket|both) docker network connect combd-bucket node1;; esac
    case $1 in wire|both) docker network connect --alias w1 combd-wire node1;; esac
}

load() { # label
    echo "== $1"
    docker run --rm --network combd-client $IMG ./target/release/combd-spike fleet-load "$SECS" "$THREADS" "$OBJECTS" $NODES
}

docker run -d --name s3 --network combd-bucket -e MINIO_ROOT_USER=combd -e MINIO_ROOT_PASSWORD=combd-secret-key $SILO server /data >/dev/null
sleep 3
for n in 0 1 2; do node $n; done
sleep 4
for n in 0 1 2; do
    if ! docker run --rm --network combd-client bench-wrk curl -sf -m 3 http://node$n:8080/_combd/stats >/dev/null; then
        echo "node$n isn't up:"; docker logs node$n 2>&1 | tail -5; exit 1
    fi
done

load "steady (no split)"
for what in bucket wire both; do
    (sleep 5; cut $what; echo "   (cut node1 off: $what, at 5 s)"; sleep 4; heal $what; echo "   (healed at 9 s)") &
    load "node1 cut off: $what"
    wait
done
for n in 0 1 2; do echo "node$n stats: $(docker run --rm --network combd-client bench-wrk curl -s -m 3 http://node$n:8080/_combd/stats)"; done
