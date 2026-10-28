# Running a fleet

A fleet is several combd nodes that share one bucket. The bucket decides
who owns each object. It holds every app's versions and the catalog. The
catalog says which version of each app is live, with its vars and its
sealed secrets. Nodes talk to each other over the wire, with mutual TLS.
Requests for an object go to its owner, and D3 writes go to its peers.

## Set up a fleet, step by step

These steps make a three-node fleet on Linux machines you already have.
They follow the arena's real setup ([below](#a-real-fleet-the-arena)).
Replace each `<...>` with your own value.

1. **Name the machines and open their ports.** Point `n0.<domain>`,
   `n1.<domain>`, and `n2.<domain>` at the three machines. Open 80 and 443
   to everyone, 7100 only between the nodes, and 9443 to wherever you
   deploy from. The nodes reach each other on 7100, and the admin API is
   on 9443.

2. **Get combd.** Take a release or build it, as in the
   [Quickstart](quickstart.md#1-install). You need it on your own machine
   to make keys and deploy, and the Linux binary on each node.

3. **Make a bucket and check it.** Make an S3 or R2 bucket, and a key that
   can read and write it. Then run the test every node runs at start:

   ```sh
   COMBD_S3=https://<account>.r2.cloudflarestorage.com COMBD_S3_BUCKET=<bucket> \
   COMBD_S3_KEY=<key> COMBD_S3_SECRET=<secret> COMBD_S3_REGION=auto \
     combd bucket-check
   ```

   A node refuses a bucket that fails it. For S3, use its endpoint and
   region in place of R2's.

4. **Make the fleet's keys** on your own machine, once, in a folder you
   keep private:

   ```sh
   mkdir -m 700 fleet && cd fleet && umask 077
   combd wire-certs certs 3
   combd master-key master.key
   OUT=$(combd admin-token '<org>/*:*' '*/*:read')
   echo "$OUT" | sed -n 's/.*: \(combd_[0-9a-f]*\)$/\1/p' > token
   echo "$OUT" | sed -n 's/^line for the tokens file: *//p' > tokens
   ```

   `certs` is the wire's CA and a certificate per node. `master.key` seals
   the apps' secrets. `token` is for you to deploy with, and the nodes get
   only its hash, in `tokens`. The `*/*:read` scope lets it read
   `/v1/nodes`. See [What you need](#what-you-need).

5. **Make a builder key, only if an app runs compiled modules.** On the
   builder, `combd builder-key builder.key` prints its public half. Put
   that key in a file named `builder-keys`. Nodes load only builds signed
   by a key in it. Then run the builder, as in
   [Compiled modules and builders](builders.md).

6. **Get a TLS certificate** for the three node names and your apps'
   domains, from any CA. The arena uses Let's Encrypt. Save the chain as
   `cert.pem` and its key as `key.pem`. combd ends HTTPS itself with them
   ([TLS in combd](#tls-in-combd)).

7. **On each node, add a user and install combd:**

   ```sh
   sudo useradd --system --home-dir /var/lib/combd --shell /usr/sbin/nologin combd
   sudo mkdir -p /var/lib/combd /etc/combd/certs /etc/combd/tls /opt/combd/current/combd/target/release
   sudo chown combd:combd /var/lib/combd
   sudo install -m 755 combd /opt/combd/current/combd/target/release/combd
   ```

   The unit in step 10 runs combd from that path, as that user, and lets
   it write only `/var/lib/combd`.

8. **Copy node N's files to it:**

   - `certs/ca.pem`, `certs/node<N>.pem`, and `certs/node<N>.key` to
     `/etc/combd/certs/`
   - `master.key` and `tokens` to `/etc/combd/`
   - `cert.pem` and `key.pem` to `/etc/combd/tls/`
   - `builder-keys` to `/etc/combd/`, if you made one

   Then make its secrets combd's own:

   ```sh
   cd /etc/combd
   sudo chown combd:combd tokens master.key certs/node<N>.key tls/key.pem
   sudo chmod 600 tokens master.key certs/node<N>.key tls/key.pem
   ```

   combd refuses a master key that others may read. Never copy
   `certs/ca.key` to a node.

9. **Write `/etc/combd/env`** on node N, mode 600:

   ```sh
   COMBD_NODE=<N>
   COMBD_PEERS=0=n0.<domain>:7100,1=n1.<domain>:7100,2=n2.<domain>:7100
   COMBD_PUBLIC_URLS=0=https://n0.<domain>,1=https://n1.<domain>,2=https://n2.<domain>
   COMBD_WIRE_CERTS=/etc/combd/certs
   COMBD_DATA=/var/lib/combd
   COMBD_REPLICAS=2
   COMBD_S3=https://<account>.r2.cloudflarestorage.com
   COMBD_S3_BUCKET=<bucket>
   COMBD_S3_KEY=<key>
   COMBD_S3_SECRET=<secret>
   COMBD_S3_REGION=auto
   COMBD_ADMIN_PORT=9443
   COMBD_ADMIN_TOKENS=/etc/combd/tokens
   COMBD_MASTER_KEY_FILE=/etc/combd/master.key
   COMBD_TLS_CERT=/etc/combd/tls/cert.pem
   COMBD_TLS_KEY=/etc/combd/tls/key.pem
   COMBD_REDIRECT_PORT=80
   COMBD_BUILDER_KEYS=/etc/combd/builder-keys
   ```

   Only `COMBD_NODE` differs between nodes. Leave out
   `COMBD_BUILDER_KEYS` if you made no builder key. `COMBD_DATA` must be
   `/var/lib/combd`, the one folder the unit lets combd write. The arena's
   whole file is in [Starting a node](#starting-a-node), and every
   setting is in [Settings](#settings).

10. **Install the systemd unit on each node.** Copy
    [`examples/arena/cloud/combd.service`](https://github.com/jtwebman/combd/blob/main/examples/arena/cloud/combd.service)
    to `/etc/systemd/system/combd.service`. It runs
    `combd serve-fleet both 443` as the `combd` user, with the settings in
    `/etc/combd/env`, and restarts it if it stops.

11. **Start each node:**

    ```sh
    sudo systemctl daemon-reload
    sudo systemctl enable --now combd
    journalctl -u combd -f
    ```

    The log shows the bucket test and any setting combd refuses. A node
    won't start with a node number another running node uses.

12. **Check the fleet** from your own machine:

    ```sh
    curl -s https://n0.<domain>/_combd/ping
    curl -s https://n0.<domain>/_combd/stats
    curl -s -H "authorization: Bearer $(cat token)" https://n0.<domain>:9443/v1/nodes
    ```

    Ping answers `{}`. `/v1/nodes` lists every node's stats, and a node
    that doesn't answer says why. See [Stats and monitoring](stats.md).

13. **Deploy an app:**

    ```sh
    COMBD_TOKEN=$(cat token) combd deploy ./<app> <org> https://n0.<domain>:9443
    ```

    The nodes have a public certificate, so the token is all you need.
    Every node picks the app up from the bucket. See
    [Deploying](deploying.md).

## What you need

- **A bucket** that speaks S3 and honors conditional writes, such as S3
  or R2. Before it serves, every node races 16 writers on one conditional
  write, and refuses the bucket unless exactly one wins.
  `combd bucket-check` runs that test on its own, with the same
  `COMBD_S3...` settings.
- **The wire's certificates**, a CA and one certificate per node.
- **A master key** for the apps' secrets, the same on every node.
- **Admin tokens**, for `combd deploy`.
- Builders, if any app runs compiled modules
  ([Compiled modules and builders](builders.md)).

Make the keys once, somewhere safe:

```sh
combd wire-certs certs 3          # certs/ca.pem, ca.key, node0.pem, node0.key, ... node2
combd master-key master.key       # 64 hex characters; give every node a copy
combd admin-token 'acme/*:*'      # a token for your CI, and the line for the nodes' tokens file
```

Each node gets `ca.pem` and its own `node<N>.pem` and `node<N>.key`. Keep
`ca.key` off the nodes. combd refuses a master key file that others may
read, so `chmod 600` it.

## Starting a node

```sh
combd serve-fleet both 443
```

The engine is `interp`, which interprets every module, or `both`, which
runs each as its manifest says. `both` needs a combd built with the
compiled tier, as the releases are. Everything else comes from
the environment. A node of the arena's fleet has this, with your values
in `<...>`:

```sh
COMBD_NODE=0
COMBD_PEERS=0=n0.arena.combd.dev:7100,1=n1.arena.combd.dev:7100,2=n2.arena.combd.dev:7100
COMBD_REGIONS=0=nyc3,1=fra1,2=sgp1
COMBD_PUBLIC_URLS=0=https://n0.arena.combd.dev,1=https://n1.arena.combd.dev,2=https://n2.arena.combd.dev
COMBD_WIRE_CERTS=/etc/combd/certs
COMBD_REPLICAS=2
COMBD_THREADS=1
COMBD_DATA=/var/lib/combd
COMBD_LEASE_MS=3000
COMBD_S3=https://<account>.r2.cloudflarestorage.com
COMBD_S3_BUCKET=combd-arena
COMBD_S3_KEY=<key>
COMBD_S3_SECRET=<secret>
COMBD_S3_REGION=auto
COMBD_ADMIN_PORT=9443
COMBD_ADMIN_TOKENS=/etc/combd/tokens
COMBD_MASTER_KEY_FILE=/etc/combd/master.key
COMBD_READ_LEASE=1
COMBD_TLS_CERT=/etc/combd/tls/cert.pem
COMBD_TLS_KEY=/etc/combd/tls/key.pem
COMBD_REDIRECT_PORT=80
COMBD_MEMORY_BUDGET=550M
COMBD_BUILDER_KEYS=/etc/combd/builder-keys
```

On start, a node tests the bucket and catches up on the catalog. It gives
up after 60 s if it can't reach the bucket. It loads every live app, and
refuses to start with a node number another running node is using. Then
it serves, and swaps in whatever the catalog says changed.

A node refuses a wire with no certificates, unless
`COMBD_WIRE_INSECURE=1` asks for plain TCP, with a warning in the log.

## TLS in combd

A node can end TLS itself, with no proxy in front. `COMBD_TLS_CERT` is
the chain and `COMBD_TLS_KEY` the key, both PEM files.
`COMBD_REDIRECT_PORT=80` opens a port that only redirects to HTTPS. combd
checks the certificate's file every few seconds and loads a new one for
new connections, so a renewal needs no restart. Replace the key first,
then the certificate. With a public certificate on the node,
`combd deploy` needs only a token
([Deploying](deploying.md#reaching-the-node)).

## Placement

In a fleet, each durable process says where its objects live, with
`placement` in the manifest:

| `placement` | An object's home |
|---|---|
| `nearest` (default) | The node most of its requests come in through. |
| `first` | Wherever it started. It doesn't move. |
| `spread` | Its hash among the nodes in `scope` of the node that asked. `scope` is `zone`, `region`, or `fleet`. |
| `pin` | A node in the regions it names, such as `pin = "fra1,ams3"`, or an area. |

- `COMBD_REGIONS` says where each node is, as a region and, after a
  slash, a zone: `0=nyc3/a,1=fra1/a`. Every node not listed shares one
  region.
- `COMBD_AREAS` names sets of regions for `pin`, such as
  `eu=fra1+ams3,us=nyc3`.
- If none of a pinned object's regions is up, `fallback = "nearest"` runs
  it elsewhere until one is back. `fallback = "none"` refuses it with 503
  until then, so its data never leaves them.

Once a window, each owner works out each object's home and offers the
object to it. The window is `COMBD_PLACE_MS`, 30 minutes, and 0 means
objects never move. `nearest` moves an object only when one node sent it
at least 20 requests and two thirds of them, two windows running, so
nothing bounces. The target may say no. It takes at most 16 objects a
window, and none while it's under memory pressure, refusing connections,
or has less than 30% of its machine's memory free. A refused object stays
and asks again next window.

A fork can put its own rules in `home` in `combd/src/policy.rs`, which has
the last word.

## Clients picking a node

Every node answers, on its main port:

- `GET /_combd/nodes` gives `{"me", "nodes", "version"}`, with every
  node's public URL from `COMBD_PUBLIC_URLS`, so a page can time each one.
- `GET /_combd/ping` gives `{}`, to time a round trip.

Both allow any origin. The arena's page times every node this way, and
its matchmaker starts a match on the node best for both players.

`GET /v1/nodes` also uses `COMBD_PUBLIC_URLS`, to gather every node's
stats ([Stats and monitoring](stats.md)).

## A real fleet: the arena

[`examples/arena/cloud/`](https://github.com/jtwebman/combd/tree/main/examples/arena/cloud)
runs the arena and One Million Checkboxes on three 1 vCPU, 1 GB droplets
in New York, Frankfurt, and Singapore, over an R2 bucket, set up with
Terraform.
[`examples/arena/README.md`](https://github.com/jtwebman/combd/blob/main/examples/arena/README.md)
walks through it. The parts worth copying:

- **`up.sh`** makes the fleet's secrets once, with `wire-certs`,
  `admin-token`, and `master-key`. It runs Terraform, waits for each
  node's `/_combd/ping`, and deploys. **`deploy.sh`** deploys again after
  a change.
- **`cloud-init.tftpl`** puts each node's files in `/etc/combd`, with its
  secrets 600 and owned by the `combd` user. It adds a `combd` system
  user, 1 GB of swap, and a cron job for updates, and starts combd.
- **`combd.service`** runs `combd serve-fleet both 443` under systemd as
  the `combd` user, with no Docker, `Restart=always`, and
  `LimitNOFILE=1048576`. It locks combd down, as in
  [Security model](security.md#the-process-around-it). It sets
  `MALLOC_MMAP_THRESHOLD_` and `MALLOC_MMAP_MAX_`, so glibc keeps cells'
  buffers as lazy zero pages.
- **`combd-tune.sh`** runs at every boot, before combd. It turns off
  connection tracking for ports 80 and 443, and the Ubuntu services a
  node doesn't need.
- **`combd-update.sh`** runs every minute from cron and moves the node to
  a new combd, as below.
- **`tls.sh`** gives every node a renewed certificate, which combd picks
  up within seconds.
- **`roll.sh`** moves every node now, one at a time, each only once the
  last answers again.

combd raises its open-files limit to the ceiling. It keeps connections
under three quarters of it, and under `COMBD_MAX_CONNS`, so a full node
answers instead of running out of files.

### Signed releases

The arena's nodes fetch combd from a GitHub release that CI replaces on
every push to `main`. Nothing unsigned runs:

1. CI writes a note, `combd-arena.txt`, with the source hash, the
   tarball's SHA-256, and the commit, and signs it with the ed25519
   release key. The private key is a CI secret. Each node holds the
   public half, `release.pub`.
2. `combd-update` checks the note's signature, then the tarball's
   SHA-256 against the note. It ignores a note that doesn't check out.
3. Before it moves, it runs the new combd's `combd builds-ready`, which
   asks for its builds of every compiled module, and exits 0 only once
   they're all in the bucket. Moving sooner would take those
   apps down on this node.
4. Node `n` moves once the release has been out `n` x 2 minutes, so the
   nodes restart one at a time. It waits while the node has open
   WebSockets, up to 15 minutes past its turn, so it doesn't cut off a
   game in play.
5. It keeps this version and the one before.

While a node restarts, for a few seconds, it refuses new connections, and
browsers try another of the name's addresses.

**A new combd reads bundles an older one deployed.** It meets them in the
bucket through every update. The other way round, nodes still on an old
combd refuse a bundle it can't read. So a new format ships in two steps,
a combd that reads it, then one that writes it.

## Settings

Every setting is an environment variable. Times are in ms unless they say
otherwise.

### Any node

| Setting | Default | What it does |
|---|---|---|
| `COMBD_DATA` | `data` | Where objects, the catalog's copy, and builds live. |
| `COMBD_THREADS` | the CPU count | Event loops. |
| `COMBD_IDLE_MS` | 60000 | An object idle this long leaves memory. |
| `COMBD_MAX_OBJECTS` | 10000 | Objects in memory, per loop. |
| `COMBD_MAX_CONNS` | 200000 | Connections open at once, and at most three quarters of the open-files limit. |
| `COMBD_MAX_AWAKE` | 1000 | Cells at work at once, per loop ([Overload](overload.md)). |
| `COMBD_MEMORY_BUDGET` | the container's limit, else the RAM | What the node may use, in bytes, or `512M`, `4G`. |
| `COMBD_TLS_CERT`, `COMBD_TLS_KEY` | none | End TLS on the main port. PEM files. |
| `COMBD_REDIRECT_PORT` | none | A port that only redirects to HTTPS. |
| `COMBD_MAX_BODY` | 1 MiB | A request's body. Past it, 413. |
| `COMBD_MAX_HEADER_BYTES` | 16 KiB | The request line and headers. Past it, 431. |
| `COMBD_MAX_HEADERS` | 64 | Headers in a request. Past it, 431. |
| `COMBD_MAX_WS_MESSAGE` | 1 MiB | A WebSocket message. Past it, close 1009. |
| `COMBD_READ_TIMEOUT_MS` | 30000 | A request must arrive within this, and a new connection start one. Past it, 408. |
| `COMBD_KEEPALIVE_MS` | 75000 | A kept connection with nothing to do. |
| `COMBD_TLS_HANDSHAKE_MS` | 10000 | A TLS handshake. |
| `COMBD_CPU_LIMIT_TICKS` | 100000000 | Loop iterations one call of a compiled cell may run before it's killed, about 100 ms of tight loop. |
| `COMBD_EV_MAX` | 32 | Requests per event-cell instance, for a node with no manifest. With a manifest, `max_concurrent` sets it. |
| `COMBD_AOT_DIR` | none | A folder of compiled builds, `<hash>.so`, that a node loads beside those built in. |

### A fleet node

| Setting | Default | What it does |
|---|---|---|
| `COMBD_S3` | none | The bucket's endpoint, `https://...`, or `http://` for a local one. Setting it makes a fleet node. |
| `COMBD_S3_BUCKET` | `combd` | The bucket. |
| `COMBD_S3_KEY`, `COMBD_S3_SECRET` | none | Its credentials. |
| `COMBD_S3_REGION` | `us-east-1` | Its region. R2 uses `auto`. |
| `COMBD_NODE` | none | This node's number. |
| `COMBD_PEERS` | none | Every node's wire address: `0=host:7100,1=host:7100,...`. |
| `COMBD_WIRE_PORT` | 7100 | Where this node listens for peers. |
| `COMBD_WIRE_CERTS` | none | The folder from `combd wire-certs`. |
| `COMBD_WIRE_INSECURE` | off | `1` for plain TCP between nodes. |
| `COMBD_LEASE_MS` | 10000 | How long a silent node keeps its objects, renewed every third of it. |
| `COMBD_REPLICAS` | 0 | Peers per object, for D3. 0 is D2 ([Storage](storage.md)). |
| `COMBD_UPLOAD_MS` | 10000 | With peers up, how long an object's uploads may wait. It warns above 5 minutes. |
| `COMBD_UPLOAD_BYTES` | 1 MB | ...or until this much builds up, at most 16 MB. |
| `COMBD_READ_LEASE` | off | `1`: owners answer reads with no round, while their peers' promises hold. Only once every node runs a version that promises. |
| `COMBD_RESTORES_AT_ONCE` | 4 | Objects restored from the bucket at once, per loop. |
| `COMBD_REGIONS` | none | Where each node is: `0=nyc3/a,1=fra1/a`. |
| `COMBD_AREAS` | none | Named sets of regions: `eu=fra1+ams3,us=nyc3`. |
| `COMBD_PLACE_MS` | 1800000 | The placement window. 0 means objects never move. |
| `COMBD_PUBLIC_URLS` | none | Every node's public URL: `0=https://a,1=https://b`. |
| `COMBD_MASTER_KEY_FILE` | none | The master key, to open and seal secrets. |
| `COMBD_ADMIN_PORT` | none | Serve the admin API on this port. |
| `COMBD_ADMIN_TOKENS` | none | The tokens file. Required with `COMBD_ADMIN_PORT`. |
| `COMBD_BUILDER_KEYS` | none | A file of builder public keys this node trusts. |
| `COMBD_BUILDS_DIR` | `<data>/builds` | Where fetched builds are kept. |
| `COMBD_BUILDER_PUSH` | none | A push builder's URL. |
| `COMBD_PUSH_KEY` | none | The key file pushes are signed with. |

### The command line and builders

| Setting | Used by | What it does |
|---|---|---|
| `COMBD_TOKEN` | `deploy`, `status`, ... | The admin token. |
| `COMBD_ADMIN_CA` | `deploy`, `status`, ... | The fleet's `ca.pem`, for a fleet with its own CA. |
| `COMBD_ADMIN_NAME` | `deploy`, `status`, ... | The node's name in that CA, default `node0.combd`. |
| `COMBD_FLEET` | `builder` | Any node's admin API: `https://<name>:9443`. |
| `COMBD_BUILDER_KEY` | `builder` | Its key file, default `builder.key`. |
| `COMBD_BUILDER_ID` | `builder` | Its name, default `$HOSTNAME`, else `builder`. |
| `COMBD_AOT_SRC` | `builder` | The `combd/aot` sources it builds against, default the ones it was built from. |
| `COMBD_BUILD_WORK` | `builder` | Its cargo folder, kept between builds. |
| `COMBD_PUSH_KEYS` | `builder serve` | A file of the nodes' push public keys. Required. |
| `PORT` | `builder serve` | Where it listens, default 8080. |

The other `COMBD_` names in the source are for combd's own tests and
benchmarks.
