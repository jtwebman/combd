# combd: Durable Objects on your own machines

*Draft, 2026-10-25. Numbers are from `spike/RESULTS.md`, "The click race".*

combd runs Cloudflare-style Durable Objects on machines you own. Every
chat room, game, or document is its own small server with its own SQLite
database. State lives on local disk and in a bucket you already pay for
(S3, R2, or anything that honors conditional writes). There is no control
plane and no consensus cluster. It's one Rust binary, open source under
Apache-2.0.

Today we're publishing it, with a race against the two other ways to run
Durable Objects yourself: Cloudflare's workerd and Deno's celld.

## Why we built it

In October, Cloudflare bought Deno. The part that caught our eye was celld,
which the Deno team had shipped that summer: Durable Objects on your own
machines, with a bucket as the only coordinator. It's a good idea. celld
also carried a full V8, tens of thousands of lines of Node.js shims, and a
security model that trusts every line of code it runs. And it's now headed
into Cloudflare's runtime.

So we started over and kept the idea. We read celld closely, wrote a plan,
and used none of its code.

## How it differs

**Every object is a sandboxed process, Erlang style.** User code is
WebAssembly. combd runs it on its own interpreter, or compiles it ahead of
time to machine code through safe Rust. Either way the scheduler can pause
a cell at any point, so a cell stuck in a loop costs its neighbors
microseconds, not the whole core.

**Let it crash.** When a cell breaks, only that cell goes down. It restarts
clean in microseconds, and its data is safe in SQLite. A request that hit
the crash is rolled back.

**Your SQLite, your bucket.** Each object has its own SQLite file. Writes
are confirmed by peer nodes on disk and reach the bucket in batches. The
bucket decides who owns each object, by conditional write. Every node
tests its bucket for that at startup, because one popular S3-compatible
server let several writers win the same conditional write.

**Checked before it was trusted.** The ownership, replication, and catalog
designs each have a TLA+ spec. A deterministic simulator runs hundreds of
thousands of crash, split, and lost-message schedules against the real
decision code. Known-bad versions must fail both. The spec found a real
bug in our fast-write design, and the simulator found one the spec
couldn't see. Both were fixed before any real node ran that code.

**Reviewed for security in rounds.** Reviewers (an AI model, one per
surface, each in its own copy of the code) attacked the compiler and
builder, the interpreter and host calls, the public edge, and the wire
between nodes, each proving findings with a failing test. We fixed what
they confirmed, and the next round attacked the fixes. What they found and
what changed is written up in the plan, including what is still open.

**Small.** A sleeping WebSocket session is about 2 KB in combd. A whole
idle connection, socket and kernel included, is about 6 KB.

It runs live on three $6 droplets (New York, Frankfurt, Singapore): a
real-time game and a copy of One Million Checkboxes.

## The click race

One Million Checkboxes is a good test for this kind of system. A million
shared boxes, split into 64 chunk objects. Every open page holds a
WebSocket and watches a chunk or two. Every click goes to everyone
watching that chunk. Idle pages cost memory; clicking pages cost fan-out.

We wrote million twice: once for combd, and once as a standard Workers
app with Durable Objects, for workerd and celld. Same page, same protocol,
same limits. Then we gave each server what a $6 droplet has (one CPU,
1 GB, no swap) and pointed the same load client at it: pages that only
watch, added until the server refuses or falls over, and 1,000 to 20,000
pages that click.

Idle pages, in 1 GB:

| Server | Pages held | Memory a page |
|---|---|---|
| combd | about 156,000, then it refused new ones | 6.1 KB |
| workerd | 2,900 to 6,000, then killed for memory | 133 to 300 KB |
| celld | about 3,000, then it refused new ones | about 220 KB |

Clicking pages, one CPU:

| Pages | combd | workerd | celld |
|---|---|---|---|
| 1,000 | p50 echo 152 ms, 63 MB, 4% CPU | p50 155 ms, 290 to 610 MB | p50 8 to 9 s, CPU-bound |
| 5,000 | p50 112 to 153 ms, 146 MB, 25% CPU | p50 0.6 to 5.4 s at 920 to 940 MB, or killed for memory | killed for memory |
| 10,000 | p50 115 ms to 1.2 s, nothing lost, 220 MB, 55% CPU | killed for memory | half of all clicks lost |
| 20,000 | 445,000 changes a second, p99 under 0.9 s, nothing lost, 440 MB, 82% CPU | killed for memory | couldn't open the pages |

Where workerd held on, it kept up: at 1,000 pages it delivered the same
changes as combd, nearly as fast. What ended it was memory. An open page
costs it 20 to 50 times what it costs combd.

There's a design reason for part of this, and it's worth saying plainly.
On Workers, a WebSocket belongs to the object that accepted it. A chunk
can't write to a page's socket; it has to call the object holding it.
combd lets a chunk send straight to its watchers' sockets, without waking
the sessions that hold them. We measured two ways to write the Workers
version, and both are in the repository.

We also estimated what the same load would cost on Cloudflare, from its
published prices (we deployed nothing there). At 10,000 clicking pages
it's about $2.80 an hour with the cheaper design and about $34 an hour
with an object per socket, where each push is a billed call. combd ran
that load in a box the size of a $6-a-month droplet, with over 40% of its
CPU free. Cloudflare wouldn't fall over at this load: it spreads objects
across many machines. That's what you pay it for.

## What these numbers don't say

- **It's a Mac, not a droplet.** We ran this on an Apple M4 Max, in
  Docker's Linux VM, with the load clients on the same machine. Each test
  ran twice; where runs differed, we show both. The droplet tests we've
  published before (TLS, real network) are the ones to quote for a
  node's capacity.
- **No TLS anywhere.** celld can't end TLS, so all three ran plain
  WebSockets. On our droplets combd ends TLS itself, which adds about 4 KB
  an idle connection.
- **The Workers app is ours.** A better port may exist. We'd like to see
  one, and we'll run it.
- **Hibernation didn't free memory on workerd here.** Idle objects stayed
  in memory after three minutes, with no settings changed. On Cloudflare
  they'd leave memory, so workerd's idle numbers say nothing about what
  Cloudflare holds.
- **celld ran in its single-node dev mode.** We didn't measure its
  production mode.
- **combd has a gap too.** When pages arrived as fast as the clients could
  open them, about 14,000 a second, combd was killed for memory in two of
  four tries before its guard caught up. At 1,000 a second it refused
  cleanly every time. It's on our list.

## Try it

Start with the quickstart: [combd in ten minutes](../docs/quickstart.md).

Everything here is in the repository: the plan with what building taught
us, every measurement with how it was made, the specs, and the race
itself (`bench/click/run.sh`, one command, pinned versions). If you run it
on other hardware and get different numbers, we want to see them.
