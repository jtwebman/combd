#!/bin/sh
# From outside the fleet: the pages answer, every 30 s (soak.sh runs it on a
# load droplet, to /root/probe.log).
while true; do
    for u in https://million.combd.dev/ https://arena.combd.dev/; do
        echo "$(date +%s) $u $(curl -s -m 10 -o /dev/null -w %{http_code} $u)"
    done
    sleep 30
done
