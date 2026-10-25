using Workerd = import "/workerd/workerd.capnp";

# million on workerd: `workerd serve config.capnp`, with index.html
# (../million/client/index.html) copied beside this file.
const config :Workerd.Config = (
  services = [
    (name = "main", worker = .million),
    (name = "disk", disk = (path = "/w/data", writable = true)),
  ],
  sockets = [(name = "http", address = "*:8080", http = (), service = "main")],
);

const million :Workerd.Worker = (
  modules = [
    (name = "worker.mjs", esModule = embed "worker.mjs"),
    (name = "index.html", text = embed "index.html"),
  ],
  compatibilityDate = "2026-09-01",
  durableObjectNamespaces = [
    (className = "Chunk", uniqueKey = "chunk", enableSql = true),
    (className = "Boxes", uniqueKey = "boxes", enableSql = true),
    (className = "Hub", uniqueKey = "hub", enableSql = true),
    (className = "Viewer", uniqueKey = "viewer", enableSql = true),
  ],
  durableObjectStorage = (localDisk = "disk"),
  bindings = [
    (name = "CHUNK", durableObjectNamespace = "Chunk"),
    (name = "BOXES", durableObjectNamespace = "Boxes"),
    (name = "HUB", durableObjectNamespace = "Hub"),
    (name = "VIEWER", durableObjectNamespace = "Viewer"),
    # "hub" (a few Hub objects hold the pages' sockets) unless the
    # environment says "object" (a Viewer per socket) or "worker".
    (name = "SESSIONS", fromEnvironment = "SESSIONS"),
  ],
);
