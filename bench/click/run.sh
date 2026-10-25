#!/bin/bash
# The click race: million on combd (compiled), million-workers on workerd
# and celld, and million-phoenix on the BEAM, one server at a time, each
# limited like a $6 droplet (1 CPU, 1 GB, no swap), driven by ws-click
# from its own containers.
#
#   bench/click/run.sh                  # build, then every server and test
#   SERVERS="combd celld" TESTS="idle 1000" bench/click/run.sh
#
# Servers: combd, workerd, celld, phoenix. The Workers app holds pages'
# sockets in a few Hub objects (its best design here), or, to compare, a
# Viewer object per socket (workerd-object, celld-object) or the Worker
# (workerd-worker, celld-worker): examples/million-workers/README.md.
#
# Tests: `idle` (pages that only watch and renew, added until the server
# refuses or falls over) and a number of clicking pages (each clicks 1 to 30
# boxes 1 to 20 s apart, then watches). Pages arrive at RATE a second, STEP
# to a client container (idle: IDLE_RATE and IDLE_STEP, for finer steps
# where a server's limit is low). Raw logs and a summary go to $OUT.
set -u
cd "$(dirname "$0")/../.."
SERVERS=${SERVERS:-"combd workerd celld phoenix"}
TESTS=${TESTS:-"idle 10 100 500 1000 5000 10000 20000"}
RATE=${RATE:-1000}             # new pages a second
STEP=${STEP:-2000}             # pages to a client container
SECS=${SECS:-180}              # clicking, once every page is open
IDLE_MAX=${IDLE_MAX:-240000}   # idle pages to try for
IDLE_SECS=${IDLE_SECS:-900}    # longest an idle ramp may take
OUT=${OUT:-/tmp/click-race-$(date +%Y%m%d-%H%M%S)}
NET=combd-click
mkdir -p "$OUT"

if [ "${BUILD:-1}" = 1 ]; then
    for t in combd workerd celld celld-object celld-worker phoenix; do
        docker build -q -f bench/click/Dockerfile --target $t -t bench-click-$t . >/dev/null || exit 1
    done
fi
docker network create $NET >/dev/null 2>&1

# client <k> <pages> <max clicks> <max gap s>: one ws-click container, cli<k>.
client() {
    docker run -d --name cli$1 --network $NET --ulimit nofile=1048576:1048576 \
        bench-click-combd combd ws-click srv:8080 million.localhost /ws "$2" - "$3" "$4" >/dev/null
}
stop_all() {
    docker ps -aq --filter name='^cli[0-9]' | xargs docker rm -f >/dev/null 2>&1
    docker rm -f srv click-net >/dev/null 2>&1
}
# logs <prefix>: every client's log, and the server's.
logs() {
    for c in $(docker ps -a --format '{{.Names}}' --filter name='^cli[0-9]'); do docker logs $c > "$1-$c.log" 2>&1; done
    docker logs srv > "$1-srv.log" 2>&1
}
# Connections refused, over all the clients.
refusals() { grep -Ho 'refused [0-9]* times' "$1"-cli*.log 2>/dev/null | awk -F: '{ split($2, a, " "); if (a[2] > n[$1]) n[$1] = a[2] } END { for (f in n) t += n[f]; print t + 0 }'; }

# start <server>: a fresh server container, `srv`, limited like the droplet,
# and a sidecar in its network namespace (click-net) to count its sockets.
start() {
    stop_all
    rm -f "$OUT/.cpu"
    case $1 in
        combd) args="bench-click-combd" ;;
        workerd) args="bench-click-workerd" ;;
        workerd-object|workerd-worker) args="-e SESSIONS=${1#workerd-} bench-click-workerd" ;;
        *) args="bench-click-$1" ;;
    esac
    docker run -d --name srv --network $NET --cpus=1 --memory=1g --memory-swap=1g \
        --ulimit nofile=1048576:1048576 $args >/dev/null || exit 1
    docker run -d --name click-net --network container:srv debian:bookworm-slim sleep infinity >/dev/null
    for _ in $(seq 1 120); do
        docker run --rm --network $NET bench-click-combd bash -c \
            'exec 3<>/dev/tcp/srv/8080 && printf "GET /api/count HTTP/1.1\r\nhost: million.localhost\r\nconnection: close\r\n\r\n" >&3 && head -1 <&3' \
            2>/dev/null | grep -q " 200" && return
        sleep 1
    done
    echo "$1 never answered"; exit 1
}

