"""The soak's report: per node, each measure's start, end, low, high, and
its growth an hour after the first hour (the warm-up); and the outside
probe's failures. Growth that doesn't level off is a leak."""
import csv, os, sys

d = sys.argv[1]
for f in sorted(x for x in os.listdir(d) if x.endswith(".csv")):
    rows = list(csv.DictReader(open(os.path.join(d, f))))
    if len(rows) < 3:
        print(f, "too few samples"); continue
    t0 = int(rows[0]["time"]); hours = (int(rows[-1]["time"]) - t0) / 3600
    print(f"{f[:-4]}: {len(rows)} samples over {hours:.1f} h")
    warm = [r for r in rows if int(r["time"]) - t0 >= 3600] or rows
    for k in ["rss_kb", "fds", "threads", "counted_mb", "available_mb", "swap_mb", "data_kb", "journal_mb", "restarts", "refused"]:
        v = [float(r[k] or 0) for r in rows]; w = [(int(r["time"]), float(r[k] or 0)) for r in warm]
        span = (w[-1][0] - w[0][0]) / 3600 or 1
        print(f"  {k:13} start {v[0]:>10.0f}  end {v[-1]:>10.0f}  low {min(v):>10.0f}  high {max(v):>10.0f}  after warm-up {(w[-1][1] - w[0][1]) / span:+.1f}/h")
p = os.path.join(d, "probe.log")
if os.path.exists(p):
    lines = [l.split() for l in open(p) if len(l.split()) == 3]
    bad = [l for l in lines if l[2] != "200"]
    print(f"probe: {len(lines)} checks, {len(bad)} not 200")
    for l in bad[:20]:
        print("  ", *l)
