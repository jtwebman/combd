# Arena

A real-time 1 vs 1 game, built to push combd as hard as we could and to show
it can be done inside it: matchmaking by measured latency, a match running
20 ticks a second on the node best for both players, WebSockets, line of
sight, pauses, drops and reconnects, and durable player records.

**It's not trying to be a good game engine.** Real games use UDP, client-side
prediction, and lag compensation; this one keeps it simple (the server
decides everything, 20 times a second) so the test is combd, not netcode.

## What it uses

| Piece | combd feature |
|---|---|
| `lobby` | a worker: the page (served with its own `content-type`), the join and poll API |
| `matchmaker` | one Durable Object: pairs players whose round trips to some node are both low, and starts the match there (`call` with `"node"`) |
| `match` | a Durable Object per match: `tick_every(50)` (in memory, never written to disk), WebSockets, the game state in memory |
| `player` | a Durable Object per player: wins and losses, in its SQLite, from the match's `send` |
| the page | `/_combd/nodes` and `/_combd/ping`, to time every node from the browser |

Line of sight is the server's job: a player's view leaves out the other
player (and bullets) whenever a wall is in between, so there's nothing for a
client to cheat its way to. Either player may pause; a dropped connection
pauses too. Back within `PAUSE_MS` (30 s) and the match goes on; otherwise
the other player wins. A match lasts `MATCH_MS` (3 minutes) of play: then
more kills wins, and a tie is a draw.

What's durable and what isn't, plainly: the records are; a match's state
lives in memory only. If its node dies mid-match, that match is lost.

## Run it

```sh
./build.sh                                     # the game module
cd ../../combd && cargo run --release -- serve-app interp ../examples/arena/app 8080
```

Then open http://localhost:8080 in two browser windows.

## Bots

`bots/` plays over the same HTTP and WebSockets a browser uses, and sees
only what the server sends (so it has to search for you around walls).
Four strategies: `chaser` (heads for where you were last seen), `sniper`
(keeps about 400 away and strafes), `camper` (hides in cover, peeks every
few seconds), `random`. A bot plays match after match, so a person who
joins gets matched with one.

```sh
cd bots && cargo run --release -- http://localhost:8080 8 mix      # 8 bots, all four kinds
```

`ARENA_SPREAD=40` pretends the bots live around the world (each has a home
node; every node away from home adds 40 ms to what it measured), so matches
land on different nodes even with every bot on one machine.

## Three nodes, with distance

`./demo.sh` (Docker) starts a Silo bucket and three combd nodes with
pretend distance between them (node 0 near, node 1 40 ms away, node 2
100 ms, using `tc`), deploys the arena through node 0's admin API over TLS,
and prints where to go. `./demo.sh down` stops it all.

## What a run looks like

8 bots, `ARENA_SPREAD=40`, on the three-node demo (pretend distance 0, 40,
and 100 ms), a laptop shared with other work, 75 s: 20,438 ticks seen by
players; 1.43% of ticks came more than 75 ms after the last (the beat is
50 ms), the worst gap 271 ms. 21% of a player's ticks had no new input from
them, because it arrived a few ms after the tick: the server reuses the
last input, so play goes on. (A real game holds inputs a tick or two to
smooth that; this one keeps it simple.) Matches went to more than one node,
each pair's worst round trip 2 to 81 ms.

Bugs it found, in the game: waiting players were paired only when someone
new joined (they could wait out their ticket); a match had no time limit
(two campers could hide forever); the matchmaker's stats lived in memory
(an evicted object lost them); and the bots answered every other tick.
Now the matchmaker ticks while anyone waits (which also keeps it in
memory), a match lasts `MATCH_MS`, the stats are in its storage, and a bot
answers every state.

## Notes for the site

Say what's different, and only what we've checked:
- **Placement by measured latency.** Each player's browser times every
  node; the match runs on the node best for both, and two players too far
  apart for a fair game (`MAX_RTT_MS`, 150 ms for the worse off) aren't
  matched at all. Cloudflare's Durable
  Objects take a coarse region hint; this picks the node, from what the two
  players measured.
- **Your own machines**, one small binary, any language that compiles to
  Wasm, many tenants, open source.
- **Honest about durability:** the game state is deliberately in memory,
  the records are on disk, and the docs say which is which.

Multiplayer games on Durable Objects aren't new (Cloudflare's own demos do
it), so don't claim they can't be done elsewhere. Don't compare with celld
on games until we've checked what it does.

## Around the world (DigitalOcean and Cloudflare)

