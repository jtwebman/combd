# Static files

An app can serve files as they are: pages, scripts, styles, images. Put
them in a `static/` folder next to `combd.toml` and `modules/`. There is no manifest setting for it. If `static/` is there,
combd serves it.

```text
site/
├── combd.toml
├── modules/
│   └── site.wasm        # optional: a static-only app has no modules
└── static/
    ├── index.html
    ├── 404.html
    ├── app.js
    ├── app.js.br        # optional: a precompressed copy
    ├── app.js.gz
    └── guide/
        └── index.html
```

## What gets served

Routes in the manifest come first. A `GET` or `HEAD` that no route
matches is looked up in `static/`:

| Request | What it gets |
|---|---|
| `/app.js` | `static/app.js` |
| `/` or `/guide/` | that folder's `index.html` |
| `/guide` (a folder with an `index.html`) | a `301` to `/guide/`, so the page's relative links work |
| anything else | `static/404.html` with status 404 if it's there, else the plain 404 |

The 404 page is the one at the top of `static/`. Other methods, like
`POST` and `PUT`, never reach the static files. With no route, they get
the plain 404.

Static files follow the app's [domains](apps.md#domains) like its routes
do. A request reaches an app's files only on its domains, or on any host
for an app alone on a node with no domains.

## Headers

Every file gets:

- `content-type`, by its extension, such as `.html`, `.css`, `.js`,
  `.json`, `.svg`, `.png`, `.woff2`, `.wasm`, `.pdf`, and more. Anything
  unknown is `application/octet-stream`.
- An `etag`. A browser that sends a matching `if-none-match` gets a `304`
  with no body.
- `cache-control: no-cache`. Browsers keep the file but ask again each
  time, and the answer is usually that cheap `304`.
- `x-content-type-options: nosniff`.

## Compressed copies

combd doesn't compress files. To send a compressed copy, put it
beside the file: `app.js.br`, `app.js.gz`, or both. A browser whose
`accept-encoding` takes `br` gets the `.br` copy. Otherwise, one that
takes `gzip` gets the `.gz`. Either way, combd sets `content-encoding` and
`vary: accept-encoding`. Each copy has its own ETag. Anyone else gets the
plain file. The `.br` and `.gz` files aren't served under their own
names.

```sh
brotli -k static/app.js     # static/app.js.br
gzip -k static/app.js       # static/app.js.gz
```

## In memory

A version's files are read once, when it loads, and held in memory. A
request never waits on a disk read. A new deploy brings its own files.

## Names and limits

The files go up with the app in its [deploy bundle](deploying.md). Their
paths must be plain names:

- letters, digits, `.`, `-` and `_`
- at most 16 folders deep, and 255 characters in all

A deploy refuses any file that breaks these rules, and names it. Hidden
files, whose names start with `.` (`.DS_Store`, `.env`), are left out. A
bundle holds at most 4096 files and 64 MB in all, modules and manifest
included.

## A site with no code

An app can be only static files: a manifest with no processes, and a
`static/` folder. This site is one.

```toml
[app]
name = "docs"
domains = ["docs.example.com"]
```

```sh
combd deploy ./docs acme https://node.example.com:9443
```

`combd dev` builds the app's Rust crate first, so it needs one. To try a
static-only app on your machine, serve its folder's parent with
`serve-apps`, laid out as `<org>/<app>/`:

```sh
mkdir -p apps/local && cp -r docs apps/local/docs
combd serve-apps interp apps 8080
curl -H 'host: docs.example.com' http://127.0.0.1:8080/
```

## A page and an API

Files and routes mix. The routes take what they match, and the files
the rest.

```toml
[app]
name = "shop"
domains = ["shop.example.com"]

[http.api]
module = "shop"
routes = ["/api/*"]
```

Here `/api/cart` goes to the `api` process, and `/`, `/app.js`, and
`/about/` come from `static/`.
