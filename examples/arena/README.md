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
the other player wins.

What's durable and what isn't, plainly: the records are; a match's state
lives in memory only. If its node dies mid-match, that match is lost.

## Run it

```sh
./build.sh                                     # the game module
cd ../../spike && cargo run --release -- serve-app interp ../examples/arena/app 8080
```

Then open http://localhost:8080 in two browser windows.

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
