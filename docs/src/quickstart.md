# combd in ten minutes

You'll make an app with a page and a counter, run it on your machine, then
run it on a server. The counter is a durable object, one per id, each with
its own SQLite database. Each request to it is one transaction.

## 1. Install

You need Rust, from rustup.rs, with its WebAssembly target:

```sh
rustup target add wasm32-unknown-unknown
```

And combd. Once a release is out, take one for your machine from
https://github.com/jtwebman/combd/releases: Linux x86_64 and arm64, or
macOS arm64 and x86_64. Unpack it and put `combd` on your PATH. Or build
it, with Rust 1.99 or newer:

```sh
cargo install --locked --git https://github.com/jtwebman/combd combd \
  --no-default-features --features aot-lite
```

`combd help` lists what it does.

## 2. Make an app, and run it

```sh
combd new hello
cd hello
combd dev
```

`combd dev` builds the app's Rust for wasm32 into `modules/`, and serves
it on port 8080. Open http://hello.localhost:8080 and click. Or:

```sh
curl -X POST http://hello.localhost:8080/counter/demo/incr   # {"count":1}
curl -X POST http://hello.localhost:8080/counter/demo/incr   # {"count":2}
curl http://hello.localhost:8080/counter/other/              # {"count":0}
```

Stop it with Ctrl-C and start it again. The counts are still there, in
`data/`.

## 3. What you made

`combd.toml` says what runs and where requests go:

```toml
[app]
name = "hello"
domains = ["hello.localhost"]

[http.web]               # a stateless process: the page
module = "hello"
routes = ["/"]

[durable.counter]        # a durable object per :id, with its own SQLite
module = "hello"
routes = ["/counter/:id/*"]
```

`src/lib.rs` is one function. A request comes in, a response goes out:

```rust
fn handle(req: Request) -> Response {
    match (req.kind, req.method, req.route()) {
        ("api", "GET", "/") => Response::html(PAGE),
        ("do", "POST", "/incr") => {
            sql::run("CREATE TABLE IF NOT EXISTS counter (n INTEGER NOT NULL)", &[]).unwrap();
            sql::run("INSERT INTO counter (rowid, n) VALUES (1, 1) ON CONFLICT (rowid) DO UPDATE SET n = n + 1", &[]).unwrap();
            count()
        }
        ("do", "GET", "" | "/") => count(),
        _ => Response::text(404, "not found\n"),
    }
}
```

`kind` is `api` for the page and `do` for the counter. A counter's
`route()` is what comes after its id. Its SQL runs in its own database,
`counter/demo` apart from `counter/other`. The request is one
transaction. If `handle` panics, its writes are rolled back, and the next
request finds the counter as it was. combd answers only once the writes
are safe on disk.

Each counter is a cell, a small sandboxed process. combd pauses any cell
that runs too long, so a busy one can't starve the others. One that
crashes starts again clean.

## 4. Change it

Add a reset to `handle`:

```rust
("do", "POST", "/reset") => {
    sql::run("DELETE FROM counter", &[]).unwrap();
    count()
}
```

Run `combd dev` again, then:

```sh
curl -X POST http://hello.localhost:8080/counter/demo/reset   # {"count":0}
```

`combd-sdk` also has alarms (`alarm::set`), the clock (`now_ms`), and
calls to other processes (`call`, `send`). An alarm comes back as an
`ALARM` request. See [The Rust SDK](rust-sdk.md).

Files that need no code, like images, styles, or a whole site, go in a
`static/` folder beside `combd.toml`. See [Static files](static-files.md).

## 5. Run it on a server

A node serves every app in a folder, laid out as `<org>/<app>/`. On a Linux
server with combd on it, and a domain pointing there:

```sh
mkdir -p /srv/combd/apps/me/hello
# from your machine: the manifest and the built module are all it needs
# (and static/, if it has one)
scp -r combd.toml modules server:/srv/combd/apps/me/hello/
```

Put your domain in `combd.toml`'s `domains`, then start it with your
certificate:

```sh
COMBD_DATA=/srv/combd/data \
COMBD_TLS_CERT=/etc/letsencrypt/live/hello.example.com/fullchain.pem \
COMBD_TLS_KEY=/etc/letsencrypt/live/hello.example.com/privkey.pem \
COMBD_REDIRECT_PORT=80 \
combd serve-apps interp /srv/combd/apps 443
```

`interp` runs the modules interpreted. `serve-apps` takes vars and
secrets from a `combd.dev.toml` beside the manifest, if there is one. See
[Apps and the manifest](apps.md#the-dev-environment). Compiled modules
(`[modules.<name>] engine = "compiled"`) are built by builders, which
work through a fleet. On one node, keep them interpreted. See
[Compiled modules and builders](builders.md).

## 6. More than one machine

A fleet keeps objects in a bucket you own on S3 or R2, moves them to
where their users are, and survives losing a node. Apps reach it with
`combd deploy`, over a node's admin API. See [Deploying](deploying.md) and
[Running a fleet](fleet.md). `examples/arena/cloud/` sets up three nodes
on DigitalOcean with Terraform, with builders for compiled modules and
systemd units. `examples/arena/README.md` walks through it. `plan.md` has
the design and how it's tested, with a formal spec and a simulator that
crashes everything.
