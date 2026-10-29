# combd

**Self-hosted Durable Objects, built from scratch in Rust.**
Tiny sealed cells, packed tight. Light, but very strong.

combd runs apps made of small WebAssembly programs, called cells. A cell
can be a stateless HTTP handler, a durable object, or one process per
WebSocket. A durable object is one per id, each with its own SQLite
database. You say which in the app's manifest, `combd.toml`, and combd
routes requests to them.

It is one binary. It runs on one machine, or on a fleet that shares a
bucket you own on S3 or R2. The bucket decides who owns each
object. There is no control plane or consensus cluster to run.

What it does:

- **Every cell is sandboxed.** A cell has no system calls. It can't read
  files or open sockets. It gets only what the host hands it: its own
  SQLite, a clock, calls to other processes in its app, and Postgres
  through a pool the host runs.
- **Cells take turns.** combd runs each cell a short slice at a time. A
  cell stuck in a loop can't starve its neighbors.
- **Let it crash.** A cell that crashes takes only its own request down.
  For a durable object, that request's writes roll back, and the next
  request gets a fresh cell.
- **An answer means it's saved.** A durable object answers only once its
  writes are safe. That is on disk on one node, and with its peers or the
  bucket in a fleet. See [Storage and durability](storage.md).
- **Idle things are cheap.** Idle objects leave memory, and their state
  stays in SQLite. An idle WebSocket session sleeps, and its socket stays
  open.
- **Two engines.** combd has its own WebAssembly interpreter, and a
  compiler that turns a module into safe Rust, then machine code. Each
  module says which it runs on. See
  [Compiled modules and builders](builders.md).

Apps are written in Rust today, with [`combd-sdk`](rust-sdk.md). Other
languages that compile to Wasm work at the host's ABI level, without an
SDK. See [Other languages](languages.md).

Start with the [Quickstart](quickstart.md), a page and a counter on your
machine, then on a server.

How combd is built and tested, and the numbers behind it, are in the
repo: [`plan.md`](https://github.com/jtwebman/combd/blob/main/plan.md) and
[`combd/RESULTS.md`](https://github.com/jtwebman/combd/blob/main/combd/RESULTS.md).
combd is open source, under Apache-2.0. The code is on GitHub at
[github.com/jtwebman/combd](https://github.com/jtwebman/combd).

Every page is also plain markdown, at its name with `.md`. [`llms.txt`](/llms.txt) lists them, and [`llms-full.txt`](/llms-full.txt) has them all in one file, for tools that read docs.
