FROM caddy:2.8.4 AS caddy
FROM debian:bookworm-slim
RUN apt-get update -qq && apt-get install -y -qq haproxy procps >/dev/null && rm -rf /var/lib/apt/lists/*
COPY --from=caddy /usr/bin/caddy /usr/bin/caddy
