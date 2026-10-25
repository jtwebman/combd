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

We wrote million three times: for combd, as a standard Workers app with
Durable Objects (for workerd and celld), and in Elixir on Bandit, the
server Phoenix runs on. Same page, same protocol, same limits. Then we
gave each server what a $6 droplet has (one CPU, 1 GB, no swap) and
pointed the same load client at it: pages that only watch, added until
the server refuses or falls over, and 10 to 20,000 pages that click.

Idle pages, in 1 GB:

| Server | Pages held | Memory a page |
|---|---|---|
| combd | about 156,000, then it refused new ones | 6.1 KB |
| Phoenix | about 68,000, then killed for memory | 14.5 KB |
| celld | 39,000 to 40,500, then killed for memory | 23 KB |
| workerd | about 11,500, then killed for memory | 82 to 88 KB |

Clicking pages, one CPU (p99 echo is the worst 10 seconds of three
minutes):

| Pages | combd | Phoenix | workerd | celld |
|---|---|---|---|---|
| 1,000 | p99 echo 162 ms, 62 MB, 4% CPU | p99 199 to 229 ms, 80 to 87 MB, 4% CPU | p99 184 to 191 ms, 165 MB, 5% CPU | p99 1.7 to 5.1 s, at 18% CPU |
| 5,000 | p99 0.7 to 1.0 s, 140 to 152 MB, 24% CPU | p99 215 to 299 ms, 165 MB, 29% CPU | p99 220 to 299 ms, 515 MB, 38% CPU | p99 5 to 8 s, 80% CPU |
| 10,000 | p99 0.7 to 3.4 s, nothing lost, 205 MB, 55% CPU | p99 227 to 251 ms, nothing lost, 245 MB, 76% CPU | p99 0.8 to 2.3 s, at 1 GB of 1 GB, 84% CPU | 0.2 to 0.6% of clicks lost, 97% CPU |
| 20,000 | 442,000 changes a second, p99 2.1 to 2.5 s, nothing lost, 430 MB, 77% CPU | p99 just over 1 s, 2 clicks lost, 440 MB, 100% CPU | killed for memory | killed for memory |

Below 1,000 pages all four kept every echo under a third of a second.
Where workerd held on, it kept up: to 5,000 pages its echoes were as short
as anyone's. What ended it was memory: an open page costs it 13 to 14
times what it costs combd. celld ran out of time before it ran out of
CPU: from 1,000 pages its slowest echoes took seconds with its CPU at 18%.
The BEAM is the one that kept pace. At 5,000 and 10,000 pages Phoenix's
slowest echoes were shorter than combd's, for about a fifth more memory
and 40% more CPU, until its one core filled at 20,000. combd held 2.3
times as many idle pages.

There's a design reason for part of this, and it's worth saying plainly.
On Workers, a WebSocket belongs to the object that accepted it. A chunk
can't write to a page's socket; it has to call the object holding it.
combd lets a chunk send straight to its watchers' sockets, without waking
the sessions that hold them, and Phoenix sends straight to each socket's
process. We measured three ways to write the Workers version, and all are
in the repository; the one raced here holds every page's socket in one
object, so a chunk's beat is one call, not one per page.

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
- **No TLS anywhere.** celld can't end TLS, so all four ran plain
  WebSockets. On our droplets combd ends TLS itself, which adds about 4 KB
  an idle connection.
- **One core each.** Phoenix ran one scheduler, combd one event loop.
  With more cores the BEAM, workerd and celld spread out too.
- **The Workers and Phoenix apps are ours.** We reworked the Workers app
  until changes stopped helping, and tuned the BEAM the way its users do;
  what each change measured is in their READMEs. Better ports may exist.
  We'd like to see them, and we'll run them.
- **Hibernation frees little on workerd.** It does evict idle objects,
  but most of an idle page's memory there is the connection itself, and
  we found no setting that changes it. On Cloudflare a hibernated object
  leaves memory, so workerd's idle numbers say nothing about what
  Cloudflare holds.
- **celld ran in its single-node dev mode.** We didn't measure its
  production mode.
- **combd has a gap too.** When pages arrived as fast as the clients could
  open them, about 14,000 a second, combd was killed for memory in two of
  four tries before its guard caught up. At 1,000 a second it refused
  cleanly in four of five tries; the fifth, with other tests sharing the
  machine, was killed at 114,000. It's on our list.

## Try it

Start with the quickstart: [combd in ten minutes](../docs/quickstart.md).

Everything here is in the repository: the plan with what building taught
us, every measurement with how it was made, the specs, and the race
itself (`bench/click/run.sh`, one command, pinned versions). If you run it
on other hardware and get different numbers, we want to see them.
