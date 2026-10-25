# One Million Checkboxes, on Workers

[million](../million/) as a Cloudflare Workers app with Durable Objects,
for the click race (`bench/click`). Same page (`../million/client/index.html`),
same protocol, same limits: 64 chunks of 15,625 boxes, `range` and `set`
in, `all`, `set`, `sets` and `slow` out, at most 4 chunks watched and 20
sets a second per socket, changes sent on a 150 ms beat and only once
they're on disk (the output gate), subscriptions renewed every 20 s and
forgotten after 60. So `ws-click` drives it unchanged.

| million on combd | Here |
|---|---|
| `chunk`, durable | `Chunk`, a Durable Object with SQLite |
| `boxes`, durable | `Boxes`, a Durable Object with SQLite |
| `web`, http | the Worker's `fetch` |
| `viewer`, a session per socket, asleep when idle | a `Hub` holding many pages' sockets, hibernatable (or a `Viewer` per socket, or the Worker: see below) |
| a chunk sends a change straight to its watchers' sockets | a chunk calls each `Hub` with a watcher, once a beat, and the Hub sends it on |

The last row is the one real difference. On Workers, a socket belongs to
the object that accepted it; nothing else can write to it. So a chunk
can't reach a page's socket except through the object holding it.

## Three ways to hold a page's socket (`SESSIONS`)

- **`hub` (the default): Hub objects hold every page's socket**
  (`HUBS` of them, a page to one at random; 1 here), with the hibernation API
  (`ctx.acceptWebSocket`, `webSocketMessage`, the page's range in its
  socket's attachment, rebuilt from those if the Hub was evicted). A Hub
  watches a chunk once for all its pages that show it, renews that as its
  pages renew theirs, and keeps a copy of the chunk's bits, so a page that
  arrives gets `all` from the Hub with no call. A chunk's beat is one RPC
  per Hub with a watcher (not per page); the Hub sends the same message to
  each of its pages watching. This is what combd's chunk does in one host
  call, and what million's README lists as next for combd ("one push per
  node"). On Cloudflare, a Hub is one object on one thread: at real scale
  you'd raise `HUBS`. On the race's one core, one Hub did best (below).
- **`object`: a Viewer object per socket**, as million's viewer session:
  it hibernates when idle, and every beat a chunk calls each watcher's
  Viewer, which sends the change on. The faithful port, and the costly
  one: an RPC per watcher per beat, and each wakes its object.
- **`worker`: the Worker holds the page's socket** (it can't hibernate) and
  opens a socket to each chunk on screen, which the chunk holds
  hibernatable; a beat is one `send` per watcher, and the Worker passes it
  on. The Worker keeps its event alive with `ctx.waitUntil` until the page
  leaves: without it, celld closes a Worker's outgoing sockets when its
  event ends (its docs say so), which is why the first version of this
  port ran this way on workerd only.

All three write a chunk's changes once a beat, in one transaction
(`transactionSync`), just before the beat sends them; the sends wait for
the write (the output gate), so nobody sees a change that isn't on disk.
million writes each set as it comes, but its host commits all the requests
that waited in one transaction, so under load it's about the same.

## What each measured

The click race's harness (`bench/click/run.sh`), each server in Docker
with 1 CPU and 1 GB, no swap; idle pages arriving 500 a second until the
server refuses or dies, then 1,000 and 5,000 clicking pages for 60 s
(2026-10-25, Apple M4 Max, Docker Desktop's Linux VM; workerd
1.20260929.1, celld v0.6.2; another benchmark sometimes ran beside it).
Echo is the worst 10 s window's worst client. The `hub` rows had four
Hubs; the race then settled on one (below).

| | idle pages held | a page | 1,000 clicking: p50 / p99, memory, CPU | 5,000 clicking |
|---|---|---|---|---|
| workerd, `hub` | **11,463** (then killed for memory) | **90 KB** | 155 / 242 ms, 253 MB, 8% | p50 689 ms / p99 4.2 s, 612 MB, 53%; nothing lost |
| workerd, `object` | 4,665 (killed) | 200 KB | 158 / 400 ms, 429 MB, 13% | killed for memory as pages arrived |
| workerd, `worker` | 7,649 (killed) | 123 KB | 155 / 184 ms, 256 MB, 9% | p50 1.2 s / p99 5.2 s, 948 MB, 67% |
| celld, `hub` | **32,260** (then it shed load) | **24 KB** | 160 / 288 ms, 174 MB, 29% | p50 5.4 s / p99 9.9 s, 0.9% lost, 92% CPU |
| celld, `object` | 1,981 (shed load) | 275 KB | 3.6 s / 4.8 s, 682 MB, 84% | 43% lost |
| celld, `worker` | 16,503 (killed) | 58 KB | 1.7 s / 7.4 s, 333 MB, 86% | 78% lost |

