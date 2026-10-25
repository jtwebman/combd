#!/bin/bash
# The click race: million on combd (compiled), and million-workers on
# workerd and celld, one server at a time, each limited like a $6 droplet
# (1 CPU, 1 GB, no swap), driven by ws-click from its own containers.
#
#   bench/click/run.sh                  # build, then every server and test
#   SERVERS="combd celld" TESTS="idle 1000" bench/click/run.sh
#
# Tests: `idle` (pages that only watch and renew, added until the server
# refuses or falls over) and a number of clicking pages (each clicks 1 to 30
# boxes 1 to 20 s apart, then watches). Raw logs and a summary go to $OUT.
set -u
cd "$(dirname "$0")/../.."
SERVERS=${SERVERS:-"combd workerd workerd-worker celld"}
TESTS=${TESTS:-"idle 1000 5000 10000 20000"}
CLIENTS=${CLIENTS:-4}          # load containers (one IP each: ~60,000 sockets apiece)
SECS=${SECS:-180}              # clicking, once every page is open
IDLE_MAX=${IDLE_MAX:-240000}   # idle pages to try for, across the clients
IDLE_SECS=${IDLE_SECS:-900}    # longest an idle ramp may take
OUT=${OUT:-/tmp/click-race-$(date +%Y%m%d-%H%M%S)}
NET=combd-click
mkdir -p "$OUT"

if [ "${BUILD:-1}" = 1 ]; then
    for t in combd workerd celld; do
        docker build -q -f bench/click/Dockerfile --target $t -t bench-click-$t . >/dev/null || exit 1
    done
fi
docker network create $NET >/dev/null 2>&1

# start <server>: a fresh server container, `srv`, limited like the droplet.
start() {
    docker rm -f srv >/dev/null 2>&1
    rm -f "$OUT/.cpu"
    case $1 in
        combd) args="bench-click-combd" ;;
        workerd) args="bench-click-workerd" ;;
        workerd-worker) args="-e SESSIONS=worker bench-click-workerd" ;;
        celld) args="bench-click-celld" ;;
    esac
    docker run -d --name srv --network $NET --cpus=1 --memory=1g --memory-swap=1g \
        --ulimit nofile=1048576:1048576 $args >/dev/null || exit 1
    for _ in $(seq 1 120); do
        docker run --rm --network $NET bench-click-combd bash -c \
            'exec 3<>/dev/tcp/srv/8080 && printf "GET /api/count HTTP/1.1\r\nhost: million.localhost\r\nconnection: close\r\n\r\n" >&3 && head -1 <&3' \
            2>/dev/null | grep -q " 200" && return
        sleep 1
    done
    echo "$1 never answered"; exit 1
}

# clients <pages each> <max clicks> <max gap s>: CLIENTS ws-click containers.
clients() {
    for c in $(seq 1 "$CLIENTS"); do
        docker rm -f cli$c >/dev/null 2>&1
        docker run -d --name cli$c --network $NET --ulimit nofile=1048576:1048576 \
            --sysctl net.ipv4.ip_local_port_range="1024 65535" bench-click-combd \
            combd-spike ws-click srv:8080 million.localhost /ws "$1" - "$2" "$3" >/dev/null
    done
}

