# Phase 0 race on Linux. Build from the repo root:
#   docker build -f spike/Dockerfile -t combd-spike .
#   docker run --rm combd-spike
FROM rust:1.94.1-slim-bookworm
RUN rustup target add wasm32-unknown-unknown
WORKDIR /combd
COPY guest guest
COPY spike spike
RUN cd guest && cargo build --locked --release --target wasm32-unknown-unknown
RUN cd spike && cargo build --locked --release && cargo test --locked --release
WORKDIR /combd/spike
# glibc normally raises its mmap threshold after big frees, then hands out
# reused heap memory that it must zero by hand (slow, and it touches pages).
# Pinning it keeps every 1 MiB+ cell buffer as lazy zero pages from mmap.
ENV MALLOC_MMAP_THRESHOLD_=131072
CMD ["./target/release/combd-spike", "all"]
