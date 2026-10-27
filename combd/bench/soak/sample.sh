#!/bin/sh
# On a node, every 5 minutes (cron, soak.sh puts it there): one line of what
# could leak, to /var/lib/combd-soak.csv.
F=/var/lib/combd-soak.csv
[ -f $F ] || echo "time,rss_kb,fds,threads,counted_mb,available_mb,swap_mb,conns,sockets,sessions,objects,data_kb,journal_mb,restarts,pressure,refused" > $F
pid=$(systemctl show combd -p MainPID --value)
st=$(curl -sk -m 5 https://127.0.0.1/_combd/stats)
j() { echo "$st" | sed -n "s/.*\"$1\":\([0-9]*\).*/\1/p"; }
echo "$(date +%s),$(awk '/VmRSS/ {print $2}' /proc/$pid/status 2>/dev/null),$(ls /proc/$pid/fd 2>/dev/null | wc -l),$(ls /proc/$pid/task 2>/dev/null | wc -l),$(( $(j memory_used) / 1048576 )),$(( $(j machine_available) / 1048576 )),$(free -m | awk '/Swap/ {print $3}'),$(j conns_open),$(j sockets_open),$(j sessions_live),$(j objects_live),$(du -sk /var/lib/combd | cut -f1),$(journalctl --disk-usage 2>/dev/null | sed -n 's/.* \([0-9.]*\)M .*/\1/p'),$(systemctl show combd -p NRestarts --value),$(j pressure),$(j conns_refused)" >> $F
