# langs

The same test cell in C, Zig, Go (TinyGo), and AssemblyScript. Each one
exports `alloc` and `handle` and imports `combd.counter_incr`, exactly like
the Rust cell in `../guest`, and answers `hello`, `fib N`, `print`, `spin`,
and `panic`.

Build all of them in Docker (nothing to install on your machine), from the
repo root:

    docker build -f langs/Dockerfile -o langs/out langs

Then run them on both engines:

    cd spike && cargo run --release -- langs
