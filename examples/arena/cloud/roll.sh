#!/bin/sh
# Move every node to the newest image, one at a time (the next only once the
# last answers again), so the fleet stays up. Then deploy.sh for the game.
set -e
cd "$(dirname "$0")"
HOST=$(terraform output -raw host)
IMAGE=$(sed -n 's/^image *= *"\(.*\)"/\1/p' terraform.tfvars)
IMAGE=${IMAGE:-ghcr.io/jtwebman/combd-arena:latest}
for n in 0 1 2; do
    echo "n$n.$HOST: pulling $IMAGE and restarting combd"
    ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new "root@n$n.$HOST" "docker pull -q $IMAGE >/dev/null && docker rm -f combd >/dev/null && \
        docker run -d --name combd --restart unless-stopped --network host --env-file /etc/combd/env \
        -v /var/lib/combd:/data -v /etc/combd:/etc/combd:ro $IMAGE ./target/release/combd-spike serve-fleet interp 8080 >/dev/null"
    i=0; until curl -sf -m 3 -o /dev/null "https://n$n.$HOST/_combd/ping"; do i=$((i + 1)); [ $i -gt 60 ] && { echo "  no answer after 3 minutes"; exit 1; }; sleep 3; done
    echo "  up"
done
