# Apps and the manifest

An app is a folder:

```text
hello/
├── combd.toml       # the manifest: what runs, and what reaches it
├── combd.dev.toml   # your machine's vars and secrets (never in git)
├── modules/
│   └── hello.wasm   # one file per module
└── static/          # optional: files served as they are
```

The manifest is exactly one of `combd.toml`, `combd.yaml`, or
`combd.json`, at most 1 MB. All three read into the same tree and
get the same checks. combd reports every mistake at once. With any
mistake, nothing starts and no deploy goes through.

A whole one, with every section:

```toml
[app]
name = "shop"
domains = ["shop.example.com"]

[vars]
LOG_LEVEL = "info"                          # a default; its type is string
PG_MAX = { type = "int", min = 1, default = 8 }
REGION = { type = "string" }                # no default: every environment sets it
"MY_FLAG?" = { type = "bool" }              # ? = optional

[secrets]
PG_MAIN_URL = { secret = "pg-url", type = "url" }   # env var -> secret name
"SIGNING_KEY?" = "jwt-key"                          # secrets never have defaults

[modules.shop]
engine = "interpreted"

# A process's type is its table: http, ws, durable, or pool.
[http.api]
module = "shop"                             # modules/shop.wasm
routes = ["shop.example.com/api/*"]

[durable.cart]
module = "shop"
routes = ["/cart/:id/*"]

[pool.main]                                 # code names it: {"db": "main"}
module = "combd:postgres"
url = "$PG_MAIN_URL"
connection.timeout_ms = 3000
max = "$PG_MAX"
callers = ["api", "cart"]
```

The top level takes only `app`, `vars`, `secrets`, `modules`, `http`,
`ws`, `durable`, and `pool`.

## `[app]`

| Key | What it is |
|---|---|
| `name` | The app's name. Lowercase letters, digits, `-` and `_`, at most 63. Required. |
| `domains` | The hosts its routes answer on, like `["shop.example.com"]`. |

### Domains

A route with no host matches only the app's own domains. An app with no
domains answers on any host, but only while it's alone on the node. With
more than one app, each names its domains, and no two apps may share one.
A route that names a host must use one of the app's domains.

The org isn't in the manifest. It comes from who deployed the app with
`combd deploy <dir> <org> <url>`, or on a single node from the app's
folder, `<root>/<org>/<app>/`. Everything an app owns is scoped to
`<org>/<app>`: its objects, its pools, its calls.

## `[vars]`

Vars are typed settings, given to every process as environment
variables. Each one is a default, or a table that says its type:

```toml
[vars]
LOG_LEVEL = "info"                                  # string, from the default
RETRIES = 3                                         # int
RATIO = 0.5                                         # float
VERBOSE = false                                     # bool
PG_MAX = { type = "int", min = 1, max = 64, default = 8 }
TIER = { type = "enum", values = ["free", "pro"], default = "free" }
LAUNCH = { type = "datetime", min = "2026-01-01T00:00:00Z" }
CALLBACK = { type = "url" }
SLUG = { type = "string", pattern = "^[a-z-]+$", max = 40 }
"BETA?" = { type = "bool", description = "turns on the beta page" }
```

Types:

- `string`, with an optional regex `pattern`
- `int`, `float`, `bool`
- `datetime`, RFC 3339 with an offset, like `2026-10-15T08:00:00Z`
- `url`, `scheme://...`
- `enum`, with `values`

`min` and `max` go with `int`, `float`, `datetime`, and `string`, where
they bound its length in characters. A table may have a `description`.

A var with no default must be set in every environment, unless its name
ends in `?`, which makes it optional. An optional var can't have a
default, since it would never be missing.

Names are `A-Z`, `0-9` and `_`, not starting with a digit. `COMBD_` names
are the host's. Every process also gets `COMBD_ORG`, `COMBD_APP`,
`COMBD_PROCESS` (its own name), and `COMBD_NODE`.

Set them on a fleet with `combd vars`. See [Deploying](deploying.md). While a
version is live, a value is checked against its type before it's taken.

## `[secrets]`

A secret is an env var whose value comes from the secret store, never from
the manifest. Each one names the secret it comes from:

```toml
[secrets]
PG_URL = { secret = "pg-url", type = "url" }   # PG_URL gets secret "pg-url"
"SIGNING_KEY?" = "jwt-key"                     # a string, optional
```

A secret's type is `string` unless it says otherwise. It never has a
default, which would be in git and in every environment. Secret
names are `a-z`, `0-9`, `-`, `_` and `.`, up to 128. A name can't be in
both `[vars]` and `[secrets]`. Errors about a secret never show its value.

Set them on a fleet with `combd secret put`. See [Deploying](deploying.md).

## Using vars and secrets in settings

A process's setting can name a var or a secret with `$`:

```toml
[pool.main]
module = "combd:postgres"
url = "$PG_URL"
max = "$PG_MAX"
callers = ["api"]
```

The var must be declared, and of exactly the type the setting wants.
No conversions. A required setting can't take an optional var.
`$$` is a literal `$`. The value is checked again when the app starts, so
a var whose own limits are wider than the setting's can't sneak past it.

## Processes

A process is a table under its type, named by you: `[http.web]`,
`[ws.viewer]`, `[durable.counter]`, `[pool.main]`. A name is one process,
whatever its type. Names are lowercase letters, digits, `-` and `_`.