# The server's memory (MiB: its cgroup's, less inactive page cache, as
# docker stats counts it) and CPU (% of a core since the last call), read
# from its cgroup by a probe container (docker stats hangs under load).
docker rm -f click-probe >/dev/null 2>&1
docker run -d --name click-probe -v /sys/fs/cgroup:/cg:ro debian:bookworm-slim sleep infinity >/dev/null
mem() {
    id=$(docker inspect -f '{{.Id}}' srv 2>/dev/null) || { echo "0 0"; return; }
    docker exec click-probe sh -c "cd /cg/docker/$id 2>/dev/null && echo \$(cat memory.current) \$(awk '\$1 == \"inactive_file\" {print \$2}' memory.stat) \$(awk '\$1 == \"usage_usec\" {print \$2}' cpu.stat) \$(date +%s%N)" |
        awk -v f="$OUT/.cpu" '{ prev = ""; getline prev < f; close(f); split(prev, p, " ")
            printf "%.1f %.0f\n", ($1 - $2) / 1048576, (p[2] && $4 > p[2]) ? 100000 * ($3 - p[1]) / ($4 - p[2]) : 0
            print $3, $4 > f }'
}
up() { [ "$(docker inspect -f '{{.State.Running}}' srv 2>/dev/null)" = true ]; }
# WebSockets the server holds (established on :8080, i.e. 1F90).
held() {
    n=$(docker exec click-net sh -c "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | awk '\$2 ~ /:1F90\$/ && \$4 == \"01\"' | wc -l" 2>/dev/null)
    echo "${n:-0}"
}
died() { docker inspect -f 'server died (OOMKilled={{.State.OOMKilled}} exit={{.State.ExitCode}})' srv; }

# idle <server>: pages that never click (their first click is due in ~a
# week), each renewing every 20 s as million's page does, until IDLE_MAX
# have arrived, the server holds two steps fewer than have arrived for
# 30 s (it refuses, or can't keep up), or it dies. Sampled every second.
idle() {
    s=$1 log=$OUT/$1-idle step=${IDLE_STEP:-$STEP} rate=${IDLE_RATE:-$RATE}
    rm -f "$log"[-.]*
    start "$s"
    base=$(mem | cut -d' ' -f1)
    t0=$(date +%s) k=0 sent=0 behind=0 peak=0 peak_mem=$base top=0 h=0 why=
    echo "secs	arrived	held	mem_mib	cpu_pct" > "$log.tsv"
    while :; do
        if [ $sent -lt "$IDLE_MAX" ] && [ $behind -eq 0 ]; then
            k=$((k + 1)) sent=$((sent + step))
            client $k $step 1 1000000
        fi
        for _ in $(seq 1 $((step / rate))); do
            sleep 1
            h=$(held) m=$(mem)
            echo "$(($(date +%s) - t0))	$sent	$h	${m% *}	${m#* }" >> "$log.tsv"
            up || break
            if [ "$h" -gt "$peak" ]; then peak=$h peak_mem=${m% *}; fi
            top=$(awk -v a="$top" -v b="${m% *}" 'BEGIN { print (b > a ? b : a) }')
            if [ "$h" -lt $((sent - 2 * step)) ]; then behind=$((behind + 1)); else behind=0; fi
        done
        if ! up; then why=$(died); break; fi
        if [ $behind -ge 30 ]; then why="refusing or behind (held $h of $sent arrived)"; break; fi
        if [ $sent -ge "$IDLE_MAX" ] && [ $behind -eq 0 ]; then
            sleep 20; h=$(held) m=$(mem); peak=$h peak_mem=${m% *}; why="all $IDLE_MAX open"; break
        fi
        if [ $(($(date +%s) - t0)) -ge "$IDLE_SECS" ]; then why="out of time"; break; fi
    done
    # Still serving? A new page must get its echo.
    if up; then
        docker run -d --name cli0 --network $NET bench-click-combd combd ws-click srv:8080 million.localhost /ws 1 - 1 1 >/dev/null
        sleep 15
        if docker logs cli0 2>&1 | grep -q 'clicks 1 .*lost 0'; then why="$why; a new page works"; else why="$why; a new page got no echo in 15 s"; fi
    fi
    logs "$log"
    per=$(awk -v m="$peak_mem" -v b="$base" -v h="$peak" 'BEGIN { if (h > 0) printf "%.1f", (m - b) * 1024 / h; else print "-" }')
    printf "%-15s idle   held %7d  memory %4.0f MiB (base %3.0f, top %4.0f)  %5s KB a page  refusals %6d  end: %s\n" \
        "$s" "$peak" "$peak_mem" "$base" "$top" "$per" "$(refusals "$log")" "$why" | tee -a "$OUT/summary.txt"
    stop_all
}

