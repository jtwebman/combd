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
cd ../../spike && cargo run --release -- serve-app interp ../examples/arena/app 8080
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
  node; the match runs on the node best for both. Cloudflare's Durable
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
`arena.<your domain>`, that Cloudflare answers with the nearest healthy
node (DNS only, so a player's ping is to the node itself). Each node also
has its own name, `n0.arena.<domain>` and so on.

1. GitHub builds the image (`.github/workflows/arena-image.yml`) on each
   push; make the `combd-arena` package public once so droplets can pull it.
2. Make the bucket and its key: `wrangler r2 bucket create combd-arena`,
   then an R2 API token with Object Read & Write on it.
3. `cp cloud/terraform.tfvars.example cloud/terraform.tfvars` and fill it in.
4. `DIGITALOCEAN_TOKEN=... CLOUDFLARE_API_TOKEN=... cloud/up.sh`. The
   Cloudflare token needs Zone: DNS Edit, Zone: Load Balancers Edit, and
   Account: Load Balancing: Monitors and Pools Edit.
5. `cloud/deploy.sh` deploys again after a change; `cloud/down.sh` takes it
   all down (about $18 a month while it's up, plus the load balancer).
