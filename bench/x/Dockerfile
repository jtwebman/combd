# Images for the X race (bench/x/run.sh builds them from the repo root):
#   docker build -f bench/x/Dockerfile --target <combd|phoenix|workerd|celld|wrk|pg> -t bench-x-<target> .
# Each server gets DATABASE_URL (PG_URL for the Workers, whose pooler is
# inside) and POOL_SIZE: the same connections to Postgres for all four.

# combd as the nodes run it (aot-lite), the X clone compiled (aot-build,
# the builder's command).
FROM rust:1.99.1-slim-bookworm AS combd-build
RUN rustup target add wasm32-unknown-unknown
ENV CARGO_HTTP_MULTIPLEXING=false CARGO_NET_RETRY=10
WORKDIR /combd
COPY guest guest
COPY combd combd
COPY sdk sdk
COPY examples/x examples/x
RUN cd guest && cargo build --locked --release --target wasm32-unknown-unknown
RUN cd combd && cargo build --locked --release --no-default-features --features aot-lite
RUN cd examples/x && cargo build --locked --release --target wasm32-unknown-unknown \
    && mkdir -p /app/modules /aot && cp target/wasm32-unknown-unknown/release/x.wasm /app/modules/ \
    && sed 's/^engine = "interpreted"/engine = "compiled"/' combd.toml > /app/combd.toml \
    && w=/app/modules/x.wasm && /combd/combd/target/release/combd aot-build $w /aot/$(sha256sum $w | cut -c1-32).so

FROM debian:bookworm-slim AS combd
COPY --from=combd-build /combd/combd/target/release/combd /usr/local/bin/combd
COPY --from=combd-build /app /app
COPY --from=combd-build /aot /aot
ENV MALLOC_MMAP_THRESHOLD_=131072 MALLOC_MMAP_MAX_=4000000
ENV COMBD_AOT_DIR=/aot COMBD_DATA=/tmp/data COMBD_THREADS=1
# The pool's URL and size, as combd dev takes them (combd.dev.toml).
CMD ["sh", "-c", "printf '[vars]\\nPOOL_MAX = %s\\n[secrets]\\npg-url = \"%s\"\\n' \"${POOL_SIZE:-8}\" \"$DATABASE_URL\" > /app/combd.dev.toml && exec combd serve-app both /app 8080"]

# The X clone on the BEAM (examples/x-phoenix): Bandit, Plug, Postgrex.
FROM elixir:1.20.4-otp-28@sha256:d2b22bba38b499b709c113bd040bf4a6dd12b505668b44d9484b5964059fefa9 AS phoenix-build
ENV MIX_ENV=prod
WORKDIR /src
RUN mix local.hex --force && mix local.rebar --force
COPY examples/x-phoenix/mix.exs examples/x-phoenix/mix.lock ./
RUN mix deps.get --only prod && mix deps.compile
COPY examples/x-phoenix/lib lib
COPY examples/x-phoenix/rel rel
RUN mix release

FROM elixir:1.20.4-otp-28@sha256:d2b22bba38b499b709c113bd040bf4a6dd12b505668b44d9484b5964059fefa9 AS phoenix
COPY --from=phoenix-build /src/_build/prod/rel/x /app
ENV RELEASE_DISTRIBUTION=none RELEASE_MODE=interactive PORT=8080
CMD ["/app/bin/x", "start"]

# The Workers version (examples/x-workers), bundled with postgres.js.
FROM node:24.21.0-slim AS workers-build
WORKDIR /w
RUN npm install --no-audit --no-fund postgres@3.4.7 esbuild@0.25.10 && cp node_modules/@esbuild/linux-*/bin/esbuild /esbuild
COPY examples/x-workers/worker.mjs ./
RUN /esbuild worker.mjs --bundle --format=esm --platform=neutral --conditions=workerd,worker \
    --main-fields=module,main --external:cloudflare:sockets --external:'node:*' --outfile=bundle.mjs

FROM node:24.21.0-slim AS workerd
COPY bench/x/pgbouncer.sh bench/x/workers.sh /usr/local/bin/
RUN pgbouncer.sh install
WORKDIR /w
RUN npm install --no-audit --no-fund workerd@1.20260929.1
COPY examples/x-workers/config.capnp ./
COPY --from=workers-build /w/bundle.mjs ./
ENTRYPOINT ["workers.sh"]
CMD ["/w/node_modules/workerd/bin/workerd", "serve", "config.capnp"]

FROM ghcr.io/denoland/celld:v0.6.2 AS celld
COPY bench/x/pgbouncer.sh bench/x/workers.sh /usr/local/bin/
RUN pgbouncer.sh install
COPY --from=workers-build /esbuild /usr/local/bin/esbuild
ENV CELLD_ESBUILD=/usr/local/bin/esbuild
WORKDIR /app
COPY examples/x-workers/wrangler.json ./
COPY --from=workers-build /w/bundle.mjs ./
ENTRYPOINT ["workers.sh", "celld"]
CMD ["dev", "--host", "0.0.0.0", "--port", "8080", "--no-watch"]

# wrk, for the load.
FROM debian:bookworm-slim AS wrk
RUN apt-get update && apt-get install -y --no-install-recommends wrk && rm -rf /var/lib/apt/lists/*

# Postgres, as the race runs it (settings in run.sh).
FROM postgres:16-alpine@sha256:721873c34ceb9f8d8fc265984940dc982404c105f19ad51be9fdc5970a6080ea AS pg
