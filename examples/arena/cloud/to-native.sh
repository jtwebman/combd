#!/bin/sh
# Once (2026-10-21): move running nodes from combd in Docker to combd as its
# own user under systemd (combd.service), one node at a time, each checked
# before the next; then Docker goes. New droplets start this way
# (cloud-init). Needs the `arena` release (CI makes it on every push).
set -e
cd "$(dirname "$0")"
HOST=$(terraform output -raw host)
O="-o ConnectTimeout=10"
curl -fsS -m 20 -L https://github.com/jtwebman/combd/releases/download/arena/combd-arena.txt || { echo "no arena release yet"; exit 1; }
for n in ${NODES:-0 1 2}; do
    node="n$n.$HOST"
    echo "$node: combd's user, files, and service"
    scp -q $O combd.service "root@$node:/etc/systemd/system/combd.service"
    scp -q $O combd-update.sh "root@$node:/usr/local/bin/combd-update"
    scp -q $O combd-tune.sh "root@$node:/usr/local/bin/combd-tune"
    ssh $O "root@$node" "set -e
        id combd >/dev/null 2>&1 || useradd --system --home-dir /var/lib/combd --shell /usr/sbin/nologin combd
        mkdir -p /opt/combd
        sed -i 's|^COMBD_DATA=/data\$|COMBD_DATA=/var/lib/combd|' /etc/combd/env
        printf 'COMBD_RELEASE=https://github.com/jtwebman/combd/releases/download/arena\n' > /etc/combd/update
        sed -i 's/^Before=docker.service/Before=combd.service/' /etc/systemd/system/combd-tune.service
        systemctl daemon-reload && systemctl enable -q combd
        # Stopped, not removed: if the new way doesn't start, the old comes back.
        docker update --restart=no combd >/dev/null && docker stop combd >/dev/null
        chown -R combd:combd /var/lib/combd
        # Its secrets are its own, 600 (combd refuses a key others may read).
        chown combd:combd /etc/combd/tokens /etc/combd/master.key /etc/combd/certs/node$n.key /etc/combd/tls/key.pem
        chmod 600 /etc/combd/tokens /etc/combd/master.key /etc/combd/certs/node$n.key /etc/combd/tls/key.pem
        /usr/local/bin/combd-update now
        if [ ! -x /opt/combd/current/combd/target/release/combd ]; then chown -R root:root /var/lib/combd /etc/combd; docker start combd; echo '  no release fetched: back on Docker'; exit 1; fi
        systemctl is-active -q combd || systemctl start combd"
    i=0; until curl -sf -m 3 -o /dev/null "https://$node/_combd/ping"; do i=$((i + 1)); [ $i -gt 60 ] && { echo "  no answer after 3 minutes: stopping here"; ssh $O "root@$node" 'journalctl -u combd -n 20 --no-pager'; exit 1; }; sleep 3; done
    u=$(ssh $O "root@$node" 'ps -o user= -C combd | head -1')
    [ "$u" = combd ] || { echo "  combd runs as '$u', not combd: stopping here"; exit 1; }
    echo "  up, as combd; Docker goes"
    ssh $O "root@$node" 'docker rm -f combd >/dev/null; systemctl disable --now -q docker docker.socket containerd 2>/dev/null; DEBIAN_FRONTEND=noninteractive apt-get purge -y -qq docker.io containerd runc >/dev/null 2>&1; apt-get autoremove -y -qq >/dev/null 2>&1; rm -rf /var/lib/docker /var/lib/containerd; /usr/local/bin/combd-tune; free -m | sed -n 2p'
done
