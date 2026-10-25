#!/bin/sh
# The arena on three combd nodes, with pretend distance between them:
# node 0 is near (no delay), node 1 is 40 ms away, node 2 is 100 ms away.
# A Silo bucket holds deploys and data; the game is deployed through node 0's
# admin API, over TLS. Then open http://localhost:8081 (or 8082, 8083), and
# run bots on this machine:
#   cd examples/arena/bots && ARENA_SPREAD=40 cargo run --release -- http://localhost:8081 8 mix
#
#   ./demo.sh         start it
#   ./demo.sh deploy  deploy app/ again (after ./build.sh): a live deploy
#   ./demo.sh down    stop it and delete everything
set -e
cd "$(dirname "$0")/../.."
NET=combd-arena
SILO=pgsty/silo:RELEASE.2026-09-16T00-00-00Z
IMG=combd-arena
down() { docker rm -f arena-s3 arena0 arena1 arena2 arena-deploy >/dev/null 2>&1 || true; docker volume rm -f arena-certs arena-tokens >/dev/null 2>&1 || true; }
deploy() {
    docker run --rm --name arena-deploy --network $NET -v arena-certs:/certs:ro -v "$PWD/examples/arena/app:/app:ro" \
        -e COMBD_TOKEN="$(cat examples/arena/.demo-token)" -e COMBD_ADMIN_CA=/certs/ca.pem -e COMBD_ADMIN_NAME=node0.combd \
        $IMG ./target/release/combd deploy /app acme https://arena0:9090
}
if [ "$1" = down ]; then down; docker network rm $NET >/dev/null 2>&1 || true; rm -f examples/arena/.demo-token; echo "stopped"; exit 0; fi
if [ "$1" = deploy ]; then deploy; exit 0; fi
down
docker build -q -f examples/arena/Dockerfile -t $IMG . >/dev/null
docker network create $NET >/dev/null 2>&1 || true
docker run -d --name arena-s3 --network $NET -e MINIO_ROOT_USER=combd -e MINIO_ROOT_PASSWORD=combd-secret-key $SILO server /data >/dev/null
# The fleet's certificates, and a deploy token (only its hash goes to the nodes).
docker volume create arena-certs >/dev/null
docker run --rm -v arena-certs:/certs $IMG ./target/release/combd wire-certs /certs 3 >/dev/null
TOKEN_OUT=$(docker run --rm $IMG ./target/release/combd admin-token 'acme/*:*')
TOKEN=$(echo "$TOKEN_OUT" | sed -n 's/.*: \(combd_[0-9a-f]*\)$/\1/p')
echo "$TOKEN" > examples/arena/.demo-token
LINE=$(echo "$TOKEN_OUT" | sed -n 's/^line for the tokens file: *//p')
docker volume create arena-tokens >/dev/null
docker run --rm -v arena-tokens:/t $IMG sh -c "echo '$LINE' > /t/tokens"
PUBLIC=0=http://localhost:8081,1=http://localhost:8082,2=http://localhost:8083
node() { # n delay-ms
    docker run -d --name arena$1 --network $NET --cap-add NET_ADMIN -p 808$(($1 + 1)):8080 \
        -v arena-certs:/certs:ro -v arena-tokens:/tokens:ro \
        -e COMBD_NODE=$1 -e COMBD_PEERS=0=arena0:7100,1=arena1:7100,2=arena2:7100 -e COMBD_WIRE_CERTS=/certs \
        -e COMBD_REPLICAS=2 -e COMBD_THREADS=2 -e COMBD_DATA=/data -e COMBD_LEASE_MS=3000 \
        -e COMBD_S3=http://arena-s3:9000 -e COMBD_S3_KEY=combd -e COMBD_S3_SECRET=combd-secret-key \
        -e COMBD_PUBLIC_URLS=$PUBLIC -e COMBD_ADMIN_PORT=9090 -e COMBD_ADMIN_TOKENS=/tokens/tokens \
        $IMG sh -c "if [ $2 -gt 0 ]; then tc qdisc add dev eth0 root netem delay ${2}ms; fi; exec ./target/release/combd serve-fleet interp 8080" >/dev/null
}
sleep 2
node 0 0
node 1 40
node 2 100
for n in 1 2 3; do
    i=0; until curl -sf -o /dev/null http://localhost:808$n/_combd/ping; do i=$((i + 1)); [ $i -gt 60 ] && { echo "node $((n - 1)) didn't start:"; docker logs arena$((n - 1)) 2>&1 | tail -5; exit 1; }; sleep 1; done
done
# Deploy the arena, through node 0's admin API (TLS, the fleet's CA).
deploy
echo "up: http://localhost:8081 (node 0, near), :8082 (node 1, 40 ms), :8083 (node 2, 100 ms)"
echo "bots: cd examples/arena/bots && ARENA_SPREAD=40 cargo run --release -- http://localhost:8081 8 mix"
