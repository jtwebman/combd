#!/bin/sh
# The burst that outran combd's memory guard (RESULTS.md, "The click
# race"): a fresh combd limited like a $6 droplet (1 CPU, 1 GB, no swap),
# and six clients opening idle pages as fast as they can (~14,000 a
# second). Each second: memory and pages held; at the end, whether the
# kernel killed it. Its own container names, so it runs beside run.sh.
#   bench/click/burst.sh [image] [tries]     (image: bench-click-combd)
IMG=${1:-bench-click-combd} TRIES=${2:-4} BIN=${BIN:-combd} PAGES=${PAGES:-50000} SECS=${SECS:-60} CLIENTS=${CLIENTS:-6}
NET=burst-net
docker network create $NET >/dev/null 2>&1
clean() { docker ps -aq --filter name="^burst-" | xargs docker rm -f >/dev/null 2>&1; }
for t in $(seq 1 "$TRIES"); do
    clean
    docker run -d --name burst-srv --network $NET --cpus=1 --memory=1g --memory-swap=1g \
        --ulimit nofile=1048576:1048576 "$IMG" >/dev/null
    for _ in $(seq 1 60); do
        docker run --rm --network $NET "$IMG" bash -c 'exec 3<>/dev/tcp/burst-srv/8080 && printf "GET /api/count HTTP/1.1\r\nhost: million.localhost\r\nconnection: close\r\n\r\n" >&3 && head -1 <&3' 2>/dev/null | grep -q " 200" && break
        sleep 1
    done
    # combd's own memory (anon: not page cache, which the kernel drops at
    # the limit), five times a second, from inside.
    # And what the kernel can't take back: that plus its own (socket buffers).
    docker exec -d burst-srv sh -c 'while :; do awk "/^anon /{print \$2}" /sys/fs/cgroup/memory.stat >> /tmp/anon; awk "/^(anon|kernel) /{s += \$2} END {print s}" /sys/fs/cgroup/memory.stat >> /tmp/hard; sleep 0.2; done'
    # One client opens about 28,000 to one address (its local ports): six,
    # so combd has to refuse.
    for k in $(seq 1 "$CLIENTS"); do
        docker run -d --name burst-cli$k --network $NET --ulimit nofile=1048576:1048576 "$IMG" \
            $BIN ws-click burst-srv:8080 million.localhost /ws "$PAGES" - 1 1000000 >/dev/null
    done
    peak=0
    for s in $(seq 1 "$SECS"); do
        sleep 1
        [ "$(docker inspect -f '{{.State.Running}}' burst-srv 2>/dev/null)" = true ] || break
        m=$(docker stats --no-stream --format '{{.MemUsage}}' burst-srv 2>/dev/null | cut -d/ -f1)
        echo "  try $t +${s}s  memory $m"
    done
    oom=$(docker inspect -f '{{.State.OOMKilled}}' burst-srv 2>/dev/null)
    # The cgroup's own high-water mark: what one-second samples miss.
    peak=$(docker exec burst-srv cat /sys/fs/cgroup/memory.peak 2>/dev/null | awk '{printf "%.0f MiB", $1 / 1048576}')
    anon=$(docker exec burst-srv sort -n /tmp/anon 2>/dev/null | tail -1 | awk '{printf "%.0f MiB", $1 / 1048576}')
    hard=$(docker exec burst-srv sort -n /tmp/hard 2>/dev/null | tail -1 | awk '{printf "%.0f MiB", $1 / 1048576}')
    held=$(docker exec burst-srv sh -c 'cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | awk "\$4 == \"01\"" | wc -l' 2>/dev/null || echo "-")
    echo "try $t: killed for memory: $oom, its own memory at most ${anon:-?}, with the kernel's ${hard:-?}, cgroup peak ${peak:-?} (of 1024 MiB), connections held: $held"
done
clean
docker network rm $NET >/dev/null 2>&1