`cloud/` runs the same game on one DigitalOcean droplet per region (New
York, Frankfurt, Singapore by default), with an R2 bucket and one name,
`arena.<your domain>` (the nodes, round robin; with `load_balancer = true`,
Cloudflare's paid load balancer answers with the nearest one). The game
plays on the nearest node either way: the page measures its round trip to
each, and the matchmaker picks by those. Each node also
has its own name, `n0.arena.<domain>` and so on. HTTPS throughout (a
`.dev` name must have it): Terraform gets a Let's Encrypt certificate for
the name and `*.name` through a DNS challenge, and combd serves it itself
on each node (single mode: it ends TLS on 443, and 80 redirects there).
Until 2026-10-20 Caddy did, in front: a node held about 9,500 connections
then, about 70,000 now (`combd/RESULTS.md`, "What a node holds").

1. GitHub builds combd (`.github/workflows/arena-image.yml`) on each push
   and puts it on the repo's `arena` release, where nodes fetch it. Nodes
   run it as its own user under systemd (`cloud/combd.service`), no Docker
   (Docker took ~100 MB of a node's 1 GB; nodes before 2026-10-21 moved
   with `cloud/to-native.sh`).
2. Make the bucket and its key: `wrangler r2 bucket create combd-arena`,
   then an R2 API token with Object Read & Write on it.
3. `cp cloud/terraform.tfvars.example cloud/terraform.tfvars` and fill it in.
4. Put a DigitalOcean token and a Cloudflare API token in it too (the
   Cloudflare one needs Zone: DNS Edit, Zone: Load Balancers Edit, and
   Account: Load Balancing: Monitors and Pools Edit), then `cloud/up.sh`.
5. `cloud/deploy.sh` deploys again after a change; `cloud/down.sh` takes it
   all down (about $18 a month while it's up, plus the load balancer).
6. After that, a push to `main` rolls out by itself: CI says a new version
   is coming (no new matches), waits for live ones to end, and deploys the
   game; a change to combd itself also moves each node to the new
   release, one at a time, and only once the new combd's builds of every
   compiled module are in the bucket (it asks for them, and the old one
   serves meanwhile). Builders update by themselves with Watchtower (Builders, below). While one restarts (a few seconds) it
   refuses new connections, and browsers try another of the name's nodes;
   holding them instead (the socket kept open across the restart) is next.
   A change to docs alone does nothing.
7. Other apps share the nodes: each in `apps` (in `cloud/main.tf`; the
   million checkboxes by default) gets `<app>.<domain>` in the certificate
   and DNS. Running nodes keep the certificate they started with, so after
   a `terraform apply` that changes it (a new app, or a renewal),
   `cloud/tls.sh` gives every node the new one; combd picks it up within
   seconds, for new connections. With more
   than one app on the nodes, each names its domains in its manifest.
8. **A new combd reads bundles an older one deployed** (it meets them in the
   bucket through every update). `deploy.rs`'s test of frozen bundles
   (`combd/src/testdata`) fails if a change can't; on 2026-10-19 one
   couldn't, and every app had to be deployed again. The other way round, a
   bundle the old combd can't read is refused by the nodes still on it: a
   new format ships in two steps, a combd that reads it, then one that writes it.

### Builders

Compiled modules are built by a builder: any Linux machine with Docker (a
Windows box's WSL works) that pulls jobs from the bucket and signs what it
builds with `builder.key` (made by `cloud/up.sh`, in `cloud/.secrets`; the
nodes trust its public half). Give it its own R2 token for the bucket, in
`~/combd-builder/env` (chmod 600): `COMBD_S3`, `COMBD_S3_BUCKET`,
`COMBD_S3_KEY`, `COMBD_S3_SECRET`, `COMBD_S3_REGION=auto`,
`COMBD_BUILDER_ID`, `COMBD_BUILDER_KEY=/keys/builder.key`,
`COMBD_AOT_SRC=/combd/combd/aot`, `COMBD_BUILD_WORK=/work`. Then:

```sh
docker run -d --name combd-builder --restart unless-stopped --env-file ~/combd-builder/env \
  -v ~/combd-builder/builder.key:/keys/builder.key:ro -v combd-build-work:/work \
  ghcr.io/jtwebman/combd-builder:latest /combd/combd/target/release/combd builder
```

A builder builds for the combd it was made with, so it must move when
combd does. Watchtower moves it (checks every 2 minutes, this container
only):

```sh
docker run -d --name watchtower --restart unless-stopped -v /var/run/docker.sock:/var/run/docker.sock \
  nickfedor/watchtower --interval 120 --cleanup combd-builder
```

Until it has, nodes keep the combd they run (they wait for the new one's
builds), so a late builder delays an update; it doesn't take an app down.