| Type | One instance per... | See |
|---|---|---|
| `http` | request (a warm cell from a pool) | [Processes](processes.md#http-workers) |
| `durable` | id, with its own SQLite | [Processes](processes.md#durable-objects) |
| `ws` | WebSocket, in memory | [Processes](processes.md#websocket-sessions) |
| `pool` | a Postgres pool the host runs | [Postgres pools](postgres.md) |

Every process but a pool runs a module of the app. `module = "shop"` is
`modules/shop.wasm`. Many processes can share one module, loaded once.

Each process needs `routes` from the internet, `callers` from other
processes of the app, or both. One with neither is a mistake.

### `routes`

A route is `/path` for the app's domains, or `host/path` for that host:

```toml
routes = ["/", "/api/*", "shop.example.com/admin/*"]
```

- `:name` captures a segment. A durable object's route must name it
  `:id`, like `/cart/:id/*`. That segment is the object's id.
- `*` at the end takes the rest of the path.
- Segments use letters, digits, `-`, `.`, `_` and `~`. At most 256 bytes.
- Without a `*`, a route matches only that many segments.
- The query string isn't part of matching.
- Two routes of the same shape in one app are a mistake.

When more than one matches, the most specific wins. A route with a host
beats one without. Then, segment by segment, a literal beats a capture,
which beats `*`.

A `ws` process starts when a socket connects, so it must have a route.
A pool has no routes. Its callers reach it.

### `callers`

Which of the app's processes may `call` or `send` to this one:

```toml
[durable.player]
module = "game"
callers = ["lobby", "match"]
```

A call from anyone else is denied. For a pool, `callers` is required
and lists which processes may use it.

### Settings

Each type takes these, all optional, checked against these ranges.

**`http`**

| Setting | Default | What it does |
|---|---|---|
| `timeout_ms` | 30000 (1 to 900000) | The longest a `call` to it waits for an answer. |
| `max_concurrent` | 32 (1 to 1000) | For an event cell: requests one instance takes at once. |
| `fresh` | `false` | With `true`, every request gets a new cell. Nothing one leaves reaches the next, and nothing stays warm. |
| `memory_kb` | none (64 to 4194304) | The most memory a cell may grow to. |

**`durable`**

| Setting | Default | What it does |
|---|---|---|
| `timeout_ms` | 30000 (1 to 900000) | The longest a `call` to it waits for an answer. |
| `mailbox` | 1000 (1 to 1000000) | Requests that may wait for an object. Past it, new ones get `503 {"error":"overloaded"}`. |
| `batch` | 100 (1 to 10000) | The most waiting requests an object takes at once, if its module exports `handle_batch`. |
| `placement` | `"nearest"` | In a fleet: `nearest`, `first`, `spread`, or `pin`. See [Running a fleet](fleet.md#placement). |
| `pin` | none | Required for `placement = "pin"`. Regions or areas, like `"fra1,ams3"` or `"eu"`. |
| `fallback` | `"nearest"` | For `pin`. `nearest` runs elsewhere while none of its regions is up. `none` waits. |
| `scope` | `"region"` | For `spread`: `zone`, `region`, or `fleet`. |
| `memory_kb` | none (64 to 4194304) | The most memory a cell may grow to. |

**`ws`**

| Setting | Default | What it does |
|---|---|---|
| `timeout_ms` | 30000 (1 to 900000) | The longest a `call` to it waits for an answer. |
| `mailbox` | 1000 (1 to 1000000) | Events that may wait for a session. Past it, `503 overloaded`. |
| `hibernate_ms` | 5000 (0 to 86400000) | A session idle this long sleeps. 0 means after every event. |
| `hibernate` | `"auto"` | `auto` has the host keep what the cell changed. `app` has the app hand back its state. `off` stays awake. |
| `on_close` | `"wake"` | `wake` lets the session hear `WSCLOSE`. `drop` ends it without waking. |
| `state_kb` | 4 (1 to 1024) | For `hibernate = "app"`, the most state it may keep. |
| `memory_kb` | none (64 to 4194304) | The most memory a cell may grow to. |

**`pool`** is in [Postgres pools](postgres.md#the-manifest).

## `[modules.<name>]`

How a module runs:

```toml
[modules.shop]
engine = "compiled"     # or "interpreted"
```

A module not listed runs interpreted. `engine` is the only key, and the
module must exist in `modules/`. A compiled module is built by builders,
and a deploy waits for its builds. See
[Compiled modules and builders](builders.md).

## The dev environment

On your machine, vars and secrets come from `combd.dev.toml`, `.yaml`, or
`.json` beside the manifest. It takes only `[vars]`, by env var name, and
`[secrets]`, by secret name:

```toml
[vars]
PG_MAX = 4

[secrets]
pg-url = "postgres://me:pw@localhost/shop"
```

`combd dev` and `combd serve-apps` read it. It holds secrets, so combd
refuses it if git tracks it. `git rm --cached` it and put it in
`.gitignore`. It never goes up with a deploy.

## Limits

- A var or secret value: at most 16 KB.
- All of a process's env vars together: at most 256 KB.
- At most 512 vars and secrets.
- A `pattern`: at most 1024 bytes.