So the race runs `hub` on both, with one Hub. There (`spike/RESULTS.md`,
two runs of 180 s a level): workerd held 11,500 idle pages (82 to 88 KB
a page) and kept p99 echo under 300 ms to 5,000 clicking pages, then hit
1 GB at 10,000; celld held 39,000 to 40,500 (23 KB) and was fine to 500,
with 1.7 to 5 s tails from 1,000. Before this round, the race ran `object`
on both and `worker` on workerd, with a chunk writing each set as it came
and workerd started through `npx`.

## What else was tried, and what it did

- **Batching.** A chunk already sent one message per watcher per beat
  (`set` for one change, `sets` for more), as million does. The Hub cuts
  that to one RPC per Hub per beat.
- **Writes once a beat**, in one transaction, instead of one per set
  (`hub`, 60 s): at 2,000 clicking pages, p99 echo 307 ms against 1.8 s
  on celld (CPU 50% against 58%), and 1.0 s against 2.1 s on workerd
  (15% against 17%). At 5,000 celld lost clicks either way (11% and 5%:
  past its limit, the runs are noisy). Kept.
- **Watchers on disk.** At 10 clicking pages, workerd lost a third of the
  clicks: a chunk with nothing to do is evicted after 10 s, forgetting its
  watchers, and a Hub renews only every 20 s. A chunk now keeps who's
  watching in its SQLite (a row a Hub or Viewer; renewals write nothing),
  and nothing was lost after (the race's numbers are all with this).
- **How many Hubs.** At 5,000 clicking pages (60 s): one Hub, workerd
  p50 382 ms / p99 1.6 s at 50% CPU and 524 MB, celld p50 1.8 s / p99
  8.9 s, 0.02% lost, 80% CPU; four Hubs (the table above, and a second
  run), workerd p50 238 to 689 ms / p99 0.9 to 4.2 s at 53 to 59%, celld
  p50 5.4 to 8.4 s, 1 to 11% lost, 92% CPU; sixteen, workerd p50 736 ms
  at 60% and 695 MB, celld killed for memory. Every Hub watching a chunk
  costs that chunk an RPC a beat, and one core gains nothing from more
  objects. So one.
- **workerd without npx.** `npx workerd serve` keeps a node process
  (~37 MB) above workerd for the server's life. The image runs the binary.
- **Hibernation on workerd.** It works: a Viewer left alone 10 s is
  evicted, and its constructor runs again at its next message (counted).
  But it frees little, because most of an idle page's memory on workerd
  isn't the object. Measured with 2,000 idle `ws-click` pages and nothing
  else in the Worker: a Worker holding a bare accepted socket costs about
  70 KB a page; passing it to one object that holds them all, about
  95 KB; an object per socket, about 195 KB, even with the objects
  evicted. Closing 1,000 sockets didn't lower the process's memory either
  (it's reused, not returned). workerd's config has no setting for this
  (its only eviction setting, `preventEviction`, keeps objects in); we
  found nothing else to change. On Cloudflare, an evicted object leaves
  the machine; these numbers say nothing about what Cloudflare holds.
- **Compression.** Browsers offer `permessage-deflate`, and workerd (at
  this compatibility date) accepts it: about 300 KB a socket with Node's
  WebSocket client, which offers it. `ws-click` doesn't offer it, so the
  race doesn't pay it; with real browsers, workerd's idle pages would cost
  about four times what the race shows.
- **Auto-response.** million's page sends no application pings (the
  WebSocket protocol's own pings are answered by the runtime), so
  `setWebSocketAutoResponse` has nothing to answer.
- **celld settings.** Its docs list `CELLD_IDLE_EVICT_S` (idle cells stay
  in memory without it) and a memory cap (`CELLD_MAX_RSS_MB`, 80% by
  default, past which it answers 500). The `hub` design's objects never
  sit idle with pages renewing every 20 s, so we left both alone.

## Run it

workerd (`workerd serve config.capnp`, with `index.html` copied beside it;
`SESSIONS` from the environment), or celld (`celld dev`, with
`wrangler.json`; `SESSIONS` from `.dev.vars`): see
`../../bench/click/Dockerfile`, which does both. With wrangler, the same
`wrangler.json` should deploy it to Cloudflare (not tried).
