#!/bin/sh
# A Workers image's entry: PgBouncer started in the container, the Worker's
# DATABASE_URL pointed at it (or with DIRECT=1, at Postgres itself:
# unpooled, a connection per request), then the runtime ($@).
set -eu
if [ "${DIRECT:-0}" = 1 ]; then DATABASE_URL=$PG_URL; else DATABASE_URL=$(pgbouncer.sh start); fi
export DATABASE_URL
# celld dev reads the Worker's variables from .dev.vars.
echo "DATABASE_URL=$DATABASE_URL" > .dev.vars
exec "$@"
