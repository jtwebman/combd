# One Million Checkboxes

A million checkboxes, shared by everyone: check one and every screen
showing it sees it checked. A remake of
[One Million Checkboxes](https://eieio.games/blog/one-million-checkboxes/)
(eieio, 2024) on combd, built to be ported to other stacks and raced.

## What it uses

| Piece | combd feature |
|---|---|
| `web` | a worker: the page, on the app's domains only |
| `chunk` | a Durable Object per 15,625 boxes (64 make the million): the bits in memory and in its SQLite (one row per checked box), and the WebSockets of everyone looking at it |

A page opens a socket only to the chunks on screen (one or two at a time),
so a viewer gets the changes it can see, not the whole site's. The
original sent every client every change.

The protocol, per chunk socket (`/c/<n>/ws`): the page sends `{"t":"hi"}`
and gets `{"t":"all","bits":"<hex>"}` (bit `i` of the chunk is byte
`i / 8`, bit `i % 8`); it sends `{"t":"set","i":<0..15624>,"v":true}` and
everyone on the chunk, the sender too, gets the same back once it's on
disk. A set says what the box should be, not "toggle", so two people
clicking at once both get what they clicked. Over 20 sets a second on one
socket gets `{"t":"slow","i","v"}` (the box as it is) instead. A bad index
closes the socket (4000).

The columns are however many fit the window, so a box sits somewhere else
on every screen, and a picture drawn on one is noise on the next.

Live at https://million.combd.dev, on the arena's three nodes (New York,
Frankfurt, Singapore; see `../arena/README.md`). Deploy a change with
the token in `../arena/cloud/.secrets`:

```sh
./build.sh && cd ../arena/cloud && COMBD_TOKEN=$(cat .secrets/token) \
  COMBD_ADMIN_CA=.secrets/certs/ca.pem COMBD_ADMIN_NAME=node0.combd \
  ../../../spike/target/release/combd-spike deploy ../../million/app acme https://n0.arena.combd.dev:9443
```

## Run it

```sh
./build.sh
cd ../../spike && cargo run --release -- serve-app interp ../examples/million/app 8080
```

Then open http://million.localhost:8080 (browsers send `*.localhost` to
your own machine; other names get a 404).

## Not done yet

- No count of checked boxes across the million (the original had one).
- One message per set, to every socket on the chunk. Batch per beat if
  fan-out gets heavy.
- The rate limit is per socket, and anyone can open more sockets.
