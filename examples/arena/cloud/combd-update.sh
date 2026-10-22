#!/bin/sh
# combd-update: each node runs this every minute (cron). When a new combd is
# out (the `arena` release), node n moves to it once it has been out n x 2
# minutes, so the nodes restart one at a time and the fleet stays up. No SSH
# needed to roll out. `combd-update now` moves at once (roll.sh, a new node).
URL=$(sed -n 's/^COMBD_RELEASE=//p' /etc/combd/update)
URL=${URL:-https://github.com/jtwebman/combd/releases/download/arena}
N=$(sed -n 's/^COMBD_NODE=//p' /etc/combd/env)
SEEN=/var/lib/combd-update.seen
# The note, signed by CI's release key (cloud/release.pub, here as
# /etc/combd/release.pub): nothing unsigned is fetched, let alone run.
NOTE=$(mktemp) && SIG=$(mktemp) && trap 'rm -f "$NOTE" "$SIG" "$T"' EXIT
curl -fsSL -m 20 -o "$NOTE" "$URL/combd-arena.txt" && curl -fsSL -m 20 -o "$SIG" "$URL/combd-arena.txt.sig" || exit 0
if ! openssl pkeyutl -verify -pubin -inkey /etc/combd/release.pub -rawin -in "$NOTE" -sigfile "$SIG" >/dev/null 2>&1; then
    # Mid-upload (a new note, the old signature), or not ours: next minute.
    logger -t combd-update "the release note's signature doesn't check out: not moving"
    exit 0
fi
INFO=$(cat "$NOTE")
SRC=$(echo "$INFO" | sed -n 's/^source=\([0-9a-f]*\)$/\1/p')
SUM=$(echo "$INFO" | sed -n 's/^sha256=\([0-9a-f]*\)$/\1/p')
[ -n "$SRC" ] && [ -n "$SUM" ] || exit 0
# The same combd inside (only the game changed, and that came as a deploy):
# no restart.
if [ "$(readlink /opt/combd/current)" = "/opt/combd/$SRC" ]; then rm -f $SEEN; exit 0; fi
# Fetched (and checked) first, so its builds can be asked for while this
# node waits its turn.
if [ ! -x "/opt/combd/$SRC/spike/target/release/combd-spike" ]; then
    T=$(mktemp)
    curl -fsSL -m 300 -o "$T" "$URL/combd-arena-linux-amd64.tar.gz" || exit 0
    # Mid-upload, or not what the note says: next minute.
    echo "$SUM  $T" | sha256sum -c --quiet >/dev/null 2>&1 || { logger -t combd-update "sha256 mismatch for $SRC, trying again"; exit 0; }
    rm -rf "/opt/combd/$SRC.new" && mkdir -p "/opt/combd/$SRC.new" && tar xzf "$T" -C "/opt/combd/$SRC.new" --no-same-owner && mv "/opt/combd/$SRC.new" "/opt/combd/$SRC" || exit 0
fi
# Not before the new combd's builds of the fleet's compiled modules are in
# the bucket: moving sooner would take those apps down on this node until
# they were (2026-10-21). It asks for them; a builder makes them.
READY=$(set -a; . /etc/combd/env; set +a; "/opt/combd/$SRC/spike/target/release/combd-spike" builds-ready 2>&1)
case $? in
    0) ;;
    1) [ "$(cat $SEEN.builds 2>/dev/null)" = "$SRC" ] || { logger -t combd-update "$SRC: waiting for builds: $READY"; echo "$SRC" > $SEEN.builds; }; exit 0;;
    *) logger -t combd-update "$SRC: $READY"; exit 0;;
esac
if [ "$1" != now ]; then
    [ "$(cut -d' ' -f1 $SEEN 2>/dev/null)" = "$SRC" ] || echo "$SRC $(date +%s)" > $SEEN
    WAITED=$(( $(date +%s) - $(cut -d' ' -f2 $SEEN) ))
    [ $WAITED -ge $(( ${N:-0} * 120 )) ] || exit 0
    # Don't cut off a game: wait while this node has open WebSockets, but no
    # more than 15 minutes past its turn.
    SOCKS=$(curl -sk -m 3 https://127.0.0.1/_combd/stats | sed -n 's/.*"sockets_open":\([0-9]*\).*/\1/p')
    [ "${SOCKS:-0}" -gt 0 ] && [ $WAITED -lt $(( ${N:-0} * 120 + 900 )) ] && exit 0
fi
logger -t combd-update "moving to $SRC"
ln -sfn "/opt/combd/$SRC" /opt/combd/current.new && mv -T /opt/combd/current.new /opt/combd/current
systemctl restart combd
rm -f $SEEN $SEEN.builds
# Keep this version and the one before it.
ls -1dt /opt/combd/*/ | sed 's:/$::' | grep -vx -e /opt/combd/current -e "/opt/combd/$SRC" -e '.*\.new' | tail -n +2 | xargs -r rm -rf
