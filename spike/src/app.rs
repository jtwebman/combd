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

use crate::manifest::{Resolved, Route, Seg};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub struct Proc {
    pub name: String,
    /// Its app (`App::tenants`).
    pub tenant: usize,
    /// Index into `App::progs`.
    pub prog: usize,
    pub durable: bool,
    pub max_concurrent: usize,
    pub timeout: Duration,
    /// Processes (of its app) that may `call` or `send` to this one.
    pub callers: Vec<usize>,
    /// Pools (of its app) this one may lease from.
    pub owns: Vec<String>,
}

/// One app of one org.
pub struct Tenant {
    pub org: String,
    pub app: String,
    pub domains: Vec<String>,
    /// Its env vars, as WASI hands them out.
    pub environ: Arc<Vec<Vec<u8>>>,
}

impl Tenant {
    /// `<org>/<app>`: the prefix of everything it owns.
    pub fn key(&self) -> String {
        format!("{}/{}", self.org, self.app)
    }
}

pub struct App<P> {
    pub progs: Vec<P>,
    pub procs: Vec<Proc>,
    pub tenants: Vec<Tenant>,
    pub router: Router,
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
        let proc = Proc { name: "main".into(), tenant: 0, prog: 0, durable: false, max_concurrent, timeout: Duration::from_secs(30), callers: vec![], owns: vec![] };
        let tenant = Tenant { org: String::new(), app: "main".into(), domains: vec![], environ: Arc::default() };
        App { progs: vec![prog], procs: vec![proc], tenants: vec![tenant], router: Router::legacy() }
    }

    /// Every app, from its resolved manifest: each module loaded once (the
    /// same bytes in two apps are one program), each process with its
    /// settings and rights, all scoped to its app.
    pub fn from_apps(apps: &[AppDir], load: impl Fn(&[u8]) -> Result<P, String>) -> Result<App<P>, String> {
        let mut progs = vec![];
        let mut by_hash: HashMap<[u8; 32], usize> = HashMap::new();
        let (mut procs, mut tenants, mut targets) = (vec![], vec![], vec![]);
        let mut owner_of: HashMap<String, String> = HashMap::new();
        for a in apps {
            let t = tenants.len();
            let key = format!("{}/{}", a.org, a.app);
            if tenants.iter().any(|x: &Tenant| x.key() == key) {
                return Err(format!("{key}: two apps with the same name in one org"));
            }
            for d in a.domains {
                if let Some(other) = owner_of.insert(d.clone(), key.clone()) {
                    return Err(format!("{key}: domain {d} is already {other}'s"));
                }
            }
            if apps.len() > 1 && a.domains.is_empty() {
                return Err(format!("{key}: with more than one app, each one names its domains ([app] domains)"));
            }
            let environ = a.resolved.environ.iter().map(|(k, v)| format!("{k}={v}").into_bytes()).collect();
            tenants.push(Tenant { org: a.org.into(), app: a.app.into(), domains: a.domains.to_vec(), environ: Arc::new(environ) });
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
                        progs.push(load(&wasm).map_err(|e| format!("{key}: modules/{}.wasm: {e}", p.kind))?);
                        by_hash.insert(hash, progs.len() - 1);
                        progs.len() - 1
                    }
                };
                let n = |k: &str| p.settings[k].as_u64().unwrap_or(0);
                let durable = p.settings["durable"].as_bool() == Some(true);
                let i = procs.len();
                procs.push(Proc {
                    name: (*name).clone(),
                    tenant: t,
                    prog,
                    durable,
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
                    targets.push(Target { route: r.clone(), proc: i, tenant: t, name: format!("{key}/{name}"), durable });
                }
            }
        }
        let domains = tenants.iter().map(|t| t.domains.clone()).collect();
        Ok(App { progs, procs, tenants, router: Router::new(targets, domains) })
    }

    /// Process `name` of app `tenant`: a call or a lease never leaves its app.
    pub fn proc_in(&self, tenant: usize, name: &str) -> Option<usize> {
        self.procs.iter().position(|p| p.tenant == tenant && p.name == name)
    }

    /// The process an object belongs to: `<org>/<app>/<process>/<id>`, or 0
    /// (no manifest).
    pub fn object_proc(&self, obj: &str) -> usize {
        if self.router.legacy {
            return 0;
        }
        let mut parts = obj.splitn(4, '/');
        let (Some(org), Some(app), Some(proc)) = (parts.next(), parts.next(), parts.next()) else { return 0 };
        let tenant = self.tenants.iter().position(|t| t.org == org && t.app == app);
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
