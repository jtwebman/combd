# Soak

The live fleet under steady load for a day or two, to find what only time
shows: memory, open files, threads, disk, or logs that keep growing, and
whether it comes back from failures with nobody watching.

1. Load droplets: `cd ../load && TF_VAR_do_token=... terraform apply -var count_of=6`
   (six in nyc3, ~$1 a day; destroy them after).
2. `./soak.sh start 500`: each node samples itself every 5 minutes
   (`sample.sh`, to `/var/lib/combd-soak.csv`); two clients a node each hold
   500 million pages (`ws-click`) and start over every 30 minutes, so
   connections, sessions, and closes churn all along; one client checks
   million's and the arena's pages from outside every 30 s.
3. `./soak.sh chaos`, once the load is steady: timers on the nodes for
   - +6 h: n1's combd killed (`SIGKILL`; systemd starts it again),
   - +12 h: n2 cut off from the bucket for 10 minutes,
   - +18 h: n0 rebooted.
4. After 24 to 48 h, `./soak.sh stop`: load and sampling off, the samples
   fetched to `out/`, and the report (`report.py`).

**It passes if:** after the first hour, nothing grows without leveling off
(RSS, fds, threads, combd's count, the data folder, the journal); swap
stays flat; the only restarts are the chaos ones; and the outside probe
sees every page answer, but for seconds around each chaos event.
