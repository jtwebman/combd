# CLI reference

`combd help` prints this:

```text
combd: Durable Objects on your own machines.

An app:
  combd new <name>               a new app in ./<name> (Rust; combd.toml says what runs)
  combd dev [dir] [port]         build it and serve it here (default ., 8080)
  combd build [dir]              build it: its crate for wasm32, into modules/

A node, or a fleet (see examples/arena/cloud for a whole one):
  combd serve-apps <engine> <root> <port>   every app under <root>/<org>/<app>/
  combd serve-fleet <engine> <port>         a fleet node (COMBD_* settings)
  combd builder [serve]                     make compiled modules' builds for a fleet (serve: pushed to it)
  engine: interp, aot (compiled), or both (each module as its manifest says)

Deploying (to a node's admin port, with COMBD_TOKEN):
  combd deploy <dir> <org> <url>            combd status <org/app> <url>
  combd rollback <org/app> <version> <url>  combd remove <org/app> <url>
  combd vars <org/app> <url> [name=value]   combd secret put|rm <org/app> <name> <url>

Keys and certificates:
  combd master-key <file>   combd builder-key <file>   combd admin-token <scope>...
  combd wire-certs <dir> <nodes>

combd version    combd also has its own benchmarks, simulators and fuzzers.
```

An unknown command prints the help and exits 2. A command that fails
prints why and exits 1.

## An app

### `combd new <name>`

Makes `./<name>/` with:

- `combd.toml`, with a page on `/` in `[http.web]`, and a counter on
  `/counter/:id/*` in `[durable.counter]`
- `Cargo.toml`, using `combd-sdk`
- `src/lib.rs`
- `.gitignore`, for `target/`, `modules/`, and `data/`

The name, in lowercase letters, digits, `-` and `_`, names the package,
the module, and the folder.

### `combd build [dir]`

Runs `cargo build --release --target wasm32-unknown-unknown` in `dir`,
default `.`, and copies the result to `modules/<package>.wasm`.

### `combd dev [dir] [port]`

Builds the app, then serves it interpreted on `port`, default 8080. Data
goes in `./data`, or `COMBD_DATA`. Vars and secrets come from
`combd.dev.toml`. It doesn't rebuild on its own.

## A node

### `combd serve-apps <engine> <root> <port>`

Serves every app under `<root>/<org>/<app>/` on one node, with D1
durability. The org is the folder an app is in. Each app's vars and
secrets come from its `combd.dev.toml`. One mistake in any app, and
nothing starts. The engine is `interp`, `aot`, or `both`, as the help
says. There's no admin API. To change
an app, change its folder and restart.

### `combd serve-fleet <engine> <port>`

A fleet node. It serves what's deployed to its bucket, and the admin API
if `COMBD_ADMIN_PORT` is set. It needs at least `COMBD_S3`,
`COMBD_NODE`, and `COMBD_PEERS`. The engine is `interp` or `both`. See
[Running a fleet](fleet.md).

### `combd builder` and `combd builder serve`

A builder for a fleet's compiled modules. It pulls work from
`COMBD_FLEET`, or with `serve` takes pushed work on `$PORT`. See
[Compiled modules and builders](builders.md).

### `combd builds-ready`

Run it, with the node's own `COMBD_S3...` and `COMBD_DATA`, before a node
moves to this combd. It exits 0 if every compiled module the fleet runs
is built for it, 1 if not yet, asking for the missing builds, and 2 if it
can't tell.

### `combd bucket-check`

Runs the startup storage test on the bucket in `COMBD_S3`,
`COMBD_S3_BUCKET`, `COMBD_S3_KEY`, `COMBD_S3_SECRET`, and
`COMBD_S3_REGION`. It checks conditional writes hold, under a 16-way race
too.

## Deploying

These need `COMBD_TOKEN`, and for a fleet with its own CA,
`COMBD_ADMIN_CA` and `COMBD_ADMIN_NAME`. Each prints the admin API's JSON
answer. See [Deploying](deploying.md).

| Command | |
|---|---|
| `combd deploy <dir> <org> <url>` | Bundle the app in `dir` and make it live. |
| `combd status <org/app> <url>` | Its live version. |
| `combd rollback <org/app> <version> <url>` | Make an earlier version live. |
| `combd remove <org/app> <url>` | Take it down. |
| `combd vars <org/app> <url> [NAME=value]...` | Set vars. `NAME=` unsets. |
| `combd secret put <org/app> <name> <url>` | Set a secret, its value from stdin. |
| `combd secret rm <org/app> <name> <url>` | Delete a secret. |

## Keys and certificates

| Command | What it makes |
|---|---|
| `combd master-key <file>` | A new master key for a fleet's secrets, 64 hex characters, mode 600. It won't overwrite a file. Every node needs a copy, in `COMBD_MASTER_KEY_FILE`. |
| `combd builder-key <file>` | A new ed25519 key in `file`, mode 600. Prints the public half for `COMBD_BUILDER_KEYS`. Also makes push keys. |
| `combd admin-token <scope>...` | A new admin token, printed once, and its line for `COMBD_ADMIN_TOKENS`. |
| `combd wire-certs <dir> <nodes>` | A fleet CA, `ca.pem` and `ca.key`, and a certificate per node, `node<N>.pem` and `node<N>.key`, in `dir`. |

## `combd version`

Prints the version. So does `--version` or `-V`.

## The rest

combd also has benchmarks, simulators, and fuzzers, like `sim`, `catsim`,
and `fuzz`. They're for working on combd itself, and some need its source
tree.
