//! What a node serves: its tenants' apps, every app's processes, the
//! programs they run, and the routes that reach them (from each app's
//! manifest). With no manifest it's one program and the old fixed routes:
//! `/do/<name>` for objects, everything else for the cell.
//!
//! Everything is scoped to its app, `<org>/<app>`: object names
//! (`<org>/<app>/<process>/<id>`), pools, env vars, `call`s and leases. An
//! app's routes match only its own domains, and no two apps share one. The
//! org is the tenant, and comes from who deployed (here, the folder an app is
//! in), never from the app's own manifest.
//!
//! A deploy makes a new `App` from the old one, append-only: the new
//! version's processes are added, the old version's are marked `retired`,
//! and nothing moves. So a cell from the old version still points at a real
//! process: it finishes what it's doing on the old code, and is dropped
//! instead of going back to a pool. New requests route only to live
//! processes. ponytail: retired processes and their programs stay until the
//! node restarts; collect them once nothing uses them, if deploys get frequent.

use crate::manifest::{Resolved, Route, Seg};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct Proc {
    pub name: String,
    /// An older version's: it runs out what it has, and takes nothing new.
    pub retired: bool,
    /// Its app (`App::tenants`).
    pub tenant: usize,
    /// Index into `App::progs`.
    pub prog: usize,
    pub durable: bool,
    /// One instance per WebSocket (a session), in memory on the loop that
    /// holds the socket; idle past `hibernate`, it keeps up to `state_max`
    /// bytes and its cell goes.
    pub socket: bool,
    /// http: a fresh cell for every request, so nothing one leaves reaches
    /// the next (and no warm cache either).
    pub fresh: bool,
    pub hibernate: Duration,
    pub sleep: Sleep,
    /// A ws session hears its socket close (`WSCLOSE`); `on_close = "drop"`:
    /// it just ends, unwoken (its subscriptions expire on their own).
    pub wake_on_close: bool,
    pub state_max: usize,
    /// `mailbox`: the most requests that may wait for an instance. Past it,
    /// new ones are refused (503, "overloaded") rather than queued, so a
    /// busy object sheds load instead of piling it up.
    pub mailbox: usize,
    /// `batch`: the most waiting requests an object takes at once, if its
    /// module exports `handle_batch` (one run, one transaction).
    pub batch: usize,
    /// `placement = "spread"`: in a fleet, a new object starts on the node
    /// rendezvous hashing picks for its name, not the one that asked first,
    /// among the nodes in `scope` of the one that asked (its zone, its
    /// region, or the whole fleet).
    pub spread: Option<crate::fleet::Scope>,
    /// `memory_kb`, in Wasm pages: the most a cell may grow to.
    pub memory_pages: Option<u32>,
    pub max_concurrent: usize,
    pub timeout: Duration,
    /// Processes (of its app) that may `call` or `send` to this one.
    pub callers: Vec<usize>,
    /// Pools (of its app) this one may lease from.
    pub owns: Vec<String>,
}

/// How an idle session sleeps (`hibernate`): the host keeps what its cell
/// changed (`Auto`), the app hands back its state (`App`), or it stays warm.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sleep {
    Auto,
    App,
    Off,
}

/// One app of one org.
#[derive(Clone)]
pub struct Tenant {
    pub org: String,
    pub app: String,
    pub domains: Vec<String>,
    /// Its env vars, as WASI hands them out.
    pub environ: Arc<Vec<Vec<u8>>>,
    /// Its Postgres pools (named `<org>/<app>/<pool>`).
    pub pools: Vec<crate::pg::PoolCfg>,
}

impl Tenant {
    /// `<org>/<app>`: the prefix of everything it owns.
    pub fn key(&self) -> String {
        format!("{}/{}", self.org, self.app)
    }
}

pub struct App<P> {
    pub progs: Vec<Arc<P>>,
    pub procs: Vec<Proc>,
    pub tenants: Vec<Tenant>,
    pub router: Router,
    /// Programs by the hash of their bytes: the same module loads once,
    /// across apps and versions.
    by_hash: HashMap<[u8; 32], usize>,
    /// Apps taken down: (`tenants` index); their processes are retired.
    gone: Vec<usize>,
}

/// Where a request goes. Non-generic, so the (fuzzed) parser can use it.
pub struct Router {
    /// Most specific first.
    routes: Vec<Target>,
    /// Each app's domains, by app (`App::tenants`).
    domains: Vec<Vec<String>>,
    /// No manifest: the old fixed routes, all to process 0.
    pub legacy: bool,
}

