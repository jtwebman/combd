# One Million Checkboxes

A million checkboxes, shared by everyone: check one and every screen
showing it sees it checked. A remake of
[One Million Checkboxes](https://eieio.games/blog/one-million-checkboxes/)
(eieio, 2024) on combd, built to be ported to other stacks and raced.

## What it uses

| Piece | Type | What it does |
|---|---|---|
| `web` | `http` | the page, and the count of checked boxes |
| `viewer` | `ws` | one per WebSocket: what its page looks at, and its sets (20 a second at most) |
| `chunk` | `durable` | one per 15,625 boxes (`0` to `63`): the bits, in memory and in its SQLite, and who's watching |
| `boxes` | `durable` | one (`all`): each chunk's count, for the total |

A page opens **one** socket, to `/ws`, and that starts a viewer just for
it. The viewer subscribes to the chunks on screen (one or two) and passes
the page's sets on. A chunk sends every change **straight to the watchers'
sockets**, on whatever node holds them, without waking the viewers: a page
that's just watching costs its socket and about 30 bytes of viewer state
(asleep after 2 s idle). The original sent every client every change; here
a page gets only the boxes it can see.

The protocol, on the page's socket:
- it sends `{"t":"range","from":<box>,"to":<box>}` when what's on screen
  moves to other chunks, and every 20 s (that renews the subscriptions: a
  chunk forgets watchers that stop); it gets `{"t":"all","n":<chunk>,"bits":"<hex>"}`
  for each chunk (bit `i` is byte `i / 8`, bit `i % 8`);
- it sends `{"t":"set","i":<box>,"v":true}` and everyone watching that box,
  the sender too, gets `{"t":"set","n":<chunk>,"i":<box in chunk>,"v":true}`
  once it's on disk. A set says what the box should be, not "toggle", so
  two people clicking at once both get what they clicked. Past 20 a second
  it gets `{"t":"slow","i"}` instead.

`GET /api/count` is `{"checked": n}`: each chunk tells `boxes` its count at
most once a second.

The columns are however many fit the window, so a box sits somewhere else
on every screen, and a picture drawn on one is noise on the next.

## Two modules

The viewer is its own module (`viewer/`), small on purpose: no page, a
64 KB stack, 128 KB to start, capped at 512 KB (`memory_kb`). The page,
the chunks, and `boxes` share the other one (`cell/`).

## Run it

```sh
./build.sh
cd ../../combd && cargo run --release -- serve-app interp ../examples/million/app 8080
```

Then open http://million.localhost:8080 (browsers send `*.localhost` to
your own machine; other names get a 404).

Live at https://million.combd.dev, on the arena's three nodes (New York,
Frankfurt, Singapore; see `../arena/README.md`). Deploy a change with the
token in `../arena/cloud/.secrets`:

```sh
./build.sh && cd ../arena/cloud && COMBD_TOKEN=$(cat .secrets/token) \
  COMBD_ADMIN_CA=.secrets/certs/ca.pem COMBD_ADMIN_NAME=node0.combd \
  ../../../combd/target/release/combd deploy ../../million/app acme https://n0.arena.combd.dev:9443
```

Tests: `million_one_session_per_socket` (one node: sessions asleep between
events, a change that wakes only its sender, a viewer that moves on, the
rate limit, the count, a closed socket ending its session) and
`million_across_a_fleet` (three nodes: a set on each reaches all three).

## Not done yet

- One push per watcher per change. One per node is next: a chunk sending
  one change to 500 viewers on 3 nodes should send 3 frames.
- The rate limit is per socket, and anyone can open more sockets.
