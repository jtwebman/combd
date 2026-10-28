# Deploying

On a fleet, apps go up through a node's admin API. `combd deploy` sends
the app, and every node of the fleet picks it up through the catalog in
the bucket. Vars, secrets, rollbacks, and removals go the same way.

A single node with `serve-apps` has no admin API. Copy the app's folder
to the node and restart it, as in the
[Quickstart](quickstart.md#5-run-it-on-a-server).

## From the command line

```sh
export COMBD_TOKEN=combd_...                       # an admin token
combd deploy ./shop acme https://node.example.com:9443
# {"version":"3f9a..."}
combd status acme/shop https://node.example.com:9443
# {"version":"3f9a..."}
```

| Command | What it does |
|---|---|
| `combd deploy <dir> <org> <url>` | Bundle the app in `<dir>` and make it live as `<org>/<app>`, named by its manifest. |
| `combd status <org/app> <url>` | The live version. |
| `combd rollback <org/app> <version> <url>` | Make an earlier version live. |
| `combd remove <org/app> <url>` | Take the app down. |
| `combd vars <org/app> <url> [NAME=value]...` | Set vars. `NAME=` unsets one. |
| `combd secret put <org/app> <name> <url>` | Set a secret, its value from stdin. |
| `combd secret rm <org/app> <name> <url>` | Delete a secret. |

Each prints the API's JSON answer, or the error and exits 1. Org and app
names are lowercase letters, digits, `-` and `_`.

### Reaching the node

- **A node with a public certificate**, ending TLS itself with
  `COMBD_TLS_CERT`, needs only `COMBD_TOKEN`. combd checks the node's
  certificate against the public roots and the URL's host. So a CI
  anywhere can deploy with a token and a combd binary.
- **A fleet with its own CA**, the wire's certificates from
  `combd wire-certs`, also needs `COMBD_ADMIN_CA` set to its `ca.pem`,
  and `COMBD_ADMIN_NAME` to the node's name in it, default `node0.combd`.
- **On the node itself**, use `http://127.0.0.1:<port>`.

A URL with no port uses its scheme's, 443 for `https` and 80 for `http`.

```sh
# the arena's deploy, through node 0, with the fleet's CA
COMBD_TOKEN=$(cat .secrets/token) COMBD_ADMIN_CA=.secrets/certs/ca.pem COMBD_ADMIN_NAME=node0.combd \
  combd deploy ../app acme "https://n0.arena.combd.dev:9443"
```

### Secrets

```sh
printf %s "$DATABASE_URL" | combd secret put acme/shop pg-url https://node.example.com:9443
```

The value comes from stdin, never the command line, which other users and
shell history can see. One trailing newline is dropped. A secret is text,
at most 64 KB. Secrets are write-only. Nothing reads one back. The nodes
need the fleet's master key, `COMBD_MASTER_KEY_FILE`, to take them
([Security model](security.md#secrets)).

While a version is live, a secret must be one its manifest declares, and
valid for its type. To delete a secret it requires, first deploy a
version that doesn't. Vars are checked against their types the same way.
Text from the command line is read as the var's type, so `8` for an
`int` is 8, and `lots` is an error.

## What a deploy does

1. **The bundle.** `combd deploy` packs the manifest, `modules/*.wasm`,
   and `static/` ([Static files](static-files.md)). Nothing else in the
   folder goes up, so `combd.dev.toml` stays home. A bundle holds at most
   4096 files and 64 MB. Its version is the SHA-256 of its bytes, so
   deploying the same one twice is free.
2. **Checks.** The node unpacks it, reads the manifest, whose name must
   be the app's, resolves its vars and secrets for the fleet, and loads
   its modules. On any mistake nothing changes, and the error comes
   back. So set a new app's required vars and secrets
   first, then deploy.
3. **Live.** The version goes to the bucket, and the catalog makes it
   live. Every node swaps it in within moments. The catalog checks,
   against its newest state, that no two apps hold one domain.

A node that can't load a version keeps serving the one it has.

**Old code finishes.** Requests already running finish on the old
version. A durable object moves to the new code once it has no request
running, no open sockets, and no tick. So a deploy never ends a game in
play. An object that keeps sockets open stays on the old version until
they close.

**Compiled modules.** If the manifest asks for a compiled module whose
builds aren't ready, the deploy answers
`{"version": "...", "building": true}` and waits. The node asks builders
for the builds and tries again every couple of seconds. It makes the
version live once every kind of node in the fleet has one, and gives up
after an hour. A waiting deploy lives in that node's memory. If the node
restarts, deploy again. `GET /v1/deploys` shows what's waiting. See
[Compiled modules and builders](builders.md).

**Rollback** checks the old version again, since vars may have changed.
If its compiled builds are gone, deploy it again instead.

## Admin tokens

combd knows nothing about people, only tokens and what each may do. A
token's scopes look like `<org>/<app>:<action>`, and any part may be `*`:

| Action | Lets a token |
|---|---|
| `deploy` | deploy, roll back, and remove |
| `vars` | set vars |
| `secrets` | set and delete secrets |
| `read` | read an app's live version |

`acme/shop:deploy`, `acme/*:vars`, and `*/*:*` are all scopes. The
fleet-wide endpoints, `/v1/nodes` and `/v1/deploys`, take a `*/*:read` or
`*/*:*` token.

Make one:

```sh
combd admin-token 'acme/*:deploy' 'acme/*:vars'
# token (give it to your CI; it's shown once): combd_6b1f...
# line for the tokens file:                   9c2e...  acme/*:deploy acme/*:vars
```

The token is shown once. The line goes in the nodes' tokens file,
`COMBD_ADMIN_TOKENS`, one token a line, its SHA-256 then its scopes. `#`
starts a comment. Nodes keep only the hash, and compare in
constant time. A bad token and one that's too weak get the same answer.

## The admin API

A fleet node serves it on `COMBD_ADMIN_PORT`, with the tokens in
`COMBD_ADMIN_TOKENS`. With the wire's certificates in `COMBD_WIRE_CERTS`,
it speaks TLS and listens on every address. It shows the fleet's
certificate to a client that asks for a `*.combd` name, and the node's
public certificate, `COMBD_TLS_CERT`, to any other. Without TLS it
listens on 127.0.0.1 only, so a token never crosses a network in the
clear. The arena uses port 9443.

Every request but the panel carries `Authorization: Bearer <token>`. The
token is checked on the headers, before any of the body is read.

```text
PUT    /v1/apps/<org>/<app>                  a bundle: check it, upload it, make it live
GET    /v1/apps/<org>/<app>                  the live version
POST   /v1/apps/<org>/<app>/rollback         {"version"}: make an earlier one live
DELETE /v1/apps/<org>/<app>                  take it down
PUT    /v1/apps/<org>/<app>/vars             {"NAME": value, ...}: set vars (null unsets)
PUT    /v1/apps/<org>/<app>/secrets/<name>   the value (the body): set it
DELETE /v1/apps/<org>/<app>/secrets/<name>   delete it
GET    /v1/nodes                             every node's stats (a */*:read token)
GET    /v1/deploys                           deploys waiting for builds, and the builders (*/*:read)
POST   /v1/builds/next                       a builder's next job (signed by a builder key)
POST   /v1/builds/done                       what a builder built (signed by a builder key)
GET    /                                     the panel
```

Answers are JSON, the result with `200`, or `{"error": "..."}` with `400`
for a bad request or a failed deploy, `401` for no token, `403` for a
token that may not, or `404`.

**The panel**, `GET /` on the admin port, is a page that asks `/v1/nodes`
and `/v1/deploys` with a token you give it. It shows the nodes, their
stats, where each process's CPU goes, and deploys waiting for builds. The
page itself holds nothing.

`/v1/builds/*` are for builders, signed with a builder key instead of a
token. See [Compiled modules and builders](builders.md).
