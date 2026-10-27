# Images for the click race. Build from the repo root (run.sh does):
#   docker build -f bench/click/Dockerfile --target combd -t bench-click-combd .
#   docker build -f bench/click/Dockerfile --target workerd -t bench-click-workerd .
#   docker build -f bench/click/Dockerfile --target celld -t bench-click-celld .
# bench-click-combd is also the load client (ws-click).

# combd as the nodes run it (aot-lite, as examples/arena/Dockerfile), with
# million's modules compiled here (aot-build, the builder's command), as its
# manifest says (the page's copy is the same module: one build).
FROM rust:1.99.1-slim-bookworm AS combd-build
RUN rustup target add wasm32-unknown-unknown
ENV CARGO_HTTP_MULTIPLEXING=false CARGO_NET_RETRY=10
WORKDIR /combd
COPY guest guest
COPY spike spike
RUN cd guest && cargo build --locked --release --target wasm32-unknown-unknown
RUN cd spike && cargo build --locked --release --no-default-features --features aot-lite
COPY examples/million examples/million
RUN rm -rf examples/million/app/modules && examples/million/build.sh
# A build is found by its module's id: the first 16 bytes of its SHA-256.
RUN mkdir /aot && cd spike && for m in million viewer; do \
      w=../examples/million/app/modules/$m.wasm; \
      ./target/release/combd aot-build $w /aot/$(sha256sum $w | cut -c1-32).so; \
    done && ls -la /aot

FROM debian:bookworm-slim AS combd
COPY --from=combd-build /combd/spike/target/release/combd /usr/local/bin/combd
COPY --from=combd-build /combd/examples/million/app /app
COPY --from=combd-build /aot /aot
ENV MALLOC_MMAP_THRESHOLD_=131072 MALLOC_MMAP_MAX_=4000000
ENV COMBD_AOT_DIR=/aot COMBD_DATA=/tmp/data COMBD_THREADS=1 COMBD_MAX_CONNS=1000000
CMD ["combd", "serve-app", "both", "/app", "8080"]

# The Workers version (examples/million-workers), with million's own page.
FROM node:24.21.0-slim AS workerd
WORKDIR /w
RUN npm install --no-audit --no-fund workerd@1.20260929.1 && mkdir -p /w/data
COPY examples/million-workers/worker.mjs examples/million-workers/config.capnp ./
COPY examples/million/client/index.html ./
# The binary itself: through npx, a node process (~37 MB) sits above it.
CMD ["/w/node_modules/workerd/bin/workerd", "serve", "config.capnp"]

# celld bundles with esbuild (for the page's text import); a static binary.
FROM node:24.21.0-slim AS esbuild
RUN cd /tmp && npm install --no-audit --no-fund esbuild@0.25.10 && cp node_modules/@esbuild/linux-*/bin/esbuild /esbuild
# celld dev takes the Worker's variables from .dev.vars: one per design.
RUN for s in hub object worker; do echo "SESSIONS=$s" > /vars-$s; done

FROM ghcr.io/denoland/celld:v0.6.2 AS celld
COPY --from=esbuild /esbuild /usr/local/bin/esbuild
ENV CELLD_ESBUILD=/usr/local/bin/esbuild
WORKDIR /app
COPY examples/million-workers/worker.mjs examples/million-workers/wrangler.json ./
COPY examples/million/client/index.html ./
COPY --from=esbuild /vars-hub ./.dev.vars
CMD ["dev", "--host", "0.0.0.0", "--port", "8080", "--no-watch"]

FROM celld AS celld-object
COPY --from=esbuild /vars-object ./.dev.vars

FROM celld AS celld-worker
COPY --from=esbuild /vars-worker ./.dev.vars

# million on the BEAM (examples/million-phoenix): Bandit, Plug and WebSock,
# a release built and run in the same pinned image.
FROM elixir:1.20.4-otp-28@sha256:d2b22bba38b499b709c113bd040bf4a6dd12b505668b44d9484b5964059fefa9 AS phoenix-build
ENV MIX_ENV=prod
WORKDIR /src
RUN mix local.hex --force && mix local.rebar --force
COPY examples/million-phoenix/mix.exs examples/million-phoenix/mix.lock ./
RUN mix deps.get --only prod && mix deps.compile
COPY examples/million-phoenix/lib lib
COPY examples/million-phoenix/rel rel
COPY examples/million/client/index.html priv/index.html
RUN mix release

FROM elixir:1.20.4-otp-28@sha256:d2b22bba38b499b709c113bd040bf4a6dd12b505668b44d9484b5964059fefa9 AS phoenix
COPY --from=phoenix-build /src/_build/prod/rel/million /app
# No distribution (no epmd): one node, nothing to connect to.
# Modules load as first used, not all at boot (26 MB less code in memory).
ENV RELEASE_DISTRIBUTION=none RELEASE_MODE=interactive MILLION_DATA=/tmp/data PORT=8080
CMD ["/app/bin/million", "start"]

# wrk, for the req<n> tests (plain requests, no sockets).
FROM debian:bookworm-slim AS wrk
RUN apt-get update && apt-get install -y --no-install-recommends wrk && rm -rf /var/lib/apt/lists/*
