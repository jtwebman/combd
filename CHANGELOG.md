# Changelog

## 0.1.0 (unreleased)

The first release: Durable Objects on your own machines, as one binary.

**Run an app**
- `combd new`, `combd dev`, `combd build`: an app from nothing to running
  in a few commands (`docs/quickstart.md`).
- `combd-sdk`, a Rust crate for apps: a request in, a response out
  (`cell!`), each durable object's own SQLite (`sql::run`, one
  transaction a request), alarms, calls between processes, and its panics
  in the node's log.
- `combd.toml`: stateless processes, durable objects (one per id), a
  process per WebSocket, event cells, Postgres pools, vars, and secrets.

**The runtime**
- WebAssembly cells on combd's own engine: an interpreter, or compiled
  ahead of time to machine code through safe Rust, built by builders and
  signed. No JavaScript engine.
- Preemptive, fair scheduling: a cell stuck in a loop costs its neighbors
  microseconds. A cell that crashes restarts clean; its request rolls
  back.
- Sessions that sleep: an idle WebSocket costs about 2 KB in combd.
- An overload guard: past 80% of its memory a node reclaims, past 90% it
  refuses new connections (at TCP, so a browser tries another node), and
  it never refuses its own machine's checks.

**A fleet**
- Objects in your own bucket (S3, R2), owned by conditional write; writes
  confirmed by peers and uploaded in batches; a deploy reaches every node
  through the catalog.
- Placement: objects move to the node their requests come in through
  (`nearest`), spread by hash (`spread`), or pinned to regions (`pin`);
  hooks for your own rules (`policy.rs`).
- mTLS between nodes; admin tokens; secrets sealed per app.
- Cheap on a bucket that bills per request: an idle node lists its bucket
  once every 30 s (alarms are looked for when one is due, or when a peer
  goes quiet), and an idle builder backs off to 30 s.

**Checked**
- TLA+ specs for ownership, replication, and the catalog; a deterministic
  simulator of crashes, splits, lost messages, and moves; the Wasm spec
  test suite; differential fuzzing; three rounds of security review, each
  finding proven with a failing test.

**Known limits** (more in `plan.md`)
- Apps in Rust for now (other languages that compile to Wasm work at the
  ABI level, without the SDK).
- One compromised node can act for the fleet: every node holds the
  bucket's credentials.
- The source still lives in `spike/`.
