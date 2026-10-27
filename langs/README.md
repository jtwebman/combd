# langs

The same test cell in C, Zig, Go (TinyGo), and AssemblyScript. `js/` holds
the JS app (`app.js`) that combd's JS-to-Rust compiler will compile, the
requests every cell answers, Node's exact answers (`expected.txt`, the
oracle), and the Node/Bun baseline (`bench.mjs`). Each one
exports `alloc` and `handle` and imports `combd.counter_incr`, exactly like
the Rust cell in `../guest`, and answers `hello`, `fib N`, `print`, `spin`,
and `panic`.

Build all of them in Docker (nothing to install on your machine), from the
repo root:

    docker build -f langs/Dockerfile -o langs/out langs

Then run them on both engines:

    cd combd && cargo run --release -- langs
