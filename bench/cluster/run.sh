#!/bin/sh
# A real fleet in Docker: Silo (a MinIO fork) as the bucket and three combd
# nodes, one core each, on a private network. REPLICAS=2 runs D3 (peers
# confirm writes); the default, 0, runs D2. Each node's data is a volume, so a
# restarted node keeps its peer store, as a real disk would. `fleet-load` drives
# increments through random nodes and checks every object at the end:
# acknowledged <= count <= acknowledged + failed.
#
# Tests: steady load; SIGKILL a node mid-load (restarted after 3 s); freeze a
# node with `docker pause` (it wakes up still thinking it owns things).
#
# Build first: combd-spike-aot (spike/Dockerfile with FEATURES=aot-guard).
set -e
SECS=${SECS:-15}
THREADS=${THREADS:-32}
OBJECTS=${OBJECTS:-50}
IMG=combd-spike-aot
SILO=pgsty/silo:RELEASE.2026-09-16T00-00-00Z
NET=combd-fleet
PEERS=0=node0:7100,1=node1:7100,2=node2:7100
NODES=node0:8080,node1:8080,node2:8080

docker network create $NET >/dev/null 2>&1 || true
cleanup() { docker rm -f s3 node0 node1 node2 >/dev/null 2>&1 || true; docker volume rm -f combd-certs combd-data0 combd-data1 combd-data2 >/dev/null 2>&1 || true; }
cleanup
trap cleanup EXIT

# The wire's certificates: a fleet CA and one per node, in a volume.
docker volume create combd-certs >/dev/null
docker run --rm -v combd-certs:/certs $IMG ./target/release/combd-spike wire-certs /certs 3 >/dev/null

node() { # n
    docker rm -f node$1 >/dev/null 2>&1 || true
    docker run -d --name node$1 --network $NET --cpus=1 -v combd-certs:/certs:ro -e COMBD_WIRE_CERTS=/certs -v combd-data$1:/data \
        -e COMBD_NODE=$1 -e COMBD_PEERS=$PEERS -e COMBD_THREADS=1 -e COMBD_DATA=/data -e COMBD_LEASE_MS=${LEASE_MS:-3000} \
        -e COMBD_REPLICAS=${REPLICAS:-0} \
        -e COMBD_S3=http://s3:9000 -e COMBD_S3_KEY=combd -e COMBD_S3_SECRET=combd-secret-key \
        $IMG ./target/release/combd-spike serve aot rust 8080 >/dev/null
}

load() { # label
    echo "== $1"
    docker run --rm --network $NET $IMG ./target/release/combd-spike fleet-load "$SECS" "$THREADS" "$OBJECTS" $NODES
}

docker run -d --name s3 --network $NET -e MINIO_ROOT_USER=combd -e MINIO_ROOT_PASSWORD=combd-secret-key $SILO server /data >/dev/null
sleep 3
for n in 0 1 2; do node $n; done
sleep 4
for n in 0 1 2; do
    if ! docker run --rm --network $NET bench-wrk curl -sf -m 3 http://node$n:8080/_combd/stats >/dev/null; then
        echo "node$n isn't up:"; docker logs node$n 2>&1 | tail -5; exit 1
    fi
done
docker logs node0 2>&1 | grep -i 'storage test'

load "steady: three nodes, ${THREADS} clients, ${OBJECTS} objects"

(sleep 5; docker kill -s KILL node1 >/dev/null; echo "   (SIGKILL node1 at 5 s)"; sleep 3; node 1; echo "   (node1 restarted at 8 s)") &
load "SIGKILL a node mid-load"
wait

(sleep 5; docker pause node2 >/dev/null; echo "   (froze node2 at 5 s)"; sleep 4; docker unpause node2 >/dev/null; echo "   (node2 woke at 9 s)") &
load "freeze a node for 4 s"
wait
for n in 0 1 2; do echo "node$n stats: $(docker run --rm --network $NET bench-wrk curl -s -m 3 http://node$n:8080/_combd/stats)"; done
