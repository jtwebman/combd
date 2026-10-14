# combd

**Self-hosted Durable Objects, built from scratch in Rust.**
Tiny sealed cells, packed tight. Light, but very strong.

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

**Our own engine.** We plan to write our own WebAssembly interpreter in safe
Rust. Because a running cell is just plain data to it, combd can pause a cell
at any instruction, save it in the middle of a request, and even move it to
another machine while it runs. No JIT also means a whole class of sandbox
escape bugs can't happen. The engine sits behind a small interface, so a faster
tier, or Wasmtime, can plug in later without changing anything else.

**Yours.** It runs on your machines, and your state lives in your bucket. No
vendor, no lock-in, no account to sign up for.

**Rust first, then more.** Cells start as Rust compiled to WebAssembly. The same
simple API can later open the door to Python, Go, JavaScript, and more.

## Where we are

**At the very start.** We read celld closely, learned from what it got right
and what it got wrong, and wrote down how we think combd should work.

**[Read the plan.](plan.md)**

Then we ran the first experiment: a race between our own tiny interpreter and
Wasmtime, with 10,000 cells and a few of them stuck in endless loops. The stuck
cells cost their neighbors about 40 microseconds, instead of 146 milliseconds
without preemption. Our interpreter started cells about 25 times faster,
recovered from crashes about 3 times faster, used less memory, and came in
5.5 times smaller. It is still far slower at raw math, and closing that gap is
next. **[See the results.](spike/RESULTS.md)**

Cells already run in five languages: Rust, C, Zig, Go (TinyGo), and
AssemblyScript (TypeScript-like). See [langs/](langs/). JavaScript runs
through our own JS-to-Rust compiler, [jsc](jsc/), which turns an app into a
normal Wasm cell. Its answers match Node byte for byte.

We also built our own ahead-of-time compiler. It turns Wasm into plain safe
Rust, so the Rust compiler proves the sandbox holds. Then it becomes machine
code for any CPU.

Then we raced it over HTTP, one CPU core each, against workerd, celld, Bun,
and Node. Every server ran the same small API app. A sandboxed combd cell,
written in Rust and able to be preempted and crash safely, handled 1.3x to
1.6x more requests than Bun on every test. It beat celld by up to 5x and
workerd by up to 7x, with tail latency under half a millisecond.
**[See the race.](spike/RESULTS.md)**

The same app in JavaScript, compiled by jsc, beat Node on every route (1.3x
to 2x) and celld and workerd by 2x to 4x. It is still 9% to 32% behind Bun.
With a CPU-heavy neighbor on the same core, it kept 35x to 56x Bun's
throughput and 55x to 88x Node's, because combd keeps yielding while they
can't.

## Standing on shoulders

combd is inspired by [celld](https://github.com/denoland/celld),
[workerd](https://github.com/cloudflare/workerd), and
[Erlang/OTP](https://www.erlang.org/). We use none of their code, but we owe
them the ideas. [Wasmtime](https://wasmtime.dev/) is the bar our engine has
to race against, and the Bytecode Alliance's `wasmparser` checks every module
before our engine runs it.

## License

[Apache-2.0](LICENSE)
