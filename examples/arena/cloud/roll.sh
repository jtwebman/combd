#!/bin/sh
# Move every node to the newest release now, one at a time (the next only
# once the last answers again), so the fleet stays up. Then deploy.sh for the
# game. Without this, nodes move by themselves (combd-update, every minute).
set -e
cd "$(dirname "$0")"
HOST=$(terraform output -raw host)
for n in 0 1 2; do
    echo "n$n.$HOST: the newest combd"
    ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new "root@n$n.$HOST" /usr/local/bin/combd-update now
    i=0; until curl -sf -m 3 -o /dev/null "https://n$n.$HOST/_combd/ping"; do i=$((i + 1)); [ $i -gt 60 ] && { echo "  no answer after 3 minutes"; exit 1; }; sleep 3; done
    echo "  up"
done
