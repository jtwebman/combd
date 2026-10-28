# combd 0.1: Durable Objects on a $6 server

*Draft, 2026-10-25. Numbers are from `combd/RESULTS.md`.*

combd runs Cloudflare-style Durable Objects on machines you own. Every
chat room, game, or document is its own small server with its own SQLite
database. State lives on local disk and in a bucket you already pay for
(S3, R2, or anything that honors conditional writes). No control plane, no
consensus cluster: one 13 MB binary, open source under Apache-2.0.

The point of this release is how little it needs. On one CPU and 1 GB, the
size of a $6-a-month droplet, combd holds about 152,000 idle WebSocket
pages, or 20,000 pages clicking with nothing lost. We raced it against the
two other ways to run Durable Objects yourself, Cloudflare's workerd and
Deno's celld, on the same box. The numbers are below.

## Why it fits

**WebAssembly, and no JavaScript engine.** Workers runtimes are built on
V8: every isolate brings a heap, a garbage collector, and a JIT. combd
runs WebAssembly on its own engine instead: an interpreter, or
ahead-of-time compiled to machine code through plain, safe Rust. A cell is
a Wasm module's memory and a few host tables. Starting one takes
microseconds. A sleeping WebSocket session is about 2 KB; a whole idle
connection, socket and kernel included, about 6 KB. Today you write cells
in Rust, with `combd-sdk`; our test cell is also written in
AssemblyScript, TinyGo, C, and Zig, and each passes the same checks:
anything that compiles to Wasm can be a cell.

**Every object is a process, Erlang style.** The scheduler can pause a cell
at any point, so a cell stuck in a loop costs its neighbors microseconds,
not the core. A cell that breaks goes down alone, restarts clean, and the
request that hit the crash is rolled back.

**Pushes go straight to sockets.** A chunk of checkboxes that changes
writes to its watchers' sockets directly, without waking the sessions that
hold them. A watching page never runs.

**Your SQLite, your bucket.** Each object has its own SQLite file. Writes
are confirmed by peer nodes on disk and reach the bucket in batches. The
bucket decides who owns each object, by conditional write. Every node
tests its bucket for that at startup, because one popular S3-compatible
server let several writers win the same conditional write.

**Objects move to their users.** In a fleet, an object moves to the node
most of its requests come in through, or stays spread evenly, or pinned to
a region. A full node turns new connections away at TCP, so browsers try
the next node by themselves.

## Checked before it was trusted

The ownership, replication, and catalog designs each have a TLA+ spec. A
deterministic simulator runs hundreds of thousands of crash, split, and
lost-message schedules against the real decision code, and known-bad
versions must fail both. The spec found a real bug in our fast-write
design, and the simulator found one the spec couldn't see. Both were fixed
before any real node ran that code.

We attacked it too, in rounds: the compiler and builder, the interpreter
and its host calls, the public edge, and the wire between nodes, each
finding proven with a failing test, fixed, then attacked again. Untrusted
Wasm has no instruction that runs data as code, and combd maps its memory
read and write, never executable. Under that, the nodes run with the kernel
refusing writable executable memory, a system call filter, and nothing on
the machine combd may start. What we found and what is still open is in
the plan.

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
| combd | 152,000 to 155,000, then it refused new ones | 6.0 KB |
| Phoenix | 66,600 to 68,000, then killed for memory | 14.7 KB |
| celld | 39,000 to 40,500, then killed for memory | 23 KB |
| workerd | about 11,500, then killed for memory | 82 to 88 KB |

Clicking pages, one CPU (p99 echo is the worst 10 seconds of three
minutes):

| Pages | combd | Phoenix | workerd | celld |
|---|---|---|---|---|
| 1,000 | p99 echo 159 to 166 ms, 60 to 62 MB, 4 to 8% CPU | p99 164 to 168 ms, 82 to 107 MB, 4 to 7% CPU | p99 184 to 191 ms, 165 MB, 5% CPU | p99 1.7 to 5.1 s, at 18% CPU |
| 5,000 | p99 312 ms, 141 MB, 26% CPU | p99 206 ms, 161 MB, 32% CPU | p99 220 to 299 ms, 515 MB, 38% CPU | p99 5 to 8 s, 80% CPU |
| 10,000 | p99 395 to 430 ms, nothing lost, 202 MB, 56 to 60% CPU | p99 238 ms, nothing lost, 239 MB, 84% CPU | p99 0.8 to 2.3 s, at 1 GB of 1 GB, 84% CPU | 0.2 to 0.6% of clicks lost, 97% CPU |
| 20,000 | 446,000 changes a second, p99 685 to 758 ms, nothing lost, 373 to 392 MB, 79 to 82% CPU | p99 912 ms, 2,463 pages closed, 2 clicks lost, 394 MB, 99% CPU | killed for memory | killed for memory |

