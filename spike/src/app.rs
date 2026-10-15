//! An app at run time: one program per process kind, its processes, and the
//! routes that reach them (from the manifest). With no manifest it's one
//! program and the old fixed routes: `/do/<name>` for objects, everything
//! else for the cell.

use crate::manifest::{Resolved, Route, Seg};
use std::path::Path;
use std::time::Duration;

pub struct Proc {
    pub name: String,
    /// Index into `App::progs`.
    pub prog: usize,
    pub durable: bool,
    pub max_concurrent: usize,
    pub timeout: Duration,
    /// Processes that may `call` or `send` to this one.
    pub callers: Vec<usize>,
    /// Pools this one may lease from.
    pub owns: Vec<String>,
}

pub struct App<P> {
    pub progs: Vec<P>,
    pub procs: Vec<Proc>,
    pub router: Router,
}

/// Where a request goes. Non-generic, so the (fuzzed) parser can use it.
pub struct Router {
    /// Most specific first.
    routes: Vec<Target>,
    /// No manifest: the old fixed routes, all to process 0.
    pub legacy: bool,
}

struct Target {
    route: Route,
    proc: usize,
    name: String,
    durable: bool,
}

pub enum Hit {
    /// A worker process, and the whole path.
    Cell(usize),
    /// A durable process's object `<process>/<id>`, and the path inside it.
    Object(String, String),
}

impl<P> App<P> {
    /// One program, the old routes.
    pub fn single(prog: P, max_concurrent: usize) -> App<P> {
        let proc = Proc { name: "main".into(), prog: 0, durable: false, max_concurrent, timeout: Duration::from_secs(30), callers: vec![], owns: vec![] };
        App { progs: vec![prog], procs: vec![proc], router: Router { routes: vec![], legacy: true } }
    }

    /// The app in `dir`, from its resolved manifest: each kind's module
    /// loaded once (`load`), each process with its settings and its rights.
    pub fn from_manifest(dir: &Path, r: &Resolved, load: impl Fn(&[u8]) -> Result<P, String>) -> Result<App<P>, String> {
        let mut kinds: Vec<String> = vec![];
        let mut progs = vec![];
        let user: Vec<(&String, &crate::manifest::Process)> = r.processes.iter().filter(|(_, p)| !p.kind.starts_with("combd:")).collect();
        let index = |n: &str| user.iter().position(|(name, _)| *name == n);
        let mut procs = vec![];
        for (name, p) in &user {
            let prog = match kinds.iter().position(|k| *k == p.kind) {
                Some(i) => i,
                None => {
                    let path = crate::manifest::module_path(dir, &p.kind);
                    let wasm = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                    progs.push(load(&wasm).map_err(|e| format!("modules/{}.wasm: {e}", p.kind))?);
                    kinds.push(p.kind.clone());
                    kinds.len() - 1
                }
            };
            let n = |k: &str| p.settings[k].as_u64().unwrap_or(0);
            procs.push(Proc {
                name: (*name).clone(),
                prog,
                durable: p.settings["durable"].as_bool() == Some(true),
                max_concurrent: n("max_concurrent").max(1) as usize,
                timeout: Duration::from_millis(n("timeout_ms")),
                callers: p.callers.iter().filter_map(|c| index(c)).collect(),
                owns: r.pools.iter().filter(|(_, pool)| pool.owners.contains(name)).map(|(n, _)| n.clone()).collect(),
            });
        }
        let mut routes: Vec<Target> = user
            .iter()
            .enumerate()
            .flat_map(|(i, (name, p))| p.routes.iter().map(move |r| Target { route: r.clone(), proc: i, name: (*name).clone(), durable: p.settings["durable"].as_bool() == Some(true) }))
            .collect();
        Ok(App { progs, procs, router: Router::new(routes) })
    }

    pub fn proc_named(&self, name: &str) -> Option<usize> {
        self.procs.iter().position(|p| p.name == name)
    }

    /// The process an object belongs to: `<process>/<id>`, or 0 (no manifest).
    pub fn object_proc(&self, obj: &str) -> usize {
        if self.router.legacy {
            return 0;
        }
        obj.split_once('/').and_then(|(p, _)| self.proc_named(p)).unwrap_or(0)
    }
}

impl Router {
    /// Most specific first: a host beats any host, then segment by segment
    /// a literal beats a capture beats the rest.
    fn new(mut routes: Vec<Target>) -> Router {
        let key = |r: &Route| {
            let segs: Vec<u8> = r.segs.iter().map(|s| match s {
                Seg::Lit(_) => 2,
                Seg::Param(_) => 1,
                Seg::Rest => 0,
            }).collect();
            (r.host.is_some(), segs)
        };
        routes.sort_by(|a, b| key(&b.route).cmp(&key(&a.route)));
        Router { routes, legacy: false }
    }

    pub fn legacy() -> Router {
        Router { routes: vec![], legacy: true }
    }

    /// Routes straight from text, for tests: (route, process name, durable).
    #[cfg(test)]
    pub fn from(routes: &[(&str, &str, bool)]) -> Router {
        Router::new(routes.iter().enumerate().map(|(i, (r, n, d))| Target { route: crate::manifest::parse_route(r).unwrap(), proc: i, name: n.to_string(), durable: *d }).collect())
    }

    /// The route for `host` and `path` (no query string), if any.
    pub fn route(&self, host: &str, path: &str) -> Option<Hit> {
        let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let parts: &[&str] = if parts == [""] { &[] } else { &parts };
        'routes: for t in &self.routes {
            let r = &t.route;
            if r.host.as_ref().is_some_and(|h| *h != host) {
                continue;
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
