#!/bin/sh
# The soak test on the live fleet (README.md). Run from the repo, with the
# load droplets up (../load: terraform apply -var count_of=6).
#   ./soak.sh start <pages per client>   load, sampling, the outside probe
#   ./soak.sh chaos                      the scheduled failures (README)
#   ./soak.sh stop                       load and sampling off, CSVs fetched
set -e
cd "$(dirname "$0")"
HOST=arena.combd.dev
NODES="n0.$HOST n1.$HOST n2.$HOST"
CLIENTS=$(cd ../load && terraform output -json ips | tr -d '[]" ' | tr , ' ')
IMG=ghcr.io/jtwebman/combd-arena:latest
O="-o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new"
case "$1" in
start)
    PAGES=${2:?pages per client}
    for n in $NODES; do
        scp -q $O sample.sh "root@$n:/usr/local/bin/combd-soak-sample"
        ssh $O "root@$n" 'chmod 755 /usr/local/bin/combd-soak-sample; rm -f /var/lib/combd-soak.csv; echo "*/5 * * * * root /usr/local/bin/combd-soak-sample" > /etc/cron.d/combd-soak; /usr/local/bin/combd-soak-sample'
    done
    # Two clients a node, each its own pages; every 30 minutes each one
    # starts over (all its sockets close, new ones open): churn all along.
    set -- $NODES
    i=0
    for c in $CLIENTS; do
        n=$(eval echo "\${$(( i % 3 + 1 ))}"); ip=$(dig +short "$n" | head -1); i=$((i + 1))
        ssh-keygen -R "$c" >/dev/null 2>&1 || true
        ssh $O "root@$c" "iptables -t raw -C OUTPUT -p tcp --dport 443 -j NOTRACK 2>/dev/null || { iptables -t raw -A OUTPUT -p tcp --dport 443 -j NOTRACK; iptables -t raw -A PREROUTING -p tcp --sport 443 -j NOTRACK; }
            docker rm -f load >/dev/null 2>&1; docker run -d --name load --restart unless-stopped --network host --ulimit nofile=1048576:1048576 -v /etc/ssl/certs/ca-certificates.crt:/ca.pem:ro $IMG ./target/release/combd-spike ws-click $ip:443 million.combd.dev /ws $PAGES /ca.pem 30 20 >/dev/null
            echo '*/30 * * * * root docker restart load >/dev/null' > /etc/cron.d/soak-churn"
        echo "$c: $PAGES pages on $n ($ip)"
    done
    # From outside the fleet: the pages answer, every 30 s, to probe.log.
    # (A [b]racket keeps pkill -f from matching its own ssh command.)
    c=$(echo $CLIENTS | cut -d' ' -f1)
    scp -q $O probe.sh "root@$c:/root/probe.sh"
    ssh $O "root@$c" 'pkill -f "[p]robe.sh" || true'
    ssh $O "root@$c" 'nohup sh /root/probe.sh > /root/probe.log 2>&1 < /dev/null &'
    echo "soak started $(date)"
    ;;
chaos)
    # Timers on the nodes themselves, so they happen with nobody watching.
    set -- $NODES
    ssh $O "root@$2" 'systemd-run -q --on-active=6h --unit=soak-kill systemctl kill -s KILL combd'
    ssh $O "root@$3" 'systemd-run -q --on-active=12h --unit=soak-cut iptables -I OUTPUT -p tcp --dport 443 -m string --algo bm --string r2.cloudflarestorage.com -j DROP
        systemd-run -q --on-active=12h10min --unit=soak-uncut iptables -D OUTPUT -p tcp --dport 443 -m string --algo bm --string r2.cloudflarestorage.com -j DROP'
    ssh $O "root@$1" 'systemd-run -q --on-active=18h --unit=soak-reboot systemctl reboot'
    echo "chaos at +6h (n1: combd killed), +12h (n2: no bucket for 10 min), +18h (n0: reboot), from $(date)"
    ;;
stop)
    for c in $CLIENTS; do ssh $O "root@$c" 'rm -f /etc/cron.d/soak-churn; docker rm -f load >/dev/null 2>&1; pkill -f "[p]robe.sh" || true'; done
    mkdir -p out
    for n in $NODES; do
        ssh $O "root@$n" 'rm -f /etc/cron.d/combd-soak; systemctl stop soak-kill.timer soak-cut.timer soak-uncut.timer soak-reboot.timer 2>/dev/null || true'
        scp -q $O "root@$n:/var/lib/combd-soak.csv" "out/${n%%.*}.csv"
    done
    scp -q $O "root@$(echo $CLIENTS | cut -d' ' -f1):/root/probe.log" out/probe.log || true
    python3 report.py out
    ;;
*) echo "usage: soak.sh start <pages per client> | chaos | stop"; exit 1;;
esac
