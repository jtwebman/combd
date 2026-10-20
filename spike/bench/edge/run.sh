#!/bin/sh
# run.sh <caddy|haproxy|haproxy-small|direct|combd-tls> <n>: a fresh server like an
# arena node (1 GB, 1 CPU: combd with the million app, and the proxy in
# front, if any), n idle WebSockets held by ws-load in another container,
# and the server's memory (its cgroup's: every process and the kernel).
# Needs: docker build -t combd-edge-bench . (here), and combd built for
# Linux into the combd-linux-target volume (see RESULTS.md, "What a node
# holds").
set -e
B=$(cd "$(dirname "$0")" && pwd)
R=$(cd "$B/../../.." && pwd)
if [ ! -f "$B/certs/both.pem" ]; then
    mkdir -p "$B/certs" && cd "$B/certs"
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 -subj /CN=bench-ca -keyout ca.key -out ca.pem 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=million.localhost -keyout key.pem -out req.csr 2>/dev/null
    printf "subjectAltName=DNS:million.localhost\nbasicConstraints=CA:FALSE\n" > ext
    openssl x509 -req -in req.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile ext -out cert.pem 2>/dev/null
    cat cert.pem key.pem > both.pem
    cd - >/dev/null
fi
docker network create edge-bench >/dev/null 2>&1 || true
set +e
mode=$1 n=$2
oom=$(docker exec edge-srv sh -c "grep oom_kill /sys/fs/cgroup/memory.events" 2>/dev/null); echo "  $oom"; docker rm -f edge-srv edge-cli >/dev/null 2>&1
case $mode in
  caddy) proxy="caddy run --config /bench/Caddyfile --adapter caddyfile >/tmp/p.log 2>&1"; port=443; ca=/certs/ca.pem ;;
  haproxy-small) proxy="haproxy -f /bench/haproxy-small.cfg >/tmp/p.log 2>&1"; port=443; ca=/certs/ca.pem ;;
  haproxy) proxy="haproxy -f /bench/haproxy.cfg >/tmp/p.log 2>&1"; port=443; ca=/certs/ca.pem ;;
  direct) proxy="sleep infinity"; port=8080; ca="" ;;
  combd-tls) proxy="sleep infinity"; port=443; ca=/certs/ca.pem; tls="-e COMBD_TLS_CERT=/certs/cert.pem -e COMBD_TLS_KEY=/certs/key.pem"; listen=443 ;;
esac
docker run -d --name edge-srv ${tls:-} --network edge-bench --memory 1g --memory-swap 1g --cpus 1 --ulimit nofile=1048576:1048576 --sysctl net.ipv4.ip_local_port_range="1024 65535" \
  -v combd-linux-target:/target:ro -v $R/examples/million/app:/app:ro -v $B:/bench:ro -v $B/certs:/certs:ro -e MALLOC_MMAP_MAX_=4000000 -e MALLOC_MMAP_THRESHOLD_=131072 \
  combd-edge-bench sh -c "COMBD_DATA=/tmp/d COMBD_THREADS=1 COMBD_MAX_CONNS=1000000 /target/release/combd-spike serve-app interp /app ${listen:-8080} >/tmp/c.log 2>&1 & $proxy & wait" >/dev/null
sleep 3
base=$(docker exec edge-srv cat /sys/fs/cgroup/memory.current)
docker run -d --name edge-cli --network edge-bench --ulimit nofile=1048576:1048576 --sysctl net.ipv4.ip_local_port_range="1024 65535" -v combd-linux-target:/target:ro -v $B/certs:/certs:ro \
  rust:1.94.1-slim-bookworm /target/release/combd-spike ws-load edge-srv:$port million.localhost /ws $n '{"t":"range","from":0,"to":500}' $ca >/dev/null
for i in $(seq 1 300); do docker logs edge-cli 2>&1 | grep -qE "holding|refused|panicked" && break; sleep 2; done
sleep 8
mem=$(docker exec edge-srv cat /sys/fs/cgroup/memory.current)
sock=$(docker exec edge-srv sh -c "grep -E '^(sock|anon|kernel) ' /sys/fs/cgroup/memory.stat" | tr '\n' ' ')
rss=$(docker exec edge-srv ps -o rss=,comm= -C combd-spike,caddy,haproxy | awk '{printf "%s %d KB  ", $2, $1}')
held=$(docker logs edge-cli 2>&1 | grep -E "holding|refused|panicked" | head -1)
echo "$mode n=$n: $held | node memory $(( (mem-base)/1024 )) KB = $(( (mem-base)/n )) B/conn | $sock| $rss"
oom=$(docker exec edge-srv sh -c "grep oom_kill /sys/fs/cgroup/memory.events" 2>/dev/null); echo "  $oom"; docker rm -f edge-srv edge-cli >/dev/null 2>&1