struct Target {
    route: Route,
    proc: usize,
    tenant: usize,
    /// `<org>/<app>/<process>`.
    name: String,
    durable: bool,
}

pub enum Hit {
    /// A worker process, and the whole path.
    Cell(usize),
    /// A durable process's object `<org>/<app>/<process>/<id>`, and the path inside it.
    Object(String, String),
}

/// One app to load: its org (the tenant), its folder, its resolved manifest.
pub struct AppDir<'a> {
    pub org: &'a str,
    pub dir: &'a Path,
    pub resolved: &'a Resolved,
    pub app: &'a str,
    pub domains: &'a [String],
}

impl<P> App<P> {
    /// One program, the old routes.
    pub fn single(prog: P, max_concurrent: usize) -> App<P> {
        let proc = Proc { name: "main".into(), retired: false, tenant: 0, prog: 0, durable: false, socket: false, fresh: false, hibernate: Duration::ZERO, sleep: Sleep::Off, wake_on_close: true, state_max: 0, mailbox: usize::MAX, batch: 100, spread: None, memory_pages: None, max_concurrent, timeout: Duration::from_secs(30), callers: vec![], owns: vec![] };
        let tenant = Tenant { org: String::new(), app: "main".into(), domains: vec![], environ: Arc::default(), pools: vec![] };
        App { progs: vec![Arc::new(prog)], procs: vec![proc], tenants: vec![tenant], router: Router::legacy(), by_hash: HashMap::new(), gone: vec![] }
    }

    /// No apps yet.
    pub fn empty() -> App<P> {
        App { progs: vec![], procs: vec![], tenants: vec![], router: Router::new(vec![], vec![]), by_hash: HashMap::new(), gone: vec![] }
    }

    /// Every app, from its resolved manifest: each module loaded once (the
    /// same bytes in two apps are one program), each process with its
    /// settings and rights, all scoped to its app.
    pub fn from_apps(apps: &[AppDir], load: impl Fn(&[u8]) -> Result<P, String>) -> Result<App<P>, String> {
        let mut seen = std::collections::HashSet::new();
        for a in apps {
            if !seen.insert((a.org, a.app)) {
                return Err(format!("{}/{}: two apps with the same name in one org", a.org, a.app));
            }
        }
        App::empty().with_apps(apps, &[], load)
    }

