# Builds the same test cell in Go, C, Zig, and AssemblyScript. From the repo root:
#   docker build -f langs/Dockerfile -o langs/out langs
FROM tinygo/tinygo:0.42.0 AS go
WORKDIR /src
COPY go/ .
RUN tinygo build -o /tmp/out/go.wasm -target=wasip1 -buildmode=c-shared -opt=2 -no-debug .

FROM debian:bookworm-slim AS zig
RUN apt-get update && apt-get install -y --no-install-recommends curl xz-utils ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ARG ZIG=0.16.0
ARG ZIG_SHA=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17
RUN curl -fsSL https://ziglang.org/download/$ZIG/zig-aarch64-linux-$ZIG.tar.xz -o zig.tar.xz \
    && echo "$ZIG_SHA  zig.tar.xz" | sha256sum -c \
    && tar xf zig.tar.xz && mv zig-aarch64-linux-$ZIG /zig
WORKDIR /src
COPY c/ c/
COPY zig/ zig/
RUN mkdir -p /tmp/out \
    && /zig/zig cc -target wasm32-wasi -mexec-model=reactor -O2 -s -o /tmp/out/c.wasm c/cell.c \
    && /zig/zig build-exe zig/cell.zig -target wasm32-wasi -fno-entry -rdynamic -O ReleaseFast -fstrip -femit-bin=/tmp/out/zig.wasm

FROM node:24-slim AS as
WORKDIR /src
RUN npm install --ignore-scripts --no-audit --no-fund assemblyscript@0.28.20
COPY as/ .
RUN mkdir -p /tmp/out && npx asc cell.ts -O3 --runtime incremental --exportStart _initialize -o /tmp/out/as.wasm

FROM scratch
COPY --from=go /tmp/out/ /
COPY --from=zig /tmp/out/ /
COPY --from=as /tmp/out/ /
