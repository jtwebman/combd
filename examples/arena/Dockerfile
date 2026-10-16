# The arena demo: combd, the game module, and tc (for pretend distance).
# Build from the repo root:  docker build -f examples/arena/Dockerfile -t combd-arena .
FROM rust:1.94.1-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends iproute2 curl && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
# Docker Desktop's network often breaks cargo's HTTP/2 downloads ("Error in
# the HTTP2 framing layer"); one request at a time is reliable.
ENV CARGO_HTTP_MULTIPLEXING=false CARGO_NET_RETRY=10
WORKDIR /combd
COPY guest guest
COPY spike spike
COPY examples/arena examples/arena
RUN cd guest && cargo build --locked --release --target wasm32-unknown-unknown
RUN cd spike && cargo build --locked --release --no-default-features
RUN examples/arena/build.sh
WORKDIR /combd/spike
ENV MALLOC_MMAP_THRESHOLD_=131072
ENV MALLOC_MMAP_MAX_=4000000
