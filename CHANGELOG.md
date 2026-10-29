# Changelog

## 0.1.0 (unreleased)

The first release: Durable Objects on your own machines, as one binary.

**Run an app**
- `combd new`, `combd dev`, `combd build`: an app from nothing to running
  in a few commands (`docs/src/quickstart.md`).
- `combd-sdk`, a Rust crate for apps: a request in, a response out
  (`cell!`), each durable object's own SQLite (`sql::run`, one
  transaction a request), Postgres through the node's pool (`pg::query`),
  alarms, calls between processes, and its panics in the node's log.
- `examples/x`: an X clone on Postgres (users, posts, follows, likes, a
  home timeline), with a check of every route.
- `combd.toml`: stateless processes, durable objects (one per id), a
  process per WebSocket, event cells, Postgres pools, vars, and secrets.
- Static files: an app's `static/` folder rides in its bundle and is
  served where its routes aren't (a folder's `index.html`, a `404.html`),
  from memory, with ETags and any `.br` or `.gz` beside a file. An app can
  be static files alone.
- Postgres pools as most pools work (PgBouncer's transaction mode,
  Postgrex, Rails): a pooled statement's connection isn't reset after it,
  so what it changes in the session (a `SET`) stays for the statements
  after it on that connection; undo it in the app. A session (`pg_open`)
  is reset when it ends.
- A pooled statement waits in a line per pool on each node, and whichever
  connection is free takes the next: no thread between them, so a big
  machine isn't held to what one bookkeeping thread can pass along.
  A session waiting for a whole connection goes first. The line
  (`work.rs`) knows nothing of Postgres: another database's pool can use
  it as it is.

**The runtime**
- WebAssembly cells on combd's own engine: an interpreter, or compiled
  ahead of time to machine code through safe Rust, built by builders and
  signed. No JavaScript engine.
- Preemptive, fair scheduling: a cell stuck in a loop costs its neighbors
  microseconds. A compiled cell waiting on Postgres or another process is
  paused, not blocking its loop. A cell that crashes restarts clean; its request rolls
  back.
- Sessions that sleep: an idle WebSocket costs about 2 KB in combd.
- An overload guard: past 80% of its memory a node reclaims, past 90% it
  refuses new connections (at TCP, so a browser tries another node), and
  it never refuses its own machine's checks.

**A fleet**
- Objects in your own bucket (S3, R2), owned by conditional write; writes
  confirmed by peers and uploaded in batches; a deploy reaches every node
  through the catalog.
- A dead node can be replaced by a new machine with its number and an
  empty disk: it knows its disk is new, and the owners it's a peer of start
  a new epoch it sees whole (`reseats` in stats). An owner is never its own
  peer, so with 2 peers a write is on three machines.
- Placement: objects move to the node their requests come in through
  (`nearest`), spread by hash (`spread`), or pinned to regions (`pin`);
  hooks for your own rules (`policy.rs`).
- mTLS between nodes; admin tokens; secrets sealed per app.
- Builders hold no bucket credentials: one asks any node's admin API for
  work and sends back what it built (`COMBD_FLEET`), each request signed
  with its builder key, which the nodes must list.
- Or pushed to: `combd builder serve` builds each job while its request
  waits, so it can scale to zero and build many at once (Cloud Run). The
  node that claims a job sends it (`COMBD_BUILDER_PUSH`), signed with the
  nodes' push key, which the builder must list; a push that fails is sent
  again on the next ask.
- `combd deploy` (and status, rollback, vars, secrets) to a node with a
  public certificate needs only `COMBD_TOKEN`: the fleet's CA is for a
  fleet with its own.
- Cheap on a bucket that bills per request: an idle node lists its bucket
  once every 30 s (alarms are looked for when one is due, or when a peer
  goes quiet), and an idle builder backs off to 30 s.

**Checked**
- TLA+ specs for ownership, replication, and the catalog; a deterministic
  simulator of crashes, splits, lost messages, and moves; the Wasm spec
  test suite; differential fuzzing; three rounds of security review, each
  finding proven with a failing test.
- The guard-page wall tested the way V8 tests its sandbox: a fault in the
  running cell's memory traps, and any other fault (another cell's memory,
  a paused cell's, no cell's) still kills the process. Each test was seen
  to fail against a deliberately broken wall. The host fuzzer rewrites the
  cell's memory between calls, with pointer-shaped values.

**Known limits** (more in `plan.md`)
- Apps in Rust for now (other languages that compile to Wasm work at the
  ABI level, without the SDK).
- One compromised node can act for the fleet: every node holds the
  bucket's credentials.
