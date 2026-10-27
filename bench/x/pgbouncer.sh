#!/bin/sh
# PgBouncer inside a Workers image, in Hyperdrive's place: in its
# container's own CPU and memory, as combd's and Postgrex's pools are.
#   pgbouncer.sh install           (at build: PgBouncer from the PostgreSQL
#                                   apt repo; 1.21+ keeps prepared statements)
#   pgbouncer.sh start             (at run: pools $PG_URL, $POOL_SIZE server
#                                   connections, on 127.0.0.1:6432; prints
#                                   the URL to connect to it by)
set -eu
case $1 in
install)
    apt-get update && apt-get install -y --no-install-recommends ca-certificates curl
    mkdir -p /usr/share/keyrings
    curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc -o /usr/share/keyrings/pgdg.asc
    echo "deb [signed-by=/usr/share/keyrings/pgdg.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" > /etc/apt/sources.list.d/pgdg.list
    apt-get update && apt-get install -y --no-install-recommends pgbouncer
    rm -rf /var/lib/apt/lists/*
    pgbouncer --version
    ;;
start)
    # postgres://user:pass@host:port/db
    u=${PG_URL#*://} cred=${u%%@*} rest=${u#*@}
    hostport=${rest%%/*} db=${rest#*/}
    cat > /etc/pgbouncer/x.ini <<INI
[databases]
$db = host=${hostport%:*} port=${hostport#*:} dbname=$db user=${cred%%:*} password=${cred#*:}
[pgbouncer]
listen_addr = 127.0.0.1
listen_port = 6432
; only this container's Worker connects
auth_type = trust
auth_file = /etc/pgbouncer/users.txt
pool_mode = transaction
default_pool_size = ${POOL_SIZE:-8}
max_client_conn = 20000
max_prepared_statements = 500
ignore_startup_parameters = extra_float_digits,options
logfile = /tmp/pgbouncer.log
pidfile = /tmp/pgbouncer.pid
INI
    echo "\"${cred%%:*}\" \"\"" > /etc/pgbouncer/users.txt
    chown -R postgres /etc/pgbouncer
    pgbouncer -d -u postgres /etc/pgbouncer/x.ini >&2
    echo "postgres://${cred%%:*}@127.0.0.1:6432/$db"
    ;;
esac
