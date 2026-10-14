using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "main", worker = .room),
    (name = "disk", disk = (path = "/w/data", writable = true)),
  ],
  sockets = [(name = "http", address = "*:8080", http = (), service = "main")],
);

const room :Workerd.Worker = (
  modules = [(name = "worker.mjs", esModule = embed "worker.mjs")],
  compatibilityDate = "2026-09-01",
  durableObjectNamespaces = [(className = "Room", uniqueKey = "room", enableSql = true)],
  durableObjectStorage = (localDisk = "disk"),
  bindings = [(name = "ROOM", durableObjectNamespace = "Room")],
);
