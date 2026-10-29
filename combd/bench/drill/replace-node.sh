#!/bin/sh
# Replacing a dead node, on three real nodes on this machine (separate
# processes, killed with -9), over the local test bucket that check.sh
# starts. Every acknowledged write must survive; a node may refuse or wait,
# but never answer with fewer writes than were acknowledged.
#
#   A. Node 1's machine dies and is replaced: same number, empty disk. More
#      writes, then the owner dies too. All of them are there.
#   B. The owner and node 2 die at once (so the newest writes are on node
#      2's disk only, in no bucket), then node 1 is replaced. Alone, it must
#      not answer with less. Once node 2 is back (its disk intact), all of
#      them are there.
#
#   ./bench/drill/replace-node.sh      (from combd/, after ./check.sh once)
set -e
cd "$(dirname "$0")/../.."
BIN=$PWD/target/release/combd
[ -x "$BIN" ] || cargo build --release -q
GUEST=$PWD/../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm
[ -f "$GUEST" ] || (cd ../guest && cargo build --release -q --target wasm32-unknown-unknown)
env_of() { docker inspect "$1" --format '{{range .Config.Env}}{{println .}}{{end}}' | sed -n "s/^$2=//p"; }
export COMBD_S3=http://127.0.0.1:29000 COMBD_S3_REGION=us-east-1
COMBD_S3_KEY=$(env_of combd-s3-test MINIO_ROOT_USER); COMBD_S3_SECRET=$(env_of combd-s3-test MINIO_ROOT_PASSWORD)
export COMBD_S3_KEY COMBD_S3_SECRET
[ -n "$COMBD_S3_KEY" ] || { echo "drill: no local bucket (run ./check.sh once to start it)"; exit 1; }

W=$(mktemp -d)
export COMBD_S3_BUCKET="combd-drill-$(date +%s)"
# KEEP=1 keeps the nodes' logs and data, in the folder it names.
trap 'for f in "$W"/pid*; do kill -9 "$(cat "$f")" 2>/dev/null || true; done; if [ -n "$KEEP" ]; then echo "drill: kept $W"; else rm -rf "$W"; fi' EXIT
"$BIN" wire-certs "$W/certs" 3 >/dev/null
OUT=$("$BIN" admin-token '*/*:*')
TOKEN=$(echo "$OUT" | sed -n 's/.*: \(combd_[0-9a-f]*\)$/\1/p')
echo "$OUT" | sed -n 's/^line for the tokens file: *//p' > "$W/tokens"
PEERS="0=127.0.0.1:28600,1=127.0.0.1:28601,2=127.0.0.1:28602"

start() { # node
    env COMBD_NODE=$1 COMBD_PEERS=$PEERS COMBD_WIRE_PORT=2860$1 COMBD_WIRE_CERTS="$W/certs" \
        COMBD_DATA="$W/d$1" COMBD_REPLICAS=2 COMBD_LEASE_MS=1000 COMBD_UPLOAD_MS=600000 \
        COMBD_ADMIN_PORT=2870$1 COMBD_ADMIN_TOKENS="$W/tokens" \
        "$BIN" serve-fleet interp 2850$1 >>"$W/log$1" 2>&1 &
    echo $! > "$W/pid$1"
    i=0; until curl -sf -o /dev/null "http://127.0.0.1:2850$1/_combd/ping"; do i=$((i + 1)); [ $i -gt 100 ] && { echo "drill: node $1 didn't start"; tail -5 "$W/log$1"; exit 1; }; sleep 0.2; done
}
kill9() { for n in "$@"; do kill -9 "$(cat "$W/pid$n")" 2>/dev/null; wait "$(cat "$W/pid$n")" 2>/dev/null || true; rm -f "$W/pid$n"; done; sleep 0.5; }
wipe() { rm -rf "$W/d$1"; }
# Every object's owner record: who owns it, at what epoch, with which peers.
rec() { curl -s --aws-sigv4 "aws:amz:us-east-1:s3" --user "$COMBD_S3_KEY:$COMBD_S3_SECRET" "$COMBD_S3/$COMBD_S3_BUCKET?list-type=2&prefix=own/" | grep -oE "<Key>own/[^<]+" | sed 's/<Key>//' | while read -r k; do echo "   $k $(curl -s --aws-sigv4 "aws:amz:us-east-1:s3" --user "$COMBD_S3_KEY:$COMBD_S3_SECRET" "$COMBD_S3/$COMBD_S3_BUCKET/$k")"; done; }
# What each node still up thinks, for a failure.
stats() { for n in 0 1 2; do [ -f "$W/pid$n" ] && echo "node $n: $(curl -s -m 3 "http://127.0.0.1:2850$n/_combd/stats")"; done; }
get() { curl -s -m 5 -w ' %{http_code}' "http://127.0.0.1:2850$1$2" -X "$3"; }
# Until node $1 answers $2 with count $3; any other 200 fails the drill.
expect() {
    i=0
    while :; do
        r=$(get "$1" "$2" GET)
        case "$r" in
            *' 200') [ "$r" = "{\"count\":$3} 200" ] && return 0; echo "FAIL: node $1 answered $r, want $3: acknowledged writes lost"; exit 1;;
        esac
        i=$((i + 1)); [ $i -gt ${4:-150} ] && { echo "FAIL: node $1 never answered $2 (last: $r)"; stats; exit 1; }
        sleep 0.2
    done
}
writes() { # node object n
    for _ in $(seq 1 "$3"); do r=$(get "$1" "/room/$2/incr" POST); case "$r" in *' 200') ;; *) echo "FAIL: write to $2: $r"; exit 1;; esac; done
}

for n in 0 1 2; do start $n; done
mkdir -p "$W/app/modules" && cp "$GUEST" "$W/app/modules/classic.wasm"
printf '[app]\nname = "drill"\n[durable.room]\nmodule = "classic"\nroutes = ["/room/:id/*"]\n' > "$W/app/combd.toml"
COMBD_TOKEN=$TOKEN COMBD_ADMIN_CA="$W/certs/ca.pem" COMBD_ADMIN_NAME=node0.combd "$BIN" deploy "$W/app" drill https://127.0.0.1:28700 >/dev/null
for n in 0 1 2; do expect $n /room/warm/count 0; done

echo "A: node 1 replaced, then the owner dies"
writes 0 r1 20
echo " after 20 writes:"; rec
kill9 1; wipe 1; start 1
grep -q "peer store was lost" "$W/log1" || { echo "FAIL: node 1 didn't notice its disk was new"; exit 1; }
writes 0 r1 10
expect 0 /room/r1/count 30
echo " after node 1 is back and 10 more:"; rec
kill9 0
expect 1 /room/r1/count 30
expect 2 /room/r1/count 30
writes 1 r1 1
expect 2 /room/r1/count 31
echo "   ok: 31 of 31"

echo "B: the owner and node 2 die at once, then node 1 is replaced, alone"
start 0
writes 0 r2 20
kill9 0 2; kill9 1; wipe 1; start 1
t=$(($(date +%s) + 8))
while [ "$(date +%s)" -lt $t ]; do
    r=$(get 1 /room/r2/count GET)
    case "$r" in *' 200') [ "$r" = '{"count":20} 200' ] || { echo "FAIL: node 1 alone answered $r: acknowledged writes lost"; exit 1; };; esac
    sleep 0.3
done
echo "   alone for 8 s: no wrong answer (last: $r)"
start 2
expect 1 /room/r2/count 20 300
writes 1 r2 1
expect 2 /room/r2/count 21
echo "   ok: 21 of 21"
echo "drill: passed"
