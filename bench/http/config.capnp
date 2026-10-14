using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [(name = "main", worker = .bench)],
  sockets = [(name = "http", address = "*:8080", http = (), service = "main")],
);

const bench :Workerd.Worker = (
  modules = [(name = "worker.mjs", esModule = embed "worker.mjs")],
  compatibilityDate = "2026-09-01",
);