    /// A new `App`: this one, with `apps` deployed (new, or new versions of
    /// apps it has) and the apps in `remove` taken down. Append-only: see the
    /// module's notes.
    pub fn with_apps(&self, apps: &[AppDir], remove: &[(String, String)], load: impl Fn(&[u8]) -> Result<P, String>) -> Result<App<P>, String> {
        let mut progs = self.progs.clone();
        let mut by_hash = self.by_hash.clone();
        let mut procs = self.procs.clone();
        let mut tenants = self.tenants.clone();
        let mut gone = self.gone.clone();
        let find = |tenants: &[Tenant], org: &str, app: &str| tenants.iter().position(|x| x.org == org && x.app == app);
        let changed: Vec<usize> = apps.iter().filter_map(|a| find(&tenants, a.org, a.app)).chain(remove.iter().filter_map(|(o, a)| find(&tenants, o, a))).collect();
        for p in procs.iter_mut().filter(|p| changed.contains(&p.tenant)) {
            p.retired = true;
        }
        for (o, a) in remove {
            if let Some(t) = find(&tenants, o, a) {
                gone.push(t);
            }
        }
        for a in apps {
            let environ = Arc::new(a.resolved.environ.iter().map(|(k, v)| format!("{k}={v}").into_bytes()).collect());
            let pools = crate::pg::PgCfg::from_manifest(a.resolved, &format!("{}/{}", a.org, a.app)).map(|c| c.pools).unwrap_or_default();
            let tenant = Tenant { org: a.org.into(), app: a.app.into(), domains: a.domains.to_vec(), environ, pools };
            match find(&tenants, a.org, a.app) {
                Some(t) => {
                    tenants[t] = tenant;
                    gone.retain(|&g| g != t);
                }
                None => tenants.push(tenant),
            }
        }
        // Domains: each app's own, no two the same, and an app with none
        // only when it's alone.
        let live: Vec<usize> = (0..tenants.len()).filter(|t| !gone.contains(t)).collect();
        let mut owner_of: HashMap<&str, String> = HashMap::new();
        for &t in &live {
            let x = &tenants[t];
            for d in &x.domains {
                if let Some(other) = owner_of.insert(d, x.key()) {
                    return Err(format!("{}: domain {d} is already {other}'s", x.key()));
                }
            }
            if live.len() > 1 && x.domains.is_empty() {
                return Err(format!("{}: with more than one app, each one names its domains ([app] domains)", x.key()));
            }
        }
        for a in apps {
            let t = find(&tenants, a.org, a.app).unwrap();
            let key = tenants[t].key();
            let user: Vec<(&String, &crate::manifest::Process)> = a.resolved.processes.iter().filter(|(_, p)| !p.kind.starts_with("combd:")).collect();
            let first = procs.len();
            let index = |n: &str| user.iter().position(|(name, _)| *name == n).map(|i| first + i);
            for (name, p) in &user {
                let path = crate::manifest::module_path(a.dir, &p.kind);
                let wasm = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let hash: [u8; 32] = {
                    use sha2::Digest;
                    sha2::Sha256::digest(&wasm).into()
                };
                let prog = match by_hash.get(&hash) {
                    Some(&i) => i,
                    None => {
                        progs.push(Arc::new(load(&wasm).map_err(|e| format!("{key}: modules/{}.wasm: {e}", p.kind))?));
                        by_hash.insert(hash, progs.len() - 1);
                        progs.len() - 1
                    }
                };
                let n = |k: &str| p.settings[k].as_u64().unwrap_or(0);
                let durable = p.settings["durable"].as_bool() == Some(true);
                procs.push(Proc {
                    name: (*name).clone(),
                    retired: false,
                    tenant: t,
                    prog,
                    durable,
                    socket: p.settings["socket"].as_bool() == Some(true),
                    fresh: p.settings.get("fresh").and_then(serde_json::Value::as_bool) == Some(true),
                    hibernate: Duration::from_millis(n("hibernate_ms")),
                    wake_on_close: p.settings.get("on_close").and_then(serde_json::Value::as_str) != Some("drop"),
                    sleep: match p.settings.get("hibernate").and_then(serde_json::Value::as_str) {
                        Some("app") => Sleep::App,
                        Some("off") => Sleep::Off,
                        _ => Sleep::Auto,
                    },
                    state_max: n("state_kb") as usize * 1024,
                    spread: (p.settings.get("placement").and_then(serde_json::Value::as_str) == Some("spread")).then(|| match p.settings.get("scope").and_then(serde_json::Value::as_str) {
                        Some("zone") => crate::fleet::Scope::Zone,
                        Some("fleet") => crate::fleet::Scope::Fleet,
                        _ => crate::fleet::Scope::Region,
                    }),
                    batch: p.settings.get("batch").and_then(serde_json::Value::as_u64).map_or(1, |n| n as usize),
                    mailbox: p.settings.get("mailbox").and_then(serde_json::Value::as_u64).map_or(usize::MAX, |n| n as usize),
                    memory_pages: p.settings.get("memory_kb").and_then(serde_json::Value::as_u64).map(|kb| kb.div_ceil(64) as u32),
                    max_concurrent: n("max_concurrent").max(1) as usize,
                    timeout: Duration::from_millis(n("timeout_ms")),
                    callers: p.callers.iter().filter_map(|c| index(c)).collect(),
                    owns: a.resolved.pools.iter().filter(|(_, pool)| pool.owners.contains(name)).map(|(n, _)| n.clone()).collect(),
                });
                for r in &p.routes {
                    if let Some(h) = &r.host {
                        if !a.domains.is_empty() && !a.domains.contains(h) {
                            return Err(format!("{key}: route {} is on {h}, which isn't one of its domains", r.text));
                        }
                    }
                }
            }
        }
        // Routes: every live process's, from its manifest.
        let mut targets = vec![];
        for (t, a) in apps.iter().map(|a| (find(&tenants, a.org, a.app).unwrap(), a)) {
            for (name, p) in a.resolved.processes.iter().filter(|(_, p)| !p.kind.starts_with("combd:")) {
                let i = procs.iter().position(|x| x.tenant == t && !x.retired && &x.name == name).unwrap();
                let durable = procs[i].durable;
                targets.extend(p.routes.iter().map(|r| Target { route: r.clone(), proc: i, tenant: t, name: format!("{}/{name}", tenants[t].key()), durable }));
            }
        }
        // ...and those of apps this deploy didn't touch.
        targets.extend(self.router.routes.iter().filter(|x| !procs[x.proc].retired && !gone.contains(&x.tenant)).map(|x| Target { route: x.route.clone(), proc: x.proc, tenant: x.tenant, name: x.name.clone(), durable: x.durable }));
        let domains = tenants.iter().enumerate().map(|(t, x)| if gone.contains(&t) { vec![] } else { x.domains.clone() }).collect();
        Ok(App { progs, procs, tenants, router: Router::new(targets, domains), by_hash, gone })
    }

