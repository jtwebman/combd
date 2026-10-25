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
| `viewer`, a session per socket, asleep when idle | `Viewer`, a Durable Object per socket, holding it with the hibernation API (or the Worker; see below) |
| a chunk sends a change straight to its watchers' sockets | a chunk calls each watcher's `Viewer` over RPC, and it sends it on |

The last row is the one real difference. On Workers, a socket belongs to
the object that accepted it; nothing else can write to it. So a chunk
can't reach a page's socket except through the object holding it: one RPC
per watcher per beat, and each wakes that object if it was hibernating.

`SESSIONS=worker` holds the page's socket the other way: in the Worker
invocation (which can't hibernate), with a socket per watched chunk that
the chunk holds hibernatable, so a chunk's beat is one `send` per watcher
and the Worker passes it on. That's cheaper on Cloudflare (outgoing
WebSocket messages aren't billed; RPC calls are). celld v0.6.2 drops the
Worker's end of such a socket right after the upgrade (the chunk's
`getWebSockets()` is empty at its next beat), so this way runs on workerd
only.

## Run it

workerd (`npx workerd serve config.capnp`, with `index.html` copied beside
it), or celld (`celld dev`, with `wrangler.json`): see
`../../bench/click/Dockerfile`, which does both. With wrangler, the same
`wrangler.json` should deploy it to Cloudflare (not tried).
