# Images for the HTTP race. Build from the repo root, one target at a time:
#   docker build -f bench/http/Dockerfile --target workerd -t bench-workerd .
# (run.sh does all of this.)
FROM debian:bookworm-slim AS bundle
WORKDIR /b
COPY langs/js/app.js langs/js/requests.txt bench/http/ ./
RUN cat app.js fetch.mjs > worker.mjs \
    && body=$(sed -n 3p requests.txt | cut -d' ' -f4-) \
    && printf 'wrk.method = "POST"\nwrk.headers["Content-Type"] = "application/json"\nwrk.body = [[%s]]\n' "$body" > orders.lua

FROM node:24.21.0-slim AS workerd
WORKDIR /w
RUN npm install --no-audit --no-fund workerd@1.20260929.1
COPY --from=bundle /b/worker.mjs /b/config.capnp ./
CMD ["npx", "workerd", "serve", "config.capnp"]

FROM ghcr.io/denoland/celld:v0.6.2 AS celld
WORKDIR /app
COPY --from=bundle /b/worker.mjs /b/wrangler.json ./
CMD ["dev", "--host", "0.0.0.0", "--port", "8080"]

FROM oven/bun:1.4.2-slim AS bun
WORKDIR /w
COPY --from=bundle /b/worker.mjs /b/bun-server.mjs ./
CMD ["bun", "bun-server.mjs"]

FROM node:24.21.0-slim AS node
WORKDIR /w
COPY --from=bundle /b/worker.mjs /b/node-server.mjs ./
CMD ["node", "node-server.mjs"]

FROM debian:bookworm-slim AS wrk
RUN apt-get update && apt-get install -y --no-install-recommends wrk curl ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=bundle /b/orders.lua /orders.lua
