#!/bin/sh
# Once (2026-10-20): move running nodes from Caddy in front to combd ending
# HTTPS itself (single mode), one node at a time, each checked before the
# next. New droplets start this way (cloud-init). Needs every node on an
# image whose combd has TLS (the push that brought this script).
set -e
cd "$(dirname "$0")"
HOST=$(terraform output -raw host)
IMAGE=$(sed -n 's/^image *= *"\(.*\)"/\1/p' terraform.tfvars)
IMAGE=${IMAGE:-ghcr.io/jtwebman/combd-arena:latest}
./tls.sh
for n in 0 1 2; do
    node="n$n.$HOST"
    echo "$node: settings, updater, and combd on 443"
    ssh -o ConnectTimeout=10 "root@$node" "grep -q '^COMBD_TLS_CERT=' /etc/combd/env || printf 'COMBD_TLS_CERT=/etc/combd/tls/cert.pem\nCOMBD_TLS_KEY=/etc/combd/tls/key.pem\nCOMBD_REDIRECT_PORT=80\n' >> /etc/combd/env"
    scp -q -o ConnectTimeout=10 combd-update.sh "root@$node:/usr/local/bin/combd-update"
    ssh -o ConnectTimeout=10 "root@$node" "docker pull -q $IMAGE >/dev/null && docker rm -f caddy combd >/dev/null 2>&1; \
        docker run -d --name combd --restart unless-stopped --network host --env-file /etc/combd/env \
        -v /var/lib/combd:/data -v /etc/combd:/etc/combd:ro $IMAGE ./target/release/combd serve-fleet interp 443 >/dev/null"
    i=0; until curl -sf -m 3 -o /dev/null "https://$node/_combd/ping"; do i=$((i + 1)); [ $i -gt 60 ] && { echo "  no HTTPS answer after 3 minutes: stopping here"; exit 1; }; sleep 3; done
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "http://$node/")
    [ "$code" = 301 ] || { echo "  port 80 answered $code, not 301: stopping here"; exit 1; }
    echo "  up: HTTPS from combd, 80 redirects"
done