Below 1,000 pages all four kept every echo under a third of a second.
Where workerd held on, it kept up: to 5,000 pages its echoes were as short
as anyone's. What ended it was memory: an open page costs it 13 to 14
times what it costs combd. celld ran out of time before it ran out of
CPU: from 1,000 pages its slowest echoes took seconds with its CPU at 18%.
The BEAM is the one that kept pace. At 5,000 and 10,000 pages Phoenix's
slowest echoes were shorter than combd's (206 and 238 ms, to combd's 312
and 395 to 430), for about a fifth more memory and 40 to 50% more CPU.
At 20,000 its one core filled and it closed 2,463 of the pages, where
combd kept all 20,000 at 80% of its CPU. combd held 2.2 times as many idle
pages.

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
that load in a box the size of a $6-a-month droplet, with about 40% of its
CPU free. Cloudflare wouldn't fall over at this load: it spreads objects
across many machines. That's what you pay it for.

Plain requests, no sockets, from 10 to 1,000 connections at once (wrk,
30 seconds each): `GET /` is million's page from a stateless process, and
`GET /api/count` goes through that process to one object, the total, and
back. Requests a second, and p99:

| Route, connections | combd | Phoenix | workerd | celld |
|---|---|---|---|---|
| `/`, 10 | 94,000 to 97,000, 0.15 ms | 41,000 to 44,000, 0.3 to 0.4 ms | 26,000 to 27,000, 97 to 103 ms | 24,000 to 25,000, 12 ms |
| `/`, 100 | 93,000 to 97,000, 1.7 to 1.9 ms | 41,000, 3.2 to 3.7 ms | 25,000 to 27,000, 2.6 to 3.4 s, 1 to 28 failed | 21,000 to 24,000, 17 to 19 ms |
| `/`, 1,000 | 70,000 to 73,000, 17 to 18 ms | 37,000, 33 ms | 21,000, 156 ms | 23,000, 183 ms |
| `/api/count`, 10 | 67,000, 0.2 ms | 50,000, 0.25 to 0.28 ms | 17,000 to 18,000, 221 to 270 ms | 12,000 to 13,000, 37 ms |
| `/api/count`, 100 | 101,000, 1.5 ms | 59,000 to 60,000, 2.2 to 2.5 ms | 15,000 to 16,000, 5.6 to 7.7 s, 9 to 96 failed | 13,000, 43 ms |
| `/api/count`, 1,000 | 72,000 to 76,000, 16 to 18 ms | 46,000, 26 ms | 14,000, 227 ms | 11,000, 175 ms |

Every server used its whole CPU. combd answered the most, and on the
route through an object, 1.7 times Phoenix's at 100 connections, in 5 to
106 MB; workerd held 615 to 790 MB, and its slowest requests took seconds.

## What these numbers don't say

- **It's a Mac, not a droplet.** We ran this on an Apple M4 Max, in
  Docker's Linux VM, with the load clients on the same machine. Each test
  ran twice; where runs differed, we show both. combd and Phoenix ran
  again after combd's last fixes: one of those two runs had the machine
  busy (both servers' slowest echoes grew to seconds), so for them we show
  the other, and combd's two runs alone on the same code. The droplet
  tests we've published before (TLS, real network) are the ones to quote
  for a node's capacity.
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
- **combd had a gap too.** When pages arrived as fast as the clients could
  open them, about 14,000 a second, combd was killed for memory in two of
  four tries before its guard caught up. We fixed it: it now measures its
  memory as connections arrive, freezes idle sessions at once under
  pressure, and leaves more room for the kernel's socket buffers. Since
  then, six bursts and four idle runs ended with combd refusing new pages,
  never killed. The price is the idle number: about 152,000 pages, where
  it held 156,000 before.

## Try it

Start with the quickstart: [combd in ten minutes](../docs/src/quickstart.md).

Everything here is in the repository: the plan with what building taught
us, every measurement with how it was made, the specs, and the race
itself (`bench/click/run.sh`, one command, pinned versions). If you run it
on other hardware and get different numbers, we want to see them.
