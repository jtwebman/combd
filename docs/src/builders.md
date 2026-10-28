# Compiled modules and builders

combd runs a module one of two ways, and the manifest says which:

```toml
[modules.game]
engine = "compiled"      # or "interpreted" (the default)
```

- **Interpreted** runs on combd's own WebAssembly interpreter. It starts
  cells fast and runs anywhere, but it's slow at raw computing.
- **Compiled** turns the module into safe Rust, then into machine code
  for the node's CPU. The generated code has no `unsafe`, so the Rust
  compiler checks the sandbox holds.

A module runs only the way it asks, never switching. The same bytes,
compiled and interpreted, are two programs.

## Builds come from builders

Nodes don't compile. Builders, machines you run, make each build and sign
it. A node loads a build only if a builder in `COMBD_BUILDER_KEYS` signed
it. The signature covers the build, its module, its pin, and its CPU, so
no build can pass for another.

- A build is for a **pin**, a short hash of what it must match: the Rust
  compiler, combd's compiled runtime, and the target. A new combd with a
  new pin needs new builds.
- A build is for a **CPU**: `x86-64`, `x86-64-v2`, `-v3`, or `-v4`, by
  the features the node has, or `generic` elsewhere. A build for a level
  runs on that level and above.
- A module is named by the SHA-256 of its bytes.

Builds go through the bucket, under `_combd/builds/<pin>/<cpu>/`, where
each node notes its pin and CPU, so a deploy knows what to build.

**A deploy waits for its builds.** When a deploy has a compiled module
that isn't built for every kind of node in the fleet, it asks for the
builds and answers `"building": true`. It goes live once they're all
there, and fails if a build fails or after an hour. `GET /v1/deploys`
and the panel show what's waiting and which builders were seen lately.

**An update waits for its builds too.** Before a node moves to a new
combd, `combd builds-ready`, run with the node's own settings, exits 0
only once the new combd's builds of every compiled module the fleet runs
are in the bucket. Until then it asks for the missing ones and exits 1,
or 2 if it can't tell. The arena's `combd-update.sh` runs it, while the
old combd serves.

**A builder builds for its own combd only.** Move it before the nodes.
Until then, nodes on the new combd wait for builds. A late builder
delays an update but doesn't take an app down.

Builders serve a fleet. On a single node with `serve-apps`, keep modules
interpreted.

## A builder's key

```sh
combd builder-key builder.key
# a builder key is in builder.key; for COMBD_BUILDER_KEYS: 3b6a...
```

The private half stays on the builder and signs every build, and every
request of a pull builder. The public half goes in a file on every node,
named by `COMBD_BUILDER_KEYS`, one hex key a line, with `#` comments.

A builder holds no bucket credentials and never sees the bucket.

## Pull: `combd builder`

A pull builder asks any node's admin API for work, builds it, and sends
it back. It signs each request's path and body with its builder key,
within 5 minutes of the node's clock. A node refuses a key it doesn't
list before reading the body.

```sh
COMBD_FLEET=https://arena.combd.dev:9443 \
COMBD_BUILDER_ID=box-1 \
COMBD_BUILDER_KEY=/keys/builder.key \
COMBD_BUILD_WORK=/work \
combd builder
```

| Setting | What it is |
|---|---|
| `COMBD_FLEET` | Any node's admin API, or the fleet's name: `https://<name>:9443`. Required. |
| `COMBD_BUILDER_KEY` | Its key file, default `builder.key`. |
| `COMBD_BUILDER_ID` | Its name in `/v1/deploys`, default `$HOSTNAME`. |
| `COMBD_BUILD_WORK` | Its cargo folder, kept between builds, so combd's runtime compiles once. |
| `COMBD_AOT_SRC` | The `combd/aot` sources to build against, default the ones it was built from. |

An idle builder asks every 2 s at first, doubling to every 30 s, since
each ask costs a bucket listing. A builder that goes quiet for 15 minutes
loses its claim on a job, and another takes it.

A builder runs the Rust compiler, so it needs the toolchain combd was
built with, in `rust-toolchain.toml`, and the matching `combd/aot`
sources. The builder image has both. The arena's builder is any Linux
machine with Docker:

```sh
docker run -d --name combd-builder --restart unless-stopped --env-file ~/combd-builder/env \
  -v ~/combd-builder/builder.key:/keys/builder.key:ro -v combd-build-work:/work \
  ghcr.io/jtwebman/combd-builder:latest /combd/combd/target/release/combd builder
```

`~/combd-builder/env` holds `COMBD_FLEET`, `COMBD_BUILDER_ID`,
`COMBD_BUILDER_KEY=/keys/builder.key`, and `COMBD_BUILD_WORK=/work`.
Watchtower moves it to a new image when one is out.

## Push: `combd builder serve`

A push builder waits for work instead of asking. The node that claims a
job sends it, and the builder builds it while the request waits, one
build a request. So on a host like Cloud Run, with one request an
instance, it builds many at once and scales to zero.

The nodes sign each push with a push key the builder lists. Nothing is
shared. The nodes hold the push key, and the builder its builder key.

```sh
combd builder-key push.key      # its public half goes in the builder's COMBD_PUSH_KEYS
```

On every node:

| Setting | What it is |
|---|---|
| `COMBD_BUILDER_PUSH` | The builder's URL, `https://...`. |
| `COMBD_PUSH_KEY` | The push key file. |

On the builder:

| Setting | What it is |
|---|---|
| `COMBD_PUSH_KEYS` | A file of the push keys' public halves. Required. |
| `COMBD_BUILDER_KEY` | Its builder key, which signs the builds. |
| `COMBD_BUILD_WORK`, `COMBD_AOT_SRC` | As for a pull builder. |
| `PORT` | Where it listens, default 8080, in plain HTTP. The host in front ends TLS. |

It takes `POST /build`, signed by a push key it lists. A push for another
pin gets `409` and is sent again later, not failed. A push that fails or
times out after 14 minutes lets its claim go, and the next ask sends it
again. A node that dies mid-push leaves a claim that goes stale in 15
minutes. One node pushes at most 4 builds at once, and the same job no
sooner than 30 s after its last push.

On Cloud Run:

```sh
gcloud run deploy combd-builder --image <combd-builder, in Artifact Registry> \
  --command /combd/combd/target/release/combd --args builder,serve \
  --concurrency 1 --timeout 15m --min-instances 0 --max-instances 4 --cpu 4 --memory 8Gi \
  --allow-unauthenticated \
  --set-env-vars COMBD_BUILDER_KEY=/keys/builder/key,COMBD_PUSH_KEYS=/keys/push/keys,COMBD_BUILD_WORK=/tmp/work \
  --set-secrets /keys/builder/key=combd-builder-key:latest,/keys/push/keys=combd-push-keys:latest
```

`--allow-unauthenticated` lets the nodes reach it. The push signature
lets them in. Each cold instance compiles combd's runtime once before its
first build, so the first builds after a scale-up are slow.

Pull and push can run at once, since they claim jobs the same way.
