# The X race: an X clone on Postgres

`examples/x` (users, posts, follows, likes, a home timeline) on four
servers, against one Postgres, on one box:

| Server | The app | Its pool |
|---|---|---|
| combd | `examples/x`, compiled (the nodes' build) | combd's, in the process |
| Phoenix | `examples/x-phoenix` (Bandit, Plug, Postgrex) | Postgrex's, in the process |
| workerd, celld | `examples/x-workers` (postgres.js over `cloudflare:sockets`) | PgBouncer, in the same container (Hyperdrive's place) |
| `workerd-direct`, `celld-direct` | the same | none: a connection to Postgres per request |

```sh
bench/x/run.sh                                     # everything, pool 8
POOLS="32 200" SERVERS="combd phoenix" CONNS="100 1000" bench/x/run.sh
```

## Apples to apples

- **One Postgres** (16, 6 CPUs, 4 GB) and **one dataset** (`seed.sql`:
  10,000 users following 50 each, 200,000 posts, 500,000 likes), a fresh
  copy for every test. The same SQL in every port.
- **Each server in 1 CPU and 1 GB, its pool inside that.** combd's and
  Postgrex's pools are in their process; the Workers' PgBouncer is in
  their container, not beside it for free.
- **The same connections to Postgres:** `POOLS` sets combd's `max`,
  Postgrex's `pool_size`, and PgBouncer's `default_pool_size`. The sweep
  (8 to 200) found both combd and Phoenix at their best from 32; races run
  at 32 and at 200.
- **Plans made per statement** (`plan_cache_mode = force_custom_plan`):
  Postgrex and PgBouncer keep prepared statements, and Postgres's generic
  plan for the timeline is 4x slower (3.3 ms to 0.8 ms). Without it, the
  race measured the plan cache.
- **No pool resets between statements, anywhere,** and no query cache.
- **Every server passes `examples/x/check.sh`** before its load.
- **The load** (`load.lua`, wrk): random users, 80% home timelines, 10%
  posts, 8% likes, 2% follows; 10, 100, and 1,000 connections, 30 s after
  5 s of warm-up.

A Worker connects per request: a socket can't outlive its request on
Workers or celld. On Cloudflare that connection is to Hyperdrive, a pooler
beside the database; PgBouncer stands in for it here. celld has no
Hyperdrive, and workerd's local binding connects straight to the database.

Results: `results/`, and spike/RESULTS.md, "The X race on Postgres".
