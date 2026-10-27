# Phase 0 race on Linux. Build from the repo root:
#   docker build -f combd/Dockerfile -t combd .
#   docker build -f combd/Dockerfile --build-arg FEATURES=aot-guard -t combd-aot .
#   docker run --rm combd
FROM rust:1.99.1-slim-bookworm
# FEATURES=aot-guard builds the AOT tier with guard pages.
ARG FEATURES=""
RUN rustup target add wasm32-unknown-unknown
WORKDIR /combd
COPY guest guest
COPY combd combd
COPY langs langs
RUN cd guest && cargo build --locked --release --target wasm32-unknown-unknown
# AOT builds are keyed by the Wasm's hash, so generate them from the Wasm built here.
RUN cd combd && cargo build --locked --release \
    && case "$FEATURES" in aot*) \
         ./target/release/combd aot-gen ../guest/target/wasm32-unknown-unknown/release/combd_guest.wasm aot/src/gen_rust.rs \
         && cargo build --locked --release --features "$FEATURES";; esac \
    && cargo test --locked --release --features "$FEATURES"
WORKDIR /combd/combd
# glibc normally raises its mmap threshold after big frees, then hands out
# reused heap memory that it must zero by hand (slow, and it touches pages).
# Pinning it keeps every 1 MiB+ cell buffer as lazy zero pages from mmap.
ENV MALLOC_MMAP_THRESHOLD_=131072
# glibc also caps mmapped blocks at 65,536 by default. Past that, cell buffers
# come from the heap and density falls apart (8.5 KB to 52 KB per idle cell).
ENV MALLOC_MMAP_MAX_=4000000
CMD ["./target/release/combd", "all"]
