#!/bin/bash
# The X race: examples/x on combd, Phoenix, workerd and celld, against one
# Postgres, apples to apples:
# - the same Postgres (PG_CPUS, 6, and 4 GB: room enough that a 1-CPU
#   server, not the database, is what runs out), the same seeded data (seed.sql, a
#   fresh copy for every test), the same SQL;
# - each server in 1 CPU and 1 GB, its pool inside that (combd's and
#   Postgrex's in the process, PgBouncer in the Workers' container), each
#   with POOL connections to Postgres;
# - the same load (load.lua: 80% timelines, 10% posts, 8% likes, 2%
#   follows), from CONNS wrk connections, SECS seconds after 5 s of warm-up.
# Before its load, each server passes examples/x/check.sh. `workerd-direct`
# and `celld-direct`: the Workers with no pooler, a connection to Postgres
# per request.
#   POOLS="8 200" SERVERS="combd phoenix" CONNS="100 1000" bench/x/run.sh
set -u
cd "$(dirname "$0")/../.."
SERVERS=${SERVERS:-"combd phoenix workerd celld"}
CONNS=${CONNS:-"10 100 1000"}
POOLS=${POOLS:-8}
PG_CPUS=${PG_CPUS:-6}
SECS=${SECS:-30}
OUT=${OUT:-/tmp/x-race-$(date +%Y%m%d-%H%M%S)}
NET=combd-x PW=race PORT=18090
PG="postgres://postgres:$PW@x-pg:5432/x"
mkdir -p "$OUT"

if [ "${BUILD:-1}" = 1 ]; then
    for t in combd phoenix workerd celld wrk pg; do
        docker build -q -f bench/x/Dockerfile --target $t -t bench-x-$t . >/dev/null || { echo "build $t failed"; exit 1; }
    done
fi
docker network create $NET >/dev/null 2>&1
stop() { docker rm -f x-srv >/dev/null 2>&1; }
trap 'stop; docker rm -f x-pg >/dev/null 2>&1' EXIT
psql() { docker exec -i x-pg psql -U postgres -v ON_ERROR_STOP=1 -q "$@"; }

# Postgres, sized for 200 connections, seeded once into a template. A plan
# made for each statement's values, never a cached generic one: a pool that
# keeps prepared statements (Postgrex, PgBouncer's) would otherwise get the
# generic plan after five runs, and the timeline's is 4x slower (3.3 ms to
# 0.8 ms), which would race the plan cache, not the servers.
docker rm -f x-pg >/dev/null 2>&1
docker run -d --name x-pg --network $NET --cpus=$PG_CPUS --memory=4g --shm-size=1g -e POSTGRES_PASSWORD=$PW bench-x-pg \
    -c max_connections=300 -c shared_buffers=1GB -c effective_cache_size=3GB -c plan_cache_mode=force_custom_plan ${PG_ARGS:-} >/dev/null || exit 1
until docker exec x-pg pg_isready -U postgres -q 2>/dev/null; do sleep 1; done
sleep 2
psql -c "CREATE DATABASE x_seed" && psql -d x_seed < examples/x/schema.sql && psql -d x_seed < bench/x/seed.sql || { echo "seed failed"; exit 1; }
fresh() { psql -c "DROP DATABASE IF EXISTS x WITH (FORCE)" -c "CREATE DATABASE x TEMPLATE x_seed"; }

# serve <server> <pool>: up, answering, and passing check.sh.
serve() {
    stop
    case $1 in
        *-direct) img=bench-x-${1%-direct} env="-e DIRECT=1" ;;
        *) img=bench-x-$1 env= ;;
    esac
    docker run -d --name x-srv --network $NET -p 127.0.0.1:$PORT:8080 --cpus=1 --memory=1g --memory-swap=1g \
        --ulimit nofile=1048576:1048576 -e DATABASE_URL=$PG -e PG_URL=$PG -e POOL_SIZE=$2 ${XENV:-} $env $img >/dev/null || return 1
    for _ in $(seq 1 120); do
        [ "$(curl -s -o /dev/null -w '%{http_code}' http://x.localhost:$PORT/api/users/u0)" = 200 ] && break
        sleep 1
    done
    examples/x/check.sh http://x.localhost:$PORT >/dev/null
}

{
    echo "x race: servers [$SERVERS], pools [$POOLS], conns [$CONNS], ${SECS}s each; logs in $OUT"
    echo "docker $(docker version --format '{{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}}'), $(docker info --format '{{.NCPU}} CPUs') for docker; $(git rev-parse --short HEAD)$(git diff --quiet || echo +changes)"
    for t in combd phoenix workerd celld pg; do echo "bench-x-$t $(docker image inspect -f '{{.Id}}' bench-x-$t | cut -c1-19)"; done
} | tee "$OUT/summary.txt"

for pool in $POOLS; do
    for s in $SERVERS; do
        for n in $CONNS; do
            log=$OUT/$s-pool$pool-c$n
            stop
            fresh || exit 1
            if ! serve "$s" "$pool" > "$log-check.txt" 2>&1; then
                echo "$s pool $pool: failed check.sh: $(tail -1 "$log-check.txt")" | tee -a "$OUT/summary.txt"
                docker logs x-srv > "$log-srv.log" 2>&1
                continue
            fi
            w="docker run --rm --network $NET --ulimit nofile=1048576:1048576 -v $PWD/bench/x/load.lua:/load.lua:ro bench-x-wrk wrk -t4 -c$n --timeout 10s --latency -s /load.lua"
            $w -d5s -H "Host: x.localhost" http://x-srv:8080 >/dev/null 2>&1
            $w -d${SECS}s -H "Host: x.localhost" http://x-srv:8080 > "$log.txt" 2>&1 &
            echo "name cpu_pct mem_mib" > "$log-stats.txt"
            while kill -0 $! 2>/dev/null; do
                docker stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}' x-srv x-pg 2>/dev/null |
                    awk '{ m = $3; u = m; gsub(/[0-9.]/, "", u); sub(/[A-Za-z]+$/, "", m); f = u == "GiB" ? 1024 : u == "KiB" ? 1 / 1024 : 1; sub(/%/, "", $2); print $1, $2, m * f }' >> "$log-stats.txt"
            done
            docker logs x-srv > "$log-srv.log" 2>&1
            f=$log.txt
            rps=$(awk '/^Requests\/sec/ { printf "%.0f", $2 }' "$f")
            p50=$(awk '$1 == "50%" { print $2 }' "$f") p99=$(awk '$1 == "99%" { print $2 }' "$f")
            bad=$(awk '/Non-2xx/ { n += $NF } /Socket errors/ { gsub(/,/, ""); n += $4 + $6 + $8 + $10 } END { print n + 0 }' "$f")
            st() { awk -v who=$1 -v col=$2 -v how=$3 '$1 == who { v = $col; s += v; k++; if (v > m) m = v } END { printf "%.0f", (how == "max" ? m : (k ? s / k : 0)) }' "$log-stats.txt"; }
            printf "%-15s pool %3d  %5d conns  %7s req/s  p50 %8s  p99 %8s  errors %6s  server cpu %3s%% mem %4s MiB peak  postgres cpu %3s%%  %s\n" \
                "$s" "$pool" "$n" "${rps:--}" "${p50:--}" "${p99:--}" "$bad" "$(st x-srv 2 avg)" "$(st x-srv 3 max)" "$(st x-pg 2 avg)" \
                "$(docker inspect -f '{{if .State.OOMKilled}}killed for memory{{end}}' x-srv 2>/dev/null)" | tee -a "$OUT/summary.txt"
        done
    done
done
