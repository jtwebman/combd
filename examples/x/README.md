# x: an X clone on Postgres

Users, posts, follows, likes, and a home timeline, as a JSON API: one
stateless process (`api`), every row in your Postgres through a pool the
node runs (`[pool.main]`). No sign-in: a request names its user with `as`.
The routes are listed at the top of `src/lib.rs`. The tables make
themselves on the first request.

Run it here, against any Postgres:

```sh
cd examples/x
printf '[secrets]\npg-url = "postgres://user:pass@127.0.0.1:5432/x"\n' > combd.dev.toml
combd dev            # builds it, serves http://x.localhost:8080
./check.sh           # every route once, checked
```

`combd.dev.toml` holds the password: git ignores it (combd refuses one
git tracks).

It runs interpreted, as `combd dev` can. On a fleet, set `engine =
"compiled"` in `combd.toml`: a compiled cell waits on Postgres the same
way (`./check.sh` passes against it on the nodes' build, `aot-lite`).

Next: the same app on workerd, celld, and Phoenix, for the Postgres race.
