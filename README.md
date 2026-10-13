# combd

Self-hosted Durable Objects, built from scratch in Rust.

Every cell is a tiny sandboxed Wasm process with its own SQLite database.
An Erlang-style scheduler pauses any cell that runs too long, so no one cell
can take over a CPU. A cell that breaks just crashes and restarts clean.
A bucket you own (S3, R2, GCS, Azure) is the only thing it needs.

**Status:** planning. There is no code yet. See [plan.md](plan.md).

Inspired by [celld](https://github.com/denoland/celld),
[workerd](https://github.com/cloudflare/workerd), and Erlang/OTP. combd uses
none of their code.

## License

[Apache-2.0](LICENSE)