stop_all() {
    for c in $(seq 1 "$CLIENTS"); do docker rm -f cli$c >/dev/null 2>&1; done
    docker rm -f srv >/dev/null 2>&1
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
# Sockets the clients hold open (established), from their side.
held() {
    n=0
    for c in $(seq 1 "$CLIENTS"); do
        k=$(docker exec cli$c sh -c "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | awk '\$4 == \"01\"' | wc -l" 2>/dev/null)
        n=$((n + ${k:-0}))
    done
    echo $n
}

# idle <server>: pages that never click (their first click is due in ~a
# week), each renewing every 20 s as million's page does, until all of
# IDLE_MAX are open, the count stops growing for 60 s, or the server dies.
idle() {
    s=$1 log=$OUT/$1-idle.tsv
    start "$s"
    base=$(mem | cut -d' ' -f1)
    clients $((IDLE_MAX / CLIENTS)) 1 1000000
    t0=$(date +%s) best=0 best_at=$t0 peak=0 peak_mem=$base why=
    echo "secs	held	mem_mib	cpu_pct" > "$log"
    while :; do
        sleep 5
        now=$(date +%s) h=$(held) m=$(mem)
        echo "$((now - t0))	$h	${m% *}	${m#* }" >> "$log"
        if ! up; then why="server died ($(docker inspect -f 'OOMKilled={{.State.OOMKilled}} exit={{.State.ExitCode}}' srv))"; break; fi
        if [ "$h" -gt "$peak" ]; then peak=$h peak_mem=${m% *}; fi
        if [ "$h" -gt $((best + best / 100 + 10)) ]; then best=$h best_at=$now; fi
        if [ "$h" -ge "$IDLE_MAX" ]; then sleep 30; h=$(held) m=$(mem); peak=$h peak_mem=${m% *}; why="all $IDLE_MAX open"; break; fi
        if [ $((now - best_at)) -ge 60 ]; then why="stopped growing"; break; fi
        if [ $((now - t0)) -ge "$IDLE_SECS" ]; then why="out of time"; break; fi
    done
    refused=$(for c in $(seq 1 "$CLIENTS"); do docker logs cli$c 2>&1 | grep -o 'refused [0-9]* times' | tail -1; done | awk '{n += $2} END {print n + 0}')
    for c in $(seq 1 "$CLIENTS"); do docker logs cli$c > "$OUT/$s-idle-cli$c.log" 2>&1; done
    docker logs srv > "$OUT/$s-idle-srv.log" 2>&1
    per=$(awk -v m="$peak_mem" -v b="$base" -v h="$peak" 'BEGIN { if (h > 0) printf "%.1f", (m - b) * 1024 / h; else print "-" }')
    printf "%-15s idle   held %7d  memory %6.0f MiB (base %4.0f)  %5s KB a page  refusals %6d  end: %s\n" \
        "$s" "$peak" "$peak_mem" "$base" "$per" "$refused" "$why" | tee -a "$OUT/summary.txt"
    stop_all
}

# active <server> <pages>: every page clicking, SECS after all are open.
active() {
    s=$1 n=$2 log=$OUT/$1-$2
    start "$s"
    base=$(mem | cut -d' ' -f1)
    clients $((n / CLIENTS)) 30 20
    # Each client's first window line comes once its pages are all open.
    for _ in $(seq 1 600); do
        k=0
        for c in $(seq 1 "$CLIENTS"); do docker logs cli$c 2>&1 | grep -q '^ws-click +' && k=$((k + 1)); done
        [ $k -eq "$CLIENTS" ] && break
        up || break
        sleep 1
    done
    echo "secs	mem_mib	cpu_pct" > "$log-mem.tsv"
    t0=$(date +%s)
    while [ $(($(date +%s) - t0)) -lt "$SECS" ] && up; do
        echo "$(($(date +%s) - t0))	$(mem | tr ' ' '\t')" >> "$log-mem.tsv"
        sleep 5
    done
    died=$(up || docker inspect -f 'server died: OOMKilled={{.State.OOMKilled}} exit={{.State.ExitCode}}' srv)
    for c in $(seq 1 "$CLIENTS"); do docker logs cli$c > "$log-cli$c.log" 2>&1; done
    docker logs srv > "$log-srv.log" 2>&1
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
                if ($i == "refused") refused[t] += $(i + 1)
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
            printf "%-15s %6d pages  changes/s %7.0f avg %7.0f peak  clicks %6d  lost %5d (%.2f%%)  worst p50 %5d ms  p99 %5d ms  closed %d  refused %d\n",
                s, n, X / (last > 0 ? last : 1), peak, C, L, C ? 100 * L / C : 0, w50, w99, closed[last], refused[last]
        }' > "$log-sum.txt"
    peak=$(awk 'NR > 1 && $2 > m { m = $2 } END { printf "%.0f", m }' "$log-mem.tsv")
    cpu=$(awk 'NR > 1 { c += $3; k++ } END { printf "%.0f", k ? c / k : 0 }' "$log-mem.tsv")
    printf "%s  memory %4.0f MiB peak (base %4.0f)  cpu %3s%% avg %s\n" "$(cat "$log-sum.txt")" "$peak" "$base" "$cpu" "$died" | tee -a "$OUT/summary.txt"
    stop_all
}

{
    echo "click race: servers [$SERVERS], tests [$TESTS], $CLIENTS clients, ${SECS}s clicking; logs in $OUT"
    echo "docker $(docker version --format '{{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}}'), $(docker info --format '{{.NCPU}} CPUs, {{.MemTotal}} bytes') for docker; $(git rev-parse --short HEAD)"
    for t in combd workerd celld; do echo "bench-click-$t $(docker image inspect -f '{{.Id}}' bench-click-$t | cut -c1-19)"; done
} | tee "$OUT/summary.txt"
for t in $TESTS; do
    for s in $SERVERS; do
        if [ "$t" = idle ]; then idle "$s"; else active "$s" "$t"; fi
    done
done
docker rm -f click-probe >/dev/null 2>&1
