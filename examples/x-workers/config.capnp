using Workerd = import "/workerd/workerd.capnp";

# The X clone on workerd: `workerd serve config.capnp`, with bundle.mjs
# (worker.mjs and postgres.js, bundled by esbuild) beside this file, and
# DATABASE_URL in the environment.
const config :Workerd.Config = (
  services = [
    (name = "main", worker = .x),
    # Postgres (or the pooler) is on a private or local address, which
    # workerd's default outbound network refuses.
    (name = "net", network = (allow = ["public", "private", "local"])),
  ],
  sockets = [(name = "http", address = "*:8080", http = (), service = "main")],
);

const x :Workerd.Worker = (
  modules = [(name = "bundle.mjs", esModule = embed "bundle.mjs")],
  compatibilityDate = "2026-09-01",
  compatibilityFlags = ["nodejs_compat"],
  globalOutbound = "net",
  bindings = [(name = "DATABASE_URL", fromEnvironment = "DATABASE_URL")],
);
