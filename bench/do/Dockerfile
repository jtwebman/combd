# Images for the Durable Object race. Build from the repo root:
#   docker build -f bench/do/Dockerfile --target workerd -t bench-do-workerd .
#   docker build -f bench/do/Dockerfile --target celld -t bench-do-celld .
#   docker build -f bench/do/Dockerfile --target wrk -t bench-do-wrk .
FROM node:24.21.0-slim AS workerd
WORKDIR /w
RUN npm install --no-audit --no-fund workerd@1.20260929.1 && mkdir -p /w/data
COPY bench/do/worker.mjs bench/do/config.capnp ./
CMD ["npx", "workerd", "serve", "config.capnp"]

FROM ghcr.io/denoland/celld:v0.6.2 AS celld
WORKDIR /app
COPY bench/do/worker.mjs bench/do/wrangler.json ./
CMD ["dev", "--host", "0.0.0.0", "--port", "8080"]

FROM bench-wrk AS wrk
COPY bench/do/many.lua bench/do/post.lua /