    /// Every live app's pools.
    pub fn pools(&self) -> Vec<crate::pg::PoolCfg> {
        self.tenants.iter().enumerate().filter(|(t, _)| !self.gone.contains(t)).flat_map(|(_, x)| x.pools.clone()).collect()
    }

    /// Process `name` of app `tenant`: a call or a lease never leaves its app.
    pub fn proc_in(&self, tenant: usize, name: &str) -> Option<usize> {
        self.procs.iter().position(|p| p.tenant == tenant && !p.retired && p.name == name)
    }

    /// The process an object belongs to: `<org>/<app>/<process>/<id>`, or 0
    /// (no manifest).
    pub fn object_proc(&self, obj: &str) -> usize {
        if self.router.legacy {
            return 0;
        }
        let mut parts = obj.splitn(4, '/');
        let (Some(org), Some(app), Some(proc)) = (parts.next(), parts.next(), parts.next()) else { return 0 };
        let tenant = self.tenants.iter().enumerate().position(|(i, t)| t.org == org && t.app == app && !self.gone.contains(&i));
        tenant.and_then(|t| self.proc_in(t, proc)).unwrap_or(0)
    }

    /// The name an object of process `proc` goes by.
    pub fn object_name(&self, proc: usize, id: &str) -> String {
        let p = &self.procs[proc];
        format!("{}/{}/{id}", self.tenants[p.tenant].key(), p.name)
    }

    /// The node-wide name of pool `pool` of `proc`'s app (no manifest: as is).
    pub fn pool_key(&self, proc: usize, pool: &str) -> String {
        if self.router.legacy {
            return pool.to_string();
        }
        format!("{}/{pool}", self.tenants[self.procs[proc].tenant].key())
    }
}

impl Router {
    /// Most specific first: a host beats any host, then segment by segment
    /// a literal beats a capture beats the rest.
    fn new(mut routes: Vec<Target>, domains: Vec<Vec<String>>) -> Router {
        let key = |r: &Route| {
            let segs: Vec<u8> = r
                .segs
                .iter()
                .map(|s| match s {
                    Seg::Lit(_) => 2,
                    Seg::Param(_) => 1,
                    Seg::Rest => 0,
                })
                .collect();
            (r.host.is_some(), segs)
        };
        routes.sort_by(|a, b| key(&b.route).cmp(&key(&a.route)));
        Router { routes, domains, legacy: false }
    }

    pub fn legacy() -> Router {
        Router { routes: vec![], domains: vec![], legacy: true }
    }

    /// Routes straight from text, for tests: (route, process name, durable),
    /// all one app with no domains.
    #[cfg(test)]
    pub fn from(routes: &[(&str, &str, bool)]) -> Router {
        let routes = routes.iter().enumerate().map(|(i, (r, n, d))| Target { route: crate::manifest::parse_route(r).unwrap(), proc: i, tenant: 0, name: n.to_string(), durable: *d }).collect();
        Router::new(routes, vec![vec![]])
    }

    /// The route for `host` and `path` (no query string), if any. A route
    /// with no host matches only its app's domains (any host, for an app
    /// that names none: one app alone on a node).
    pub fn route(&self, host: &str, path: &str) -> Option<Hit> {
        let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let parts: &[&str] = if parts == [""] { &[] } else { &parts };
        'routes: for t in &self.routes {
            let r = &t.route;
            match &r.host {
                Some(h) if *h != host => continue,
                None if !self.domains[t.tenant].is_empty() && !self.domains[t.tenant].contains(&host) => continue,
                _ => {}
            }
            let (mut id, mut rest) = (None, None);
            for (i, seg) in r.segs.iter().enumerate() {
                match seg {
                    Seg::Rest => {
                        rest = Some(format!("/{}", parts[i.min(parts.len())..].join("/")));
                        break;
                    }
                    _ if i >= parts.len() => continue 'routes,
                    Seg::Lit(l) if l != parts[i] => continue 'routes,
                    Seg::Lit(_) => {}
                    Seg::Param(n) => {
                        if parts[i].is_empty() {
                            continue 'routes;
                        }
                        if n == "id" {
                            id = Some(parts[i]);
                        }
                    }
                }
            }
            if rest.is_none() && r.segs.len() != parts.len() {
                continue;
            }
            return Some(if t.durable {
                Hit::Object(format!("{}/{}", t.name, id?), rest.unwrap_or_else(|| "/".into()))
            } else {
                Hit::Cell(t.proc)
            });
        }
        None
    }
}
