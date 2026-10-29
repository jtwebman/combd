# Security model

Security in combd is walls inside walls: a sandbox around every cell, a
host that charges each cell for what it asks, signed code, sealed
secrets, and a locked-down process around all of it.

## The sandbox

Every cell runs in a WebAssembly sandbox:

- **No system calls.** A cell has no files, no processes, and no network.
  It gets only the host's calls ([the host ABI](languages.md#the-host-abi))
  and a small slice of WASI. Anything else answers "not supported".
- **Its own memory only.** Every load and store is checked against the
  cell's memory.
- **No code of its own.** Wasm has no instruction that runs data, and a
  cell's memory is never executable. combd has no JIT. The interpreter
  runs modules as data, and builders build compiled modules ahead of
  time, as safe Rust with `forbid(unsafe_code)`. A compiler bug gives a
  wrong answer or a failed build, not memory unsafety.
- **A coarse clock**, rounded to 1 ms, against timing attacks.
- **Only its own app.** A cell can `call` only processes of its own app
  that list it in `callers`, and use only pools that list it. Objects,
  pools, env vars, and calls are all scoped to `<org>/<app>`.

The Bytecode Alliance's `wasmparser` checks every module before it runs.

## The host doesn't work for free

The real danger was the host doing work a cell doesn't pay for. Fuzzing
the host API found one cell could panic the host, or eat seconds of CPU
and gigabytes of memory and disk. Every host-side cost has a cap now:

- Host calls get 1 s a request in all.
- SQLite runs fenced, with no `ATTACH`, `PRAGMA`, `VACUUM INTO`,
  extensions, or the host's own tables. A statement gets 100 ms. Values,
  answers, and each database are capped
  ([Storage](storage.md#sql-from-a-cell)).
- Output, sends, and an event cell's work in flight are capped.
- A node takes no new objects when its disk is nearly full.

## HTTP

- Bodies come only with a `content-length`. `Transfer-Encoding` and two
  lengths that disagree are refused, so there's no request smuggling.
- A cell can set only the safe headers `content-type`, `cache-control`,
  `location`, `set-cookie`, `etag`, `last-modified`, `vary`, and
  `access-control-*`, with no line breaks, so there's no response
  splitting.
- Only standard methods reach a cell, so no client can fire an `ALARM`.
- Slow clients time out with `COMBD_READ_TIMEOUT_MS`, and sizes are
  capped ([Overload](overload.md#limits-per-request)).

## Admin tokens

- Tokens are stored as SHA-256 hashes, never as themselves, and compared
  in constant time. A bad token and a weak one get the same answer.
- The token is checked on the headers, before any of a body is read.
- Without TLS, the admin API listens on 127.0.0.1 only, so a token never
  crosses a network in the clear.
- Scopes limit each token to orgs, apps, and actions
  ([Deploying](deploying.md#admin-tokens)).
- The panel holds nothing. It asks with the token you give it, and builds
  the page with `textContent`.

## Secrets

Secrets are sealed in the catalog. A node opens them only in memory, for
an app's instances as they start.

- Each app has its own data key, made with its first secret and sealed
  by the fleet's master key. `combd master-key` makes the master key, and
  combd refuses a `COMBD_MASTER_KEY_FILE` others may read.
- Each secret is sealed by its app's data key with AES-256-GCM, with the
  app and the secret's name bound in. A sealed value copied to another
  name or app won't open.
- The bucket, and every node's copy of the catalog on disk, hold only
  sealed values.
- Secrets are write-only. Values come in on stdin, never the command
  line, and errors never show them.
- A deploy carries only the manifest, modules, and `static/`, so the
  local `combd.dev.toml` can't ride along. combd refuses it if git tracks
  it.

## Signed builds

A compiled module's build is native code a node will run. So a node loads
one only with an ed25519 signature from a builder in
`COMBD_BUILDER_KEYS`, over the build's hash, its module, its pin, and its
CPU. A build can't pass for another module, another combd, or another
CPU. A node checks a build on its disk again at each load, so anything
that could write that folder can't plant one for the next restart.

Builders hold no bucket credentials. A pull builder signs its requests,
and a push builder takes only jobs signed by a push key it lists. See
[Compiled modules and builders](builders.md).

## Signed releases

The arena's nodes run only a combd whose release note is signed by CI's
ed25519 release key, whose public half each node holds. The tarball must
match the SHA-256 in that note
([Running a fleet](fleet.md#signed-releases)).

## Between nodes

The wire is mutual TLS. Each node has a certificate for `node<N>.combd`
from the fleet's CA. A node takes a connection only from a client with a
certificate from that CA, and checks the node it dials against its exact
name. Without certificates, a node refuses to join a fleet, unless
`COMBD_WIRE_INSECURE=1` asks for plain TCP.

A peer's catalog push is only news. A node reads the bucket for the
truth.

## The process around it

On the arena's nodes, combd runs as its own user under systemd, with no
Docker. The unit,
[`combd.service`](https://github.com/jtwebman/combd/blob/main/examples/arena/cloud/combd.service),
takes away what combd doesn't need. It gets ports 80 and 443 and no
other capability, a read-only system but its data, and no memory both
writable and executable. It makes only the system calls a service
makes, and can run nothing but its own binary, its libraries, and its
builds.

## How it's been checked

There were three rounds of security review. Each finding was proven with
a failing test before its fix, and the next round attacked the fixes.
They covered the compiled path against ten years of Wasm runtime
exploits, the compiler and builder, the interpreter and host calls with
SQLite, the public edge and admin, and the fleet's wire and bucket.
Alongside them ran the official Wasm spec test suite, differential
fuzzing against Wasmtime, and fuzzing of the host API, the HTTP parser,
and the wire. What each round found and fixed is in `plan.md`,
"Security review, in rounds".

The guard pages are tested the way V8 tests its sandbox. A fault in the
running cell's memory must become a trap. Any other fault must still kill
the process: one in another cell's memory, in a paused cell's, or in no
cell's at all. Each of those tests was seen to fail against a wall broken
on purpose. The host fuzzer acts like a cell that can rewrite any of its
memory between calls. It plants addresses and lengths at the edges, so
what the host wrote back last time, and the pointers inside a request,
are hostile too.

## What it doesn't do (yet)

- **One compromised node is the whole fleet.** Every node holds the
  bucket's credentials, and mTLS proves "a fleet member", not which one.
- **No rules between tenants.** combd runs your own apps. Any org with a
  deploy token can claim any domain not taken, and there are no quotas
  per tenant. A service built on combd puts its own rules in the hooks in
  `combd/src/policy.rs`. `catalog_write` checks every deploy, var,
  secret, and removal, and `new_object` every new object. Both allow
  everything as shipped.
- **No bucket-write budget.** Each object made costs bucket writes, and
  storage forever. An app that lets anyone name objects pays for that.
- `/_combd/stats` on the main port isn't behind a token.
