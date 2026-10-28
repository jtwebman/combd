# combd

**Self-hosted Durable Objects, built from scratch in Rust.**
Tiny sealed cells, packed tight. Light, but very strong.

**Docs: [docs.combd.dev](https://docs.combd.dev)**, served by combd itself.
**Try it:** [combd in ten minutes](https://docs.combd.dev/quickstart.html):
`combd new hello`, `combd dev`, a page and a counter with its own SQLite,
then on a server.

---

## The story

In October 2026, Cloudflare bought Deno. Most of the talk was about the Deno
runtime winding down. But the part that caught our eye was a small project the
Deno team had shipped that summer: **celld**.

celld had a beautiful idea. Take Cloudflare's Durable Objects, where every user,
chat room, or document is its own little server with its own database, and run
them on your own machines. No control plane. No consensus cluster. Just a bucket
you already own, like S3 or R2, holding everything together.

We loved that idea. But celld carried a lot with it: a full V8 engine, tens of
thousands of lines of Node.js shims, and a security model that trusts every line
of code it runs. Now it is headed into Cloudflare's own runtime.

So we asked a simple question. **What if we started over, and kept only the
good idea?**

Think of a honeycomb. Thousands of tiny cells, each one sealed off from the
next. It weighs almost nothing, and it can hold far more than its own weight.
That is what we want to build. That is **combd**.

## What we are reaching for

**Light.** One small binary. No V8, no Node.js. A sleeping cell costs almost
nothing, and waking one up should take well under a millisecond.

**Fair.** We borrow from Erlang, the system that has kept phone networks running
for decades. Every cell gets its turn on the CPU. A cell stuck in an endless
loop gets paused so others can run. It never freezes the machine.

**Let it crash.** When a cell breaks, only that cell goes down. It restarts
clean in microseconds, and its data is safe in SQLite. A cell that keeps
crashing gets slowed down, not restarted forever.

**Never lose a write.** If combd tells you a write is saved, it is saved, even
if a machine dies a moment later. We will prove that with a formal spec and a
crash simulator before we trust it, because the hardest bugs in systems like
this hide where no code review can see them.

**Secure from the first line.** Every cell runs in a WebAssembly sandbox. It has
no system calls and can't reach memory outside its own box. It only gets the
abilities we hand it. Around that sit more walls: locked-down processes,
encrypted and authenticated traffic between machines, and outbound requests
that are denied unless allowed. Security is not a later phase.

**Our own engine.** We wrote our own WebAssembly interpreter in safe Rust. Because a running cell is just plain data to it, combd can pause a cell
at any instruction, save it in the middle of a request, and even move it to
another machine while it runs. No JIT also means a whole class of sandbox
escape bugs can't happen. The engine sits behind a small interface, so a faster
tier, or Wasmtime, can plug in later without changing anything else.

**Yours.** It runs on your machines, and your state lives in your bucket. No
vendor, no lock-in, no account to sign up for.

**Rust first, then more.** Cells start as Rust compiled to WebAssembly. The same
simple API can later open the door to Python, Go, JavaScript, and more.

## Where we are

**Further along than we planned.** We started by reading celld closely and
writing down how we think combd should work. **[Read the plan.](plan.md)**
Then we built, and kept score. **[See the results.](combd/RESULTS.md)**

**The engine.** We wrote our own WebAssembly interpreter and raced it against
Wasmtime with 10,000 cells, some stuck in endless loops. The stuck cells cost
their neighbors about 40 microseconds, instead of 146 milliseconds without
preemption. Our interpreter started cells about 25 times faster, recovered
from crashes about 3 times faster, used less memory, and came in 5.5 times
smaller. It was far slower at raw math, so we also built an ahead-of-time
compiler that turns Wasm into plain safe Rust. The Rust compiler then proves
the sandbox holds, and it becomes machine code for any CPU.

**Languages.** Cells run in Rust, C, Zig, Go (TinyGo), and AssemblyScript
(see [langs/](langs/)). JavaScript is next: a Workers-compatible library
over combd's host, on a JS-to-Wasm compiler (our own first try, jsc, is
retired; its results below stand).

**Speed over HTTP**, one CPU core each, the same small API app everywhere:
- **A combd cell in Rust** handled 1.3x to 1.6x more requests than Bun. It
  beat celld by up to 5x and workerd by up to 7x, with tail latency under
  half a millisecond.
- **The same app in JavaScript**, compiled by jsc (since retired), beat Node on every route
  and celld and workerd by 2x to 4x. It's still 9% to 32% behind Bun.
- **With a CPU-heavy neighbor on the same core**, it kept 35x to 56x Bun's
  throughput, because combd keeps yielding and Bun can't.

**What it runs.** The details are in [the docs](https://docs.combd.dev);
the short version:
- **Durable Objects:** each object a warm cell with its own SQLite, every
  answer waiting until its writes are on disk. On one core, combd beat
  workerd's Durable Objects by 1.8x on writes and 8.3x on reads.
  ([Processes](https://docs.combd.dev/processes.html),
  [storage](https://docs.combd.dev/storage.html))
- **WebSockets that sleep:** a quiet room leaves memory with its sockets
  still open, and the next message wakes it with its state intact.
- **Your own Postgres,** through one pool for the whole fleet with a max you
  set: a thousand nodes against a database that takes 100 connections, and
  it never sees more than 100. Transactions across many messages, and a
  crash rolls them back. ([Postgres pools](https://docs.combd.dev/postgres.html))
- **Static files** beside the code, served from memory: a page and its
  assets, or a whole site (these docs).
  ([Static files](https://docs.combd.dev/static-files.html))
- **A fleet over your bucket:** several nodes share one S3 or R2 bucket,
  which decides who owns each object. Writes are confirmed by peer nodes,
  and a node that dies, freezes, or is cut off is taken over with every
  acknowledged write intact. Each part is checked four ways: a TLA+ spec, a
  deterministic simulator, kill tests on a real cluster, and known-bad
  versions that must fail. ([Running a fleet](https://docs.combd.dev/fleet.html))
- **Compiled on deploy:** a module runs interpreted, or compiled to machine
  code by builders you run, which ask the fleet for work or have it pushed
  to them (Cloud Run, scaled to zero), and sign what they build.
  ([Builders](https://docs.combd.dev/builders.html))
- **Deploys from anywhere:** `combd deploy` with a token scoped to one app.
  ([Deploying](https://docs.combd.dev/deploying.html))
- **TLS in combd itself,** no proxy in front: an idle connection costs about
  10 KB, against about 106 KB behind Caddy.
- **Overload says no instead of falling over:** a full node turns new
  connections away at TCP and keeps serving the ones it has.
  ([Overload](https://docs.combd.dev/overload.html))

**Live, on $6 machines.** Three apps run on a three-node fleet (New York,
Frankfurt, Singapore), one 1 vCPU, 1 GB droplet each, over an R2 bucket:
[a real-time 1 vs 1 game](examples/arena/),
[One Million Checkboxes](https://million.combd.dev) ([source](examples/million/)),
and [these docs](https://docs.combd.dev). Each node moves to a new release by
itself, only one signed with the release key. What one node that size does:
- **About 58,000 idle WebSockets**, then it turns new ones away and stays
  fast.
- **10,000 people clicking at once** on million, every click sent to
  everyone watching it: nothing lost, p50 echo under 420 ms.
- **Past about 30,000 active users it turns the rest away** and keeps
  serving the ones it has.

## What surprised us

Building changed the plan. The [plan](plan.md#what-building-taught-us) has
the full story; the short version:

- **Interpreting JavaScript can't beat Bun,** so JS has to be compiled.
- **fsync was 90% of a write.** Sharing one fsync across many writes made
  hot writes 8.3x faster.
- **"S3-compatible" isn't one standard.** One popular server let several
  writers win the same conditional write. Every node now tests its bucket
  for that before it serves.
- **The checkers earned their keep.** The TLA+ spec found a real bug in our
  fast-write design, and the simulator found another the spec couldn't see.
  Both were fixed before any real node ran that code.
- **Outside judges found bugs our tests missed.** The official Wasm
  testsuite and random modules raced against Wasmtime found a wrong answer
  in our interpreter, and loads our fast compiled tier skipped instead of
  trapping. None let a cell reach outside its sandbox; all are fixed.
- **The real danger was the host doing work a cell doesn't pay for.**
  Fuzzing the host API found one cell could panic the host or eat seconds of
  CPU and gigabytes of memory and disk. And any client could crash a server
  with one bad header. Every host-side cost has its own cap now.
- **Network splits never broke a write,** but they showed three ways a
  cut-off node held its clients too long. All three are fixed.
- **Benchmarks on a busy laptop swing 2x to 5x.** We only trust runs that
  alternate A and B, and real totals need quiet Linux machines.
- **A small machine runs out of packets, not code.** At 10,000 active
  users, 75% of a node's CPU is the kernel's TCP work. Moving TLS into the
  kernel and sending through io_uring didn't change the CPU at all.
- **Kernel TLS moves memory, it doesn't save it.** Our first measurement
  said it halved a connection's memory. It only counted combd's share; the
  kernel holds what combd let go. Measure the whole machine.
- **The proxy was the limit, not combd.** Behind Caddy, combd was about 3%
  of a connection's memory.

## Next

- **Environments:** an app deployed as staging and production, each with its
  own vars and secrets.
- **JavaScript, without a JS engine:** a Workers-compatible library over
  combd's host, compiled to Wasm.
- **A billion checkboxes** on the same three $6 servers, and what it needs
  first: per-IP limits, a budget for bucket writes, and cheaper cold starts.
- **One node's keys for one node:** today every node holds the bucket's
  credentials.

## Standing on shoulders

combd is inspired by [celld](https://github.com/denoland/celld),
[workerd](https://github.com/cloudflare/workerd), and
[Erlang/OTP](https://www.erlang.org/). We use none of their code, but we owe
them the ideas. [Wasmtime](https://wasmtime.dev/) is the bar our engine has
to race against, and the Bytecode Alliance's `wasmparser` checks every module
before our engine runs it.

## License

[Apache-2.0](LICENSE)