# active <server> <pages>: every page clicking, SECS after all are open.
active() {
    s=$1 n=$2 log=$OUT/$1-$2
    rm -f "$log"-*
    start "$s"
    base=$(mem | cut -d' ' -f1)
    k=0
    for left in $(seq "$n" -"$STEP" 1); do
        k=$((k + 1))
        client $k $((left < STEP ? left : STEP)) 30 20
        sleep $((STEP / RATE))
    done
    # Each client's first window line comes once its pages are all open.
    for _ in $(seq 1 600); do
        w=0
        for c in $(seq 1 $k); do docker logs cli$c 2>&1 | grep -q '^ws-click +' && w=$((w + 1)); done
        [ $w -eq $k ] && break
        up || break
        sleep 1
    done
    echo "secs	mem_mib	cpu_pct" > "$log-mem.tsv"
    t0=$(date +%s)
    while [ $(($(date +%s) - t0)) -lt "$SECS" ] && up; do
        echo "$(($(date +%s) - t0))	$(mem | tr ' ' '\t')" >> "$log-mem.tsv"
        sleep 5
    done
    dead=$(up || died)
    logs "$log"
    # Per 10 s window, summed over the clients (the worst client's p50 and
    # p99); windows up to SECS.
    cat "$log"-cli*.log | awk -v secs="$SECS" -v n="$n" -v s="$s" '
        /^ws-click \+/ {
            t = $2; sub(/^\+/, "", t); sub(/s:$/, "", t); t += 0
            if (t > secs) next
            for (i = 1; i < NF; i++) {
                if ($i == "clicks") clicks[t] += $(i + 1)
                if ($i == "p50" && $(i + 1) > p50[t]) p50[t] = $(i + 1)
                if ($i == "p99" && $(i + 1) > p99[t]) p99[t] = $(i + 1)
                if ($i == "lost") { x = $(i + 1); sub(/,/, "", x); lost[t] += x }
                if ($i == "seen") changes[t] += $(i + 1)
                if ($i == "closed") closed[t] += $(i - 1)
                if ($i == "up,") up[t] += $(i - 1)
            }
            if (t > last) last = t
        }
        END {
            for (t in clicks) {
                C += clicks[t]; L += lost[t]; X += changes[t]
                if (changes[t] / 10 > peak) peak = changes[t] / 10
                if (p50[t] > w50) w50 = p50[t]
                if (p99[t] > w99) w99 = p99[t]
            }
            printf "%-15s %6d pages  changes/s %7.0f avg %7.0f peak  clicks %6d  lost %5d (%.2f%%)  worst p50 %5d ms  p99 %5d ms  up %d closed %d",
                s, n, X / (last > 0 ? last : 1), peak, C, L, C ? 100 * L / C : 0, w50, w99, up[last], closed[last]
        }' > "$log-sum.txt"
    peak=$(awk 'NR > 1 && $2 > m { m = $2 } END { printf "%.0f", m }' "$log-mem.tsv")
    cpu=$(awk 'NR > 2 { c += $3; k++ } END { printf "%.0f", k ? c / k : 0 }' "$log-mem.tsv")
    printf "%s  refused %d  memory %4.0f MiB peak (base %3.0f)  cpu %3s%% avg %s\n" "$(cat "$log-sum.txt")" "$(refusals "$log")" "$peak" "$base" "$cpu" "$dead" | tee -a "$OUT/summary.txt"
    stop_all
}

{
    echo "click race: servers [$SERVERS], tests [$TESTS], $RATE new pages a second, $STEP a client, ${SECS}s clicking; logs in $OUT"
    echo "docker $(docker version --format '{{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}}'), $(docker info --format '{{.NCPU}} CPUs, {{.MemTotal}} bytes') for docker; $(git rev-parse --short HEAD)"
    for t in combd workerd celld celld-object celld-worker phoenix; do echo "bench-click-$t $(docker image inspect -f '{{.Id}}' bench-click-$t | cut -c1-19)"; done
} | tee "$OUT/summary.txt"
for t in $TESTS; do
    for s in $SERVERS; do
        if [ "$t" = idle ]; then idle "$s"; else active "$s" "$t"; fi
    done
done
stop_all
docker rm -f click-probe cli0 >/dev/null 2>&1
