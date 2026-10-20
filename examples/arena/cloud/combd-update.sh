#!/bin/sh
# combd-update: each node runs this every minute (cron). When a new image is
# out, node n moves to it once it has been out n x 2 minutes, so the nodes
# restart one at a time and the fleet stays up. No SSH needed to roll out.
IMAGE=$(sed -n 's/^COMBD_IMAGE=//p' /etc/combd/update)
IMAGE=${IMAGE:-ghcr.io/jtwebman/combd-arena:latest}
N=$(sed -n 's/^COMBD_NODE=//p' /etc/combd/env)
SEEN=/var/lib/combd-update.seen
docker pull -q "$IMAGE" >/dev/null 2>&1 || exit 0
NEW=$(docker image inspect -f '{{.Id}}' "$IMAGE")
CUR=$(docker inspect -f '{{.Image}}' combd 2>/dev/null)
if [ "$NEW" = "$CUR" ]; then rm -f $SEEN; exit 0; fi
# The same combd inside (only the game changed, and that came as a deploy):
# no restart.
src() { docker image inspect -f '{{ index .Config.Labels "combd.source" }}' "$1" 2>/dev/null; }
NEWSRC=$(src "$NEW"); CURSRC=$(src "$CUR")
if [ -n "$NEWSRC" ] && [ "$NEWSRC" = "$CURSRC" ]; then rm -f $SEEN; exit 0; fi
[ "$(cut -d' ' -f1 $SEEN 2>/dev/null)" = "$NEW" ] || echo "$NEW $(date +%s)" > $SEEN
WAITED=$(( $(date +%s) - $(cut -d' ' -f2 $SEEN) ))
[ $WAITED -ge $(( ${N:-0} * 120 )) ] || exit 0
# Don't cut off a game: wait while this node has open WebSockets, but no
# more than 15 minutes past its turn.
SOCKS=$(curl -sk -m 3 https://127.0.0.1/_combd/stats | sed -n 's/.*"sockets_open":\([0-9]*\).*/\1/p')
[ "${SOCKS:-0}" -gt 0 ] && [ $WAITED -lt $(( ${N:-0} * 120 + 900 )) ] && exit 0
logger -t combd-update "moving to $NEW"
docker rm -f combd >/dev/null 2>&1
docker run -d --name combd --restart unless-stopped --network host --env-file /etc/combd/env \
    -v /var/lib/combd:/data -v /etc/combd:/etc/combd:ro "$IMAGE" ./target/release/combd-spike serve-fleet both 443 >/dev/null
rm -f $SEEN
docker image prune -f >/dev/null 2>&1
