# The arena demo: combd, the game module, and tc (for pretend distance).
# Build from the repo root:  docker build -f examples/arena/Dockerfile -t combd-arena .
#
# Two stages: the toolchain builds, and the image keeps only what runs (the
# binary, the app, and the test cell), at the same paths. Tens of MB, not
# 2 GB: a node pulls a new version without stealing its one CPU for a minute.
FROM rust:1.99.1-slim-bookworm AS build
# Debian's fixes since the base image was made (it's also the builders' image).
RUN apt-get update && apt-get -y upgrade && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
# Docker Desktop's network often breaks cargo's HTTP/2 downloads ("Error in
# the HTTP2 framing layer"); one request at a time is reliable.
ENV CARGO_HTTP_MULTIPLEXING=false CARGO_NET_RETRY=10
WORKDIR /combd
COPY guest guest
COPY combd combd
# The folder was spike/ and the binary combd-spike (until 2026-10-26 and
# 2026-10-25): a builder made before then runs /combd/spike/target/release/
# combd-spike with COMBD_AOT_SRC=/combd/spike/aot. These links keep it
# working until it's moved over (README, "Builders"); then they go.
RUN ln -s combd spike
COPY examples/arena examples/arena
# The git commit, built into combd and the game (shown on the page).
ARG ARENA_VERSION=dev
ENV ARENA_VERSION=$ARENA_VERSION COMBD_VERSION=$ARENA_VERSION
RUN cd guest && cargo build --locked --release --target wasm32-unknown-unknown
# With the AOT runtime (no modules built in): a node loads compiled
# modules' builds, and this stage, published as combd-builder, makes them
# (the same rustc and sources, so their pins match).
RUN cd combd && cargo build --locked --release --no-default-features --features aot-lite \
    && ln -sf combd target/release/combd-spike
RUN examples/arena/build.sh

FROM debian:bookworm-slim
RUN apt-get update && apt-get -y upgrade && apt-get install -y --no-install-recommends iproute2 && rm -rf /var/lib/apt/lists/*
COPY --from=build /combd/combd/target/release/combd /combd/combd/target/release/combd
COPY --from=build /combd/guest/target/wasm32-unknown-unknown/release/combd_guest.wasm /combd/guest/target/wasm32-unknown-unknown/release/combd_guest.wasm
COPY --from=build /combd/examples/arena/app /combd/examples/arena/app
# A hash of combd's own sources: nodes restart for a new image only when it
# changes (a game change reaches them as a deploy, with no restart).
ARG COMBD_SOURCE=
LABEL combd.source=$COMBD_SOURCE
WORKDIR /combd/combd
ENV MALLOC_MMAP_THRESHOLD_=131072
ENV MALLOC_MMAP_MAX_=4000000
