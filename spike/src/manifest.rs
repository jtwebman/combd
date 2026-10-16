//! An app's manifest: `combd.toml`, `combd.yaml`, or `combd.json` (exactly
//! one), shipped with its code. All three read into the same tree and get the
//! same checks, and every mistake is reported at once. Any mistake and
//! nothing starts.
//!
//! ```toml
//! [app]
//! name = "shop"
//!
//! [vars]
//! LOG_LEVEL = "info"                          # a default; its type is string
//! PG_MAX = { type = "int", min = 1, default = 8 }
//! REGION = { type = "string" }                # no default: every env sets it
//! "MY_FLAG?" = { type = "bool" }              # ? = optional
//!
//! [secrets]
//! PG_MAIN_URL = { secret = "pg-url", type = "url" }   # env var -> secret name
//! "SIGNING_KEY?" = "jwt-key"                          # secrets never have defaults
//!
//! [processes.pg-conn]
//! kind = "combd:postgres"
//! url = "$PG_MAIN_URL"
//! connection.timeout_ms = 3000
//!
//! [pools.main]
//! member = "pg-conn"
//! max = "$PG_MAX"
//! ```
//!
//! Types: `string` (with an optional `pattern`), `int` (i64), `float` (f64),
//! `bool`, `datetime` (RFC 3339, offset required), `url`, and `enum`. `int`
//! and `float` take `min` and `max`. A process kind lists its settings the
//! same way. A `$NAME` setting must be exactly the type the setting wants:
//! no conversions. `$$` is a literal `$`.

use serde_json::{Map, Value as Json};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Str,
    Int,
    Float,
    Bool,
    Datetime,
    Url,
    Enum(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub ty: Ty,
    pub default: Option<Json>,
    pub optional: bool,
    /// For int and float: the value. For string: its length in characters.
    /// For datetime: a datetime.
    min: Option<Json>,
    max: Option<Json>,
    pattern: Option<regex::Regex>,
    /// Secrets: the name in the secret store.
    pub secret: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Var,
    Secret,
    Setting,
}

#[derive(Debug)]
pub struct Manifest {
    pub app: String,
    /// The hosts its routes answer on. With more than one app on a node,
    /// every app names its own, and no two share one.
    pub domains: Vec<String>,
    pub vars: BTreeMap<String, Spec>,
    pub secrets: BTreeMap<String, Spec>,
    pub processes: BTreeMap<String, Process>,
    pub pools: BTreeMap<String, Pool>,
}

/// A process: its kind, its settings (by dotted key), and who may reach it.
#[derive(Debug, PartialEq, Clone)]
pub struct Process {
    /// `combd:...` for a built-in kind, else the app's module `modules/<kind>.wasm`.
    pub kind: String,
    pub settings: BTreeMap<String, Json>,
    /// Who may reach it from the internet.
    pub routes: Vec<Route>,
    /// Which processes may `call` or `send` to it.
    pub callers: Vec<String>,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Pool {
    pub settings: BTreeMap<String, Json>,
    /// Which processes may hold a lease.
    pub owners: Vec<String>,
}

/// `host/path` or `/path` (any host). A `:name` segment captures it, and a
/// final `*` takes the rest.
#[derive(Debug, PartialEq, Clone)]
pub struct Route {
    pub text: String,
    pub host: Option<String>,
    pub segs: Vec<Seg>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Seg {
    Lit(String),
    Param(String),
    Rest,
}

/// One environment's values: vars by env var name, secrets by secret name.
#[derive(Default)]
pub struct EnvValues {
    pub vars: BTreeMap<String, Json>,
    pub secrets: BTreeMap<String, String>,
}

/// Everything resolved for one environment, checked, defaults filled in.
#[derive(Debug, PartialEq)]
pub struct Resolved {
    /// What processes see as env vars.
    pub environ: Vec<(String, String)>,
    pub processes: BTreeMap<String, Process>,
    pub pools: BTreeMap<String, Pool>,
}

/// The settings each built-in kind takes, in the same spec format.
fn schema(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "combd:postgres" => {
            r#"
            url = { type = "url" }
            "connection.timeout_ms" = { type = "int", min = 1, max = 600000, default = 5000 }
            "connection.retries" = { type = "int", min = 0, max = 10, default = 3 }
            statement_timeout_ms = { type = "int", min = 1, max = 3600000, default = 5000 }
            "#
        }
        // What every process of the app's own kinds takes.
        "combd:user" => {
            r#"
            timeout_ms = { type = "int", min = 1, max = 900000, default = 30000 }
            max_concurrent = { type = "int", min = 1, max = 1000, default = 32 }
            durable = { type = "bool", default = false }
            "#
        }
        "combd:pool" => {
            r#"
            member = { type = "string" }
            max = { type = "int", min = 1, max = 10000, default = 8 }
            wait_ms = { type = "int", min = 1, max = 600000, default = 5000 }
            lease_ms = { type = "int", min = 1, max = 86400000, default = 60000 }
            idle_ms = { type = "int", min = 0, max = 86400000, default = 60000 }
            session_idle_ms = { type = "int", min = 1, max = 86400000, default = 60000 }
            on_manager_loss = { type = "enum", values = ["finish", "stop"], default = "finish" }
            "#
        }
        _ => return None,
    })
}

/// Kinds a pool can hand out.
const POOLABLE: [&str; 1] = ["combd:postgres"];

fn settings_of(kind: &str) -> BTreeMap<String, Spec> {
    let t: Json = toml::from_str(schema(kind).unwrap()).unwrap();
    t.as_object().unwrap().iter().map(|(k, v)| (k.clone(), spec(Side::Setting, k, v).unwrap().1)).collect()
}

const FORMATS: [&str; 3] = ["toml", "yaml", "json"];

/// Hard caps; a spec's own `max` can only go lower. One value fits a PEM
/// certificate chain (Cloudflare allows 5 KB a var, Heroku 32 KB in all).
const MAX_VALUE: usize = 16 * 1024;
const MAX_ENVIRON: usize = 256 * 1024;
const MAX_NAMES: usize = 512;
const MAX_FILE: u64 = 1 << 20;
const MAX_PATTERN: usize = 1024;

/// The one file named `stem.{toml,yaml,json}` in `dir`, read into a tree.
fn read_one(dir: &Path, stem: &str) -> Result<Option<(String, Json)>, String> {
    let found: Vec<&str> = FORMATS.into_iter().filter(|k| dir.join(format!("{stem}.{k}")).exists()).collect();
    let k = match found[..] {
        [] => return Ok(None),
        [k] => k,
        _ => return Err(format!("more than one of {}: keep one", found.iter().map(|k| format!("{stem}.{k}")).collect::<Vec<_>>().join(", "))),
    };
    let file = format!("{stem}.{k}");
    if std::fs::metadata(dir.join(&file)).map_err(|e| format!("{file}: {e}"))?.len() > MAX_FILE {
        return Err(format!("{file}: bigger than {MAX_FILE} bytes"));
    }
    let text = std::fs::read_to_string(dir.join(&file)).map_err(|e| format!("{file}: {e}"))?;
    tree_of(k, &text).map(|t| Some((file.clone(), t))).map_err(|e| format!("{file}: {e}"))
}

/// A manifest's text, in format `k` (toml, yaml, json), as one tree.
fn tree_of(k: &str, text: &str) -> Result<Json, String> {
    let tree = match k {
        "toml" => toml::from_str::<Json>(text).map_err(|e| e.to_string()),
        "yaml" => serde_yaml_ng::from_str::<Json>(text).map_err(|e| e.to_string()),
        _ => serde_json::from_str::<Json>(text).map_err(|e| e.to_string()),
    };
    tree.map(untoml)
}

/// A TOML datetime written bare comes through as a one-key table: make it
/// the string it is.
fn untoml(v: Json) -> Json {
    match v {
        Json::Object(m) if m.len() == 1 && m.contains_key("$__toml_private_datetime") => m.into_iter().next().unwrap().1,
        Json::Object(m) => Json::Object(m.into_iter().map(|(k, v)| (k, untoml(v))).collect()),
        Json::Array(a) => Json::Array(a.into_iter().map(untoml).collect()),
        v => v,
    }
}

/// Read and check the manifest in `dir` (`None`: there isn't one). The
/// app's own kinds are its modules: `modules/<kind>.wasm`.
pub fn load(dir: &Path) -> Result<Option<Manifest>, String> {
    let Some((file, tree)) = read_one(dir, "combd")? else { return Ok(None) };
    let has_module = |k: &str| module_path(dir, k).is_file();
    from_tree(&tree, &has_module).map(Some).map_err(|errs| format!("{file}:\n  {}", errs.join("\n  ")))
}

pub fn module_path(dir: &Path, kind: &str) -> std::path::PathBuf {
    dir.join("modules").join(format!("{kind}.wasm"))
}

/// The local `dev` environment: `combd.dev.{toml,yaml,json}` beside the
/// manifest, with `[vars]` and `[secrets]` (by secret name). It holds
/// secrets, so git must not track it.
pub fn load_dev(dir: &Path) -> Result<EnvValues, String> {
    let Some((file, tree)) = read_one(dir, "combd.dev")? else { return Ok(EnvValues::default()) };
    let tracked = std::process::Command::new("git").args(["ls-files", "--error-unmatch", &file]).current_dir(dir).output().is_ok_and(|o| o.status.success());
    if tracked {
        return Err(format!("{file} holds secrets, and git tracks it: git rm --cached {file}, and add it to .gitignore"));
    }
    let obj = tree.as_object().ok_or(format!("{file}: want a table"))?;
    let mut env = EnvValues::default();
    for (k, v) in obj {
        match (k.as_str(), v) {
            ("vars", Json::Object(m)) => env.vars = m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            ("secrets", Json::Object(m)) => {
                for (k, v) in m {
                    let s = v.as_str().ok_or(format!("{file}: secrets.{k}: want a string"))?;
                    env.secrets.insert(k.clone(), s.to_string());
                }
            }
            _ => return Err(format!("{file}: {k}: only [vars] and [secrets] go here")),
        }
    }
    Ok(env)
}

fn from_tree(tree: &Json, has_module: &dyn Fn(&str) -> bool) -> Result<Manifest, Vec<String>> {
    let mut errs = vec![];
    let empty = Map::new();
    let top = match tree.as_object() {
        Some(t) => t,
        None => return Err(vec!["want a table at the top".into()]),
    };
    for k in top.keys() {
        if !["app", "vars", "secrets", "processes", "pools"].contains(&k.as_str()) {
            errs.push(format!("{k}: unknown section (app, vars, secrets, processes, pools)"));
        }
    }
    let table = |k: &str, errs: &mut Vec<String>| -> Map<String, Json> {
        match top.get(k) {
            None => empty.clone(),
            Some(Json::Object(m)) => m.clone(),
            Some(_) => {
                errs.push(format!("{k}: want a table"));
                empty.clone()
            }
        }
    };
    let app = table("app", &mut errs);
    let name = app.get("name").and_then(Json::as_str).unwrap_or_default().to_string();
    if !ident(&name) {
        errs.push(format!("app.name {name:?}: use lowercase letters, digits, - and _ (at most 63)"));
    }
    for k in app.keys().filter(|k| *k != "name" && *k != "domains") {
        errs.push(format!("app.{k}: unknown key"));
    }
    let mut domains = vec![];
    match app.get("domains") {
        None => {}
        Some(Json::Array(list)) => {
            for d in list {
                let ok = d.as_str().filter(|h| parse_route(&format!("{h}/")).is_ok_and(|r| r.host.is_some()));
                match ok {
                    Some(h) if !domains.iter().any(|x: &String| x == h) => domains.push(h.to_string()),
                    Some(h) => errs.push(format!("app.domains: {h} twice")),
                    None => errs.push(format!("app.domains: {d}: want a lowercase host name like shop.example.com")),
                }
            }
        }
        Some(_) => errs.push("app.domains: want a list of host names".into()),
    }
    let specs = |side: Side, sect: &str, errs: &mut Vec<String>| -> BTreeMap<String, Spec> {
        let mut out = BTreeMap::new();
        for (k, v) in table(sect, errs) {
            match spec(side, &k, &v) {
                Ok((n, s)) => {
                    out.insert(n, s);
                }
                Err(e) => errs.push(format!("{sect}.{k}: {e}")),
            }
        }
        out
    };
    let vars = specs(Side::Var, "vars", &mut errs);
    let secrets = specs(Side::Secret, "secrets", &mut errs);
    if vars.len() + secrets.len() > MAX_NAMES {
        errs.push(format!("{} vars and secrets: at most {MAX_NAMES}", vars.len() + secrets.len()));
    }
    for n in vars.keys().filter(|n| secrets.contains_key(*n)) {
        errs.push(format!("{n} is in both [vars] and [secrets]: pick one"));
    }
    let names: BTreeMap<String, Spec> = vars.iter().chain(&secrets).map(|(k, v)| (k.clone(), v.clone())).collect();
    let procs = table("processes", &mut errs);
    // The app's own processes: the only ones that can call, or hold a lease.
    let user = |n: &str| procs.get(n).and_then(|p| p.get("kind")).and_then(Json::as_str).is_some_and(|k| !k.starts_with("combd:"));
    let names_of = |at: &str, v: Option<Json>, errs: &mut Vec<String>| -> Vec<String> {
        let Some(v) = v else { return vec![] };
        let Some(list) = v.as_array().and_then(|a| a.iter().map(|x| x.as_str().map(String::from)).collect::<Option<Vec<_>>>()) else {
            errs.push(format!("{at}: want a list of process names"));
            return vec![];
        };
        for n in &list {
            if !procs.contains_key(n) {
                errs.push(format!("{at}: no process {n:?}"));
            } else if !user(n) {
                errs.push(format!("{at}: {n} is a built-in kind; only the app's own processes can do this"));
            }
        }
        list
    };
    let mut processes = BTreeMap::new();
    let mut seen_routes: BTreeMap<String, String> = BTreeMap::new();
    for (pname, p) in &procs {
        let at = format!("processes.{pname}");
        if !ident(pname) {
            errs.push(format!("{at}: use lowercase letters, digits, - and _ in names"));
        }
        let mut flat = BTreeMap::new();
        flatten("", p, &mut flat);
        let kind = flat.remove("kind").and_then(|k| k.as_str().map(String::from)).unwrap_or_default();
        let (routes_v, callers_v) = (flat.remove("routes"), flat.remove("callers"));
        let builtin = kind.starts_with("combd:");
        if builtin {
            if schema(&kind).is_none() || !POOLABLE.contains(&kind.as_str()) {
                errs.push(format!("{at}.kind {kind:?}: not a built-in kind (so far: combd:postgres)"));
                continue;
            }
            if routes_v.is_some() || callers_v.is_some() {
                errs.push(format!("{at}: {kind} is reached through a pool, so it takes no routes or callers"));
            }
        } else if !ident(&kind) {
            errs.push(format!("{at}.kind {kind:?}: name one of the app's modules (modules/<kind>.wasm), or a built-in combd: kind"));
            continue;
        } else if !has_module(&kind) {
            errs.push(format!("{at}.kind: no module modules/{kind}.wasm"));
        }
        let schema_kind = if builtin { kind.as_str() } else { "combd:user" };
        check_settings(&at, &settings_of(schema_kind), &flat, &names, &mut errs);
        let durable = flat.get("durable") == Some(&Json::Bool(true));
        if durable && flat.contains_key("max_concurrent") {
            errs.push(format!("{at}.max_concurrent: a durable object takes one event at a time"));
        }
        let mut routes = vec![];
        if let Some(v) = routes_v {
            match v.as_array().and_then(|a| a.iter().map(Json::as_str).collect::<Option<Vec<_>>>()) {
                None => errs.push(format!("{at}.routes: want a list of routes")),
                Some(list) => {
                    for r in list {
                        match parse_route(r) {
                            Ok(route) => {
                                if durable && !route.segs.contains(&Seg::Param("id".into())) {
                                    errs.push(format!("{at}.routes: {r:?}: a durable object's route names it with :id"));
                                }
                                if let Some(other) = seen_routes.insert(shape(&route), pname.clone()) {
                                    errs.push(format!("{at}.routes: {r:?} is the same route as one of {other}'s"));
                                }
                                routes.push(route);
                            }
                            Err(e) => errs.push(format!("{at}.routes: {r:?}: {e}")),
                        }
                    }
                }
            }
        }
        let callers = names_of(&format!("{at}.callers"), callers_v, &mut errs);
        processes.insert(pname.clone(), Process { kind, settings: flat, routes, callers });
    }
    let mut pools = BTreeMap::new();
    for (name, p) in table("pools", &mut errs) {
        let at = format!("pools.{name}");
        if !ident(&name) {
            errs.push(format!("{at}: use lowercase letters, digits, - and _ in names"));
        }
        let mut flat = BTreeMap::new();
        flatten("", &p, &mut flat);
        let owners_v = flat.remove("owners");
        if owners_v.is_none() {
            errs.push(format!("{at}.owners: required: which processes may hold a lease"));
        }
        let owners = names_of(&format!("{at}.owners"), owners_v, &mut errs);
        check_settings(&at, &settings_of("combd:pool"), &flat, &names, &mut errs);
        if let Some(m) = flat.get("member").and_then(Json::as_str) {
            match processes.get(m) {
                Some(p) if POOLABLE.contains(&p.kind.as_str()) => {}
                Some(p) => errs.push(format!("{at}.member: {m} is {}, which a pool can't hand out", p.kind)),
                None => errs.push(format!("{at}.member: no process {m:?}")),
            }
        }
        pools.insert(name, Pool { settings: flat, owners });
    }
    // Nothing declared that nothing can reach.
    for (n, p) in &processes {
        let reachable = if p.kind.starts_with("combd:") {
            pools.values().any(|pool| pool.settings.get("member").and_then(Json::as_str) == Some(n))
        } else {
            !p.routes.is_empty() || !p.callers.is_empty()
        };
        if !reachable && !p.kind.is_empty() {
            errs.push(format!("processes.{n}: nothing can reach it ({})", if p.kind.starts_with("combd:") { "no pool hands it out" } else { "no routes, and no callers" }));
        }
    }
    for p in processes.values() {
        for r in &p.routes {
            if let Some(h) = r.host.as_ref().filter(|h| !domains.is_empty() && !domains.contains(h)) {
                errs.push(format!("route {}: {h} isn't one of app.domains", r.text));
            }
        }
    }
    if errs.is_empty() {
        Ok(Manifest { app: name, domains, vars, secrets, processes, pools })
    } else {
        Err(errs)
    }
}

pub(crate) fn parse_route(r: &str) -> Result<Route, String> {
    if r.len() > 256 {
        return Err("longer than 256 bytes".into());
    }
    let (host, path) = match r.find('/') {
        Some(0) => (None, r),
        Some(i) => (Some(&r[..i]), &r[i..]),
        None => return Err("want host/path or /path".into()),
    };
    if let Some(h) = host {
        let label_ok = |l: &str| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if h.len() > 253 || !h.split('.').all(label_ok) {
            return Err(format!("{h:?}: want a lowercase host name like shop.example.com"));
        }
    }
    let mut segs = vec![];
    let parts: Vec<&str> = path[1..].split('/').collect();
    for (i, seg) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        match *seg {
            "" if last => {}
            "" => return Err("an empty segment (//)".into()),
            "*" if last => segs.push(Seg::Rest),
            "*" => return Err("* goes only at the end".into()),
            p if p.starts_with(':') => {
                let n = &p[1..];
                if !ident(n) {
                    return Err(format!("{p}: name it with lowercase letters, digits, - and _"));
                }
                if segs.contains(&Seg::Param(n.into())) {
                    return Err(format!("{p} twice"));
                }
                segs.push(Seg::Param(n.into()));
            }
            p if p.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)) => segs.push(Seg::Lit(p.into())),
            p => return Err(format!("{p:?}: use letters, digits, - . _ ~ in a path")),
        }
    }
    Ok(Route { text: r.to_string(), host: host.map(String::from), segs })
}

/// Two routes with the same shape match the same requests.
fn shape(r: &Route) -> String {
    let segs: Vec<String> = r.segs.iter().map(|s| match s {
        Seg::Lit(l) => l.clone(),
        Seg::Param(_) => ":".into(),
        Seg::Rest => "*".into(),
    }).collect();
    format!("{}/{}", r.host.as_deref().unwrap_or(""), segs.join("/"))
}

fn ident(s: &str) -> bool {
    !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn env_name(s: &str) -> bool {
    s.bytes().next().is_some_and(|b| b.is_ascii_uppercase() || b == b'_') && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Nested tables to dotted keys: `connection.timeout_ms`.
fn flatten(pre: &str, v: &Json, out: &mut BTreeMap<String, Json>) {
    match v {
        Json::Object(m) => {
            for (k, v) in m {
                flatten(&if pre.is_empty() { k.clone() } else { format!("{pre}.{k}") }, v, out);
            }
        }
        v => {
            out.insert(pre.to_string(), v.clone());
        }
    }
}

/// One var, secret, or setting spec. Returns its name (without `?`).
fn spec(side: Side, key: &str, v: &Json) -> Result<(String, Spec), String> {
    let (name, optional) = match key.strip_suffix('?') {
        Some(n) => (n, true),
        None => (key, false),
    };
    if side != Side::Setting && !env_name(name) {
        return Err("env var names are A-Z, 0-9 and _, not starting with a digit".into());
    }
    if side != Side::Setting && name.starts_with("COMBD_") {
        return Err("COMBD_ names are the host's (COMBD_ORG, COMBD_APP, COMBD_PROCESS, COMBD_NODE)".into());
    }
    let mut s = Spec { ty: Ty::Str, default: None, optional, min: None, max: None, pattern: None, secret: None };
    let infer = |v: &Json| match v {
        Json::String(_) => Some(Ty::Str),
        Json::Bool(_) => Some(Ty::Bool),
        Json::Number(n) if n.is_i64() => Some(Ty::Int),
        Json::Number(_) => Some(Ty::Float),
        _ => None,
    };
    match (side, v) {
        (Side::Secret, Json::String(n)) => s.secret = Some(n.clone()),
        (_, Json::Object(m)) => {
            let mut ty = None;
            for (k, x) in m {
                match k.as_str() {
                    "type" => ty = Some(x.as_str().ok_or("type: want a string")?.to_string()),
                    "default" if side == Side::Secret => return Err("secrets never have defaults: a default would be in git, and in every environment".into()),
                    "default" => s.default = Some(x.clone()),
                    "secret" if side == Side::Secret => s.secret = Some(x.as_str().ok_or("secret: want the secret's name")?.to_string()),
                    "min" => s.min = Some(x.clone()),
                    "max" => s.max = Some(x.clone()),
                    "pattern" => {
                        let p = x.as_str().ok_or("pattern: want a string")?;
                        if p.len() > MAX_PATTERN {
                            return Err(format!("pattern: longer than {MAX_PATTERN} bytes"));
                        }
                        s.pattern = Some(regex::RegexBuilder::new(p).size_limit(1 << 20).build().map_err(|e| format!("pattern: {e}"))?);
                    }
                    "values" => {
                        let vs = x.as_array().filter(|a| !a.is_empty()).ok_or("values: want a list of strings")?;
                        s.ty = Ty::Enum(vs.iter().map(|v| v.as_str().map(String::from).ok_or("values: want a list of strings")).collect::<Result<_, _>>()?);
                    }
                    "description" => {}
                    k => return Err(format!("{k}: unknown key (type, default, min, max, pattern, values, description{})", if side == Side::Secret { ", secret" } else { "" })),
                }
            }
            s.ty = match ty.as_deref() {
                Some("string") => Ty::Str,
                Some("int") => Ty::Int,
                Some("float") => Ty::Float,
                Some("bool") => Ty::Bool,
                Some("datetime") => Ty::Datetime,
                Some("url") => Ty::Url,
                Some("enum") if matches!(s.ty, Ty::Enum(_)) => s.ty.clone(),
                Some("enum") => return Err("an enum needs values".into()),
                Some(t) => return Err(format!("type {t:?}: string, int, float, bool, datetime, url, or enum")),
                None if side == Side::Secret => Ty::Str,
                None => match &s.default {
                    Some(d) => infer(d).ok_or("say its type")?,
                    None => return Err("no default, so say its type".into()),
                },
            };
            if matches!(s.ty, Ty::Enum(_)) != ty.as_deref().is_some_and(|t| t == "enum") {
                return Err("values go with type = \"enum\"".into());
            }
            if side == Side::Secret && s.secret.is_none() {
                return Err("name the secret it comes from: secret = \"...\"".into());
            }
        }
        (Side::Secret, _) => return Err("want the secret's name, or a table with secret = \"...\"".into()),
        (_, d) => {
            s.ty = infer(d).ok_or("no default, so say its type: { type = \"...\" }")?;
            s.default = Some(d.clone());
        }
    }
    if s.secret.as_deref().is_some_and(|n| !crate::secrets::valid_name(n)) {
        return Err("secret names are a-z, 0-9, -, _ and ., up to 128".into());
    }
    for (k, b) in [("min", &s.min), ("max", &s.max)] {
        let Some(b) = b else { continue };
        let ok = match s.ty {
            Ty::Int => b.is_i64(),
            Ty::Float => b.is_number(),
            Ty::Str => b.is_u64(),
            Ty::Datetime => b.as_str().is_some_and(|t| instant(t).is_some()),
            _ => return Err("min and max go with int, float, string (its length), and datetime".into()),
        };
        if !ok {
            return Err(format!("{k}: want {}", match s.ty {
                Ty::Str => "a length (a whole number, 0 or more)",
                Ty::Datetime => "a datetime like 2026-10-15T08:00:00Z",
                Ty::Int => "an int",
                _ => "a number",
            }));
        }
    }
    if s.pattern.is_some() && s.ty != Ty::Str {
        return Err("pattern goes with string".into());
    }
    if s.optional && s.default.is_some() {
        return Err("it has a default, so it's never missing: drop the ?".into());
    }
    if let Some(d) = &s.default {
        check_value(&s, d).map_err(|e| format!("default: {e}"))?;
    }
    Ok((name.to_string(), s))
}

fn ty_name(t: &Ty) -> &'static str {
    match t {
        Ty::Str => "string",
        Ty::Int => "int",
        Ty::Float => "float",
        Ty::Bool => "bool",
        Ty::Datetime => "datetime",
        Ty::Url => "url",
        Ty::Enum(_) => "enum",
    }
}

/// A value against its spec: its type, then its limits.
pub fn check_value(s: &Spec, v: &Json) -> Result<(), String> {
    let want = || format!("want {}, got {v}", ty_name(&s.ty));
    match &s.ty {
        Ty::Str | Ty::Datetime | Ty::Url => {
            let t = v.as_str().ok_or_else(want)?;
            if t.len() > MAX_VALUE {
                return Err(format!("{} bytes: at most {MAX_VALUE}", t.len()));
            }
            if s.ty == Ty::Datetime {
                let at = instant(t).ok_or_else(|| format!("{t:?}: want a datetime like 2026-10-15T08:00:00Z (RFC 3339, with an offset)"))?;
                let bound = |b: &Option<Json>| b.as_ref().and_then(Json::as_str).and_then(instant);
                if bound(&s.min).is_some_and(|m| at < m) || bound(&s.max).is_some_and(|m| at > m) {
                    return Err(format!("{t:?} is outside {} to {}", s.min.as_ref().and_then(Json::as_str).unwrap_or(".."), s.max.as_ref().and_then(Json::as_str).unwrap_or("..")));
                }
            }
            if s.ty == Ty::Str {
                let n = t.chars().count() as u64;
                if s.min.as_ref().and_then(Json::as_u64).is_some_and(|m| n < m) || s.max.as_ref().and_then(Json::as_u64).is_some_and(|m| n > m) {
                    let show = |b: &Option<Json>| b.as_ref().map_or("..".into(), |b| b.to_string());
                    return Err(format!("{n} characters, want {} to {}", show(&s.min), show(&s.max)));
                }
            }
            if s.ty == Ty::Url && !url(t) {
                return Err(format!("{t:?}: want a URL like scheme://host/..."));
            }
            if let Some(p) = &s.pattern {
                if !p.is_match(t) {
                    return Err(format!("{t:?} doesn't match {}", p.as_str()));
                }
            }
        }
        Ty::Int => {
            let n = v.as_i64().ok_or_else(want)?;
            limits(s, n as f64)?;
        }
        // JSON can't tell 1 from 1.0, so a whole number is a float too.
        Ty::Float => limits(s, v.as_f64().filter(|_| v.is_number()).ok_or_else(want)?)?,
        Ty::Bool => {
            v.as_bool().ok_or_else(want)?;
        }
        Ty::Enum(vs) => {
            let t = v.as_str().ok_or_else(want)?;
            if !vs.iter().any(|x| x == t) {
                return Err(format!("{t:?}: one of {}", vs.join(", ")));
            }
        }
    }
    Ok(())
}

fn limits(s: &Spec, n: f64) -> Result<(), String> {
    let b = |x: &Option<Json>| x.as_ref().and_then(Json::as_f64);
    if b(&s.min).is_some_and(|m| n < m) || b(&s.max).is_some_and(|m| n > m) {
        let show = |x: &Option<Json>| x.as_ref().map_or("..".into(), |x| x.to_string());
        return Err(format!("{n} is outside {} to {}", show(&s.min), show(&s.max)));
    }
    Ok(())
}

/// An RFC 3339 datetime (offset required) as nanoseconds since 1970 UTC,
/// so two can be compared whatever their offsets.
fn instant(t: &str) -> Option<i128> {
    let b = t.as_bytes();
    let num = |r: std::ops::Range<usize>| t.get(r).filter(|s| s.bytes().all(|c| c.is_ascii_digit())).and_then(|s| s.parse::<i64>().ok());
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d, h, mi, s) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&mo) || d == 0 || d > days[mo as usize - 1] || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let mut rest = &t[19..];
    let mut nanos: i128 = 0;
    if let Some(f) = rest.strip_prefix('.') {
        let n = f.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        let digits = &f[..n.min(9)];
        nanos = digits.parse::<i128>().ok()? * 10i128.pow(9 - digits.len() as u32);
        rest = &f[n..];
    }
    let offset = match rest.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let (oh, om) = (num(t.len() - 5..t.len() - 3)?, num(t.len() - 2..t.len())?);
            if oh > 23 || om > 59 {
                return None;
            }
            (oh * 60 + om) * if *sign == b'+' { 1 } else { -1 }
        }
        _ => return None,
    };
    // Days since 1970 (Howard Hinnant's days_from_civil).
    let (yy, mm) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + s - offset * 60;
    Some(secs as i128 * 1_000_000_000 + nanos)
}

fn url(t: &str) -> bool {
    t.split_once("://").is_some_and(|(scheme, rest)| {
        scheme.bytes().next().is_some_and(|b| b.is_ascii_alphabetic()) && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b)) && !rest.is_empty() && !rest.contains(char::is_whitespace)
    })
}

/// `"$NAME"`: a reference. `"$$..."`: a literal `$...`.
fn reference(v: &Json) -> Option<&str> {
    v.as_str().and_then(|s| s.strip_prefix('$')).filter(|n| !n.starts_with('$'))
}

fn literal(v: &Json) -> Json {
    match v.as_str().and_then(|s| s.strip_prefix("$$")) {
        Some(s) => Json::String(format!("${s}")),
        None => v.clone(),
    }
}

/// A kind's settings, as written: nothing unknown, nothing required missing,
/// literals of the right type, and references to declared names of the very
/// same type.
fn check_settings(at: &str, schema: &BTreeMap<String, Spec>, got: &BTreeMap<String, Json>, names: &BTreeMap<String, Spec>, errs: &mut Vec<String>) {
    for (k, v) in got {
        let Some(want) = schema.get(k) else {
            errs.push(format!("{at}.{k}: unknown setting (it takes {})", schema.keys().cloned().collect::<Vec<_>>().join(", ")));
            continue;
        };
        match reference(v) {
            Some(n) => match names.get(n) {
                None => errs.push(format!("{at}.{k}: ${n} isn't declared in [vars] or [secrets]")),
                Some(have) => {
                    let same = match (&want.ty, &have.ty) {
                        (Ty::Enum(w), Ty::Enum(h)) => h.iter().all(|x| w.contains(x)),
                        (w, h) => w == h,
                    };
                    if !same {
                        errs.push(format!("{at}.{k}: wants {}, ${n} is {}", ty_name(&want.ty), ty_name(&have.ty)));
                    } else if have.optional && want.default.is_none() && !want.optional {
                        errs.push(format!("{at}.{k}: needs a value, and ${n} is optional"));
                    }
                }
            },
            None => {
                if let Err(e) = check_value(want, &literal(v)) {
                    errs.push(format!("{at}.{k}: {e}"));
                }
            }
        }
    }
    for (k, s) in schema {
        if !s.optional && s.default.is_none() && !got.contains_key(k) {
            errs.push(format!("{at}.{k}: required"));
        }
    }
}

impl Manifest {
    /// A value for var `name`, checked against its type. Text (as a CLI
    /// sends it) is read as that type: `"8"` for an int is 8, `"lots"` is an
    /// error. A secret, or a name that isn't a var, is refused.
    pub fn var_value(&self, name: &str, v: Json) -> Result<Json, String> {
        if self.secrets.contains_key(name) {
            return Err(format!("{name} is a secret: set it as one"));
        }
        let s = self.vars.get(name).ok_or(format!("{name}: not declared in [vars]"))?;
        let v = match (&s.ty, v) {
            (Ty::Int, Json::String(t)) => t.trim().parse::<i64>().map(Json::from).map_err(|_| format!("{name}: want int, got {t:?}"))?,
            (Ty::Float, Json::String(t)) => t.trim().parse::<f64>().ok().and_then(|f| serde_json::Number::from_f64(f).map(Json::Number)).ok_or(format!("{name}: want float, got {t:?}"))?,
            (Ty::Bool, Json::String(t)) => match t.as_str() {
                "true" => Json::Bool(true),
                "false" => Json::Bool(false),
                _ => return Err(format!("{name}: want true or false, got {t:?}")),
            },
            (_, v) => v,
        };
        check_value(s, &v).map_err(|e| format!("{name}: {e}"))?;
        Ok(v)
    }

    /// Check a value for secret `name` (by the secret's name) against every
    /// env var it's for, or with `None`, that none needs it. Errors never
    /// show the value.
    pub fn secret_value(&self, name: &str, v: Option<&str>) -> Result<(), String> {
        let uses: Vec<(&String, &Spec)> = self.secrets.iter().filter(|(_, s)| s.secret.as_deref() == Some(name)).collect();
        if uses.is_empty() {
            return Err(format!("secret {name:?}: not declared in [secrets]"));
        }
        for (k, s) in uses {
            match v {
                Some(v) => check_value(s, &Json::String(v.into())).map_err(|_| format!("{k} (secret {name:?}): that isn't a valid {}", ty_name(&s.ty)))?,
                None if !s.optional => return Err(format!("{k} needs secret {name:?}: deploy a version that doesn't first")),
                None => {}
            }
        }
        Ok(())
    }

    /// Resolve for one environment: its values checked against their types,
    /// every required one set, and every setting checked again with real
    /// values (a var's limits may be wider than its setting's).
    pub fn resolve(&self, env: &EnvValues) -> Result<Resolved, String> {
        let mut errs = vec![];
        let mut values: BTreeMap<&str, Json> = BTreeMap::new();
        for (k, v) in &env.vars {
            match self.vars.get(k) {
                Some(s) => match check_value(s, v) {
                    Ok(()) => {
                        values.insert(k, v.clone());
                    }
                    Err(e) => errs.push(format!("{k}: {e}")),
                },
                None if self.secrets.contains_key(k) => errs.push(format!("{k} is a secret: set it with the secret's name")),
                None => errs.push(format!("{k}: not declared in [vars]")),
            }
        }
        for (k, s) in &self.vars {
            if !values.contains_key(k.as_str()) {
                match &s.default {
                    Some(d) => {
                        values.insert(k, d.clone());
                    }
                    None if !s.optional => errs.push(format!("{k}: required, and this environment doesn't set it")),
                    None => {}
                }
            }
        }
        for (k, s) in &self.secrets {
            let name = s.secret.as_deref().unwrap();
            match env.secrets.get(name) {
                Some(v) => match check_value(s, &Json::String(v.clone())) {
                    Ok(()) => {
                        values.insert(k, Json::String(v.clone()));
                    }
                    // Never show a secret's value.
                    Err(_) => errs.push(format!("{k} (secret {name:?}): its value isn't a valid {}", ty_name(&s.ty))),
                },
                None if !s.optional => errs.push(format!("{k}: required, and this environment has no secret {name:?}")),
                None => {}
            }
        }
        let fill = |at: &str, kind: &str, got: &BTreeMap<String, Json>, errs: &mut Vec<String>| -> BTreeMap<String, Json> {
            let schema = settings_of(kind);
            let mut out = BTreeMap::new();
            for (k, want) in &schema {
                let v = match got.get(k) {
                    Some(v) => match reference(v) {
                        Some(n) => values.get(n).cloned(),
                        None => Some(literal(v)),
                    },
                    None => None,
                };
                match v.or_else(|| want.default.clone()) {
                    Some(v) => {
                        if let Err(e) = check_value(want, &v) {
                            let secret = got.get(k).and_then(reference).is_some_and(|n| self.secrets.contains_key(n));
                            errs.push(if secret { format!("{at}.{k}: the secret isn't a valid {}", ty_name(&want.ty)) } else { format!("{at}.{k}: {e}") });
                        }
                        out.insert(k.clone(), v);
                    }
                    None if !want.optional => errs.push(format!("{at}.{k}: no value in this environment")),
                    None => {}
                }
            }
            out
        };
        let processes = self
            .processes
            .iter()
            .map(|(n, p)| {
                let kind = if p.kind.starts_with("combd:") { p.kind.as_str() } else { "combd:user" };
                (n.clone(), Process { settings: fill(&format!("processes.{n}"), kind, &p.settings, &mut errs), ..p.clone() })
            })
            .collect();
        let pools = self.pools.iter().map(|(n, p)| (n.clone(), Pool { settings: fill(&format!("pools.{n}"), "combd:pool", &p.settings, &mut errs), owners: p.owners.clone() })).collect();
        if !errs.is_empty() {
            return Err(errs.join("\n  "));
        }
        let text = |v: &Json| v.as_str().map_or_else(|| v.to_string(), String::from);
        let environ: Vec<(String, String)> = values.iter().map(|(k, v)| (k.to_string(), text(v))).collect();
        let size: usize = environ.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
        if size > MAX_ENVIRON {
            return Err(format!("the env vars come to {size} bytes: at most {MAX_ENVIRON}"));
        }
        Ok(Resolved { environ, processes, pools })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp app with these files, and modules `api`, `admin`, and `imager`.
    fn dir(files: &[(&str, &str)]) -> crate::store::tempdir::Dir {
        let d = crate::store::tempdir::Dir::new();
        std::fs::create_dir(d.0.join("modules")).unwrap();
        for m in ["api", "admin", "imager"] {
            std::fs::write(module_path(&d.0, m), b"").unwrap();
        }
        for (n, t) in files {
            std::fs::write(d.0.join(n), t).unwrap();
        }
        d
    }

    const TOML: &str = r#"
[app]
name = "shop"

[vars]
LOG_LEVEL = { type = "enum", values = ["debug", "info"], default = "info" }
PG_MAX = { type = "int", min = 1, max = 1000, default = 8 }
REGION = { type = "string", pattern = "^[a-z]{2}-[a-z]+$" }
"MY_FLAG?" = { type = "bool" }
STARTS = { type = "datetime", default = 2026-10-15T08:00:00Z }

[secrets]
PG_MAIN_URL = { secret = "pg-url", type = "url" }
"SIGNING_KEY?" = "jwt-key"

[processes.api]
kind = "api"
routes = ["shop.example.com/api/*"]
timeout_ms = 10000

[processes.pg-conn]
kind = "combd:postgres"
url = "$PG_MAIN_URL"
connection.timeout_ms = 3000

[pools.main]
member = "pg-conn"
owners = ["api"]
max = "$PG_MAX"
"#;

    const YAML: &str = r#"
app: { name: shop }
vars:
  LOG_LEVEL: { type: enum, values: [debug, info], default: info }
  PG_MAX: { type: int, min: 1, max: 1000, default: 8 }
  REGION: { type: string, pattern: "^[a-z]{2}-[a-z]+$" }
  MY_FLAG?: { type: bool }
  STARTS: { type: datetime, default: "2026-10-15T08:00:00Z" }
secrets:
  PG_MAIN_URL: { secret: pg-url, type: url }
  SIGNING_KEY?: jwt-key
processes:
  api: { kind: api, routes: [shop.example.com/api/*], timeout_ms: 10000 }
  pg-conn:
    kind: combd:postgres
    url: $PG_MAIN_URL
    connection: { timeout_ms: 3000 }
pools:
  main: { member: pg-conn, owners: [api], max: $PG_MAX }
"#;

    const JSON: &str = r#"{
  "app": {"name": "shop"},
  "vars": {
    "LOG_LEVEL": {"type": "enum", "values": ["debug", "info"], "default": "info"},
    "PG_MAX": {"type": "int", "min": 1, "max": 1000, "default": 8},
    "REGION": {"type": "string", "pattern": "^[a-z]{2}-[a-z]+$"},
    "MY_FLAG?": {"type": "bool"},
    "STARTS": {"type": "datetime", "default": "2026-10-15T08:00:00Z"}
  },
  "secrets": {"PG_MAIN_URL": {"secret": "pg-url", "type": "url"}, "SIGNING_KEY?": "jwt-key"},
  "processes": {
    "api": {"kind": "api", "routes": ["shop.example.com/api/*"], "timeout_ms": 10000},
    "pg-conn": {"kind": "combd:postgres", "url": "$PG_MAIN_URL", "connection": {"timeout_ms": 3000}}
  },
  "pools": {"main": {"member": "pg-conn", "owners": ["api"], "max": "$PG_MAX"}}
}"#;

    fn env(vars: &[(&str, Json)], secrets: &[(&str, &str)]) -> EnvValues {
        EnvValues { vars: vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), secrets: secrets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    /// Manifests are anyone's text: damaged at random, in all three formats,
    /// the parse and every check (and resolving it) must never panic.
    #[test]
    fn mangled_manifests_never_panic() {
        let e = env(&[("REGION", "us-east".into()), ("PG_MAX", 100.into())], &[("pg-url", "postgres://db/x")]);
        let mut rng = crate::sim::Rng::new(17);
        for i in 0..30_000 {
            let (k, text) = [("toml", TOML), ("yaml", YAML), ("json", JSON)][i % 3];
            let b = crate::sim::mangle(&mut rng, text.as_bytes());
            let text = String::from_utf8_lossy(&b).into_owned();
            std::panic::catch_unwind(|| {
                if let Ok(tree) = tree_of(k, &text) {
                    if let Ok(m) = from_tree(&tree, &|_| true) {
                        let _ = m.resolve(&e);
                    }
                }
            })
            .expect("the manifest checks panicked");
        }
    }

    #[test]
    fn all_three_formats_resolve_the_same() {
        let e = env(&[("REGION", "us-east".into()), ("PG_MAX", 100.into())], &[("pg-url", "postgres://db/x")]);
        let got: Vec<Resolved> = [("combd.toml", TOML), ("combd.yaml", YAML), ("combd.json", JSON)].iter().map(|f| load(&dir(&[*f]).0).unwrap().unwrap().resolve(&e).unwrap()).collect();
        assert_eq!(got[0], got[1]);
        assert_eq!(got[1], got[2]);
        let r = &got[0];
        let pool = &r.pools["main"];
        assert_eq!(pool.owners, ["api"]);
        let pool = &pool.settings;
        assert_eq!((pool["max"].clone(), pool["on_manager_loss"].clone(), pool["wait_ms"].clone()), (100.into(), "finish".into(), 5000.into()));
        let pg = &r.processes["pg-conn"];
        let (kind, pg) = (&pg.kind, &pg.settings);
        assert_eq!((kind.as_str(), pg["url"].as_str(), pg["connection.timeout_ms"].as_i64(), pg["connection.retries"].as_i64()), ("combd:postgres", Some("postgres://db/x"), Some(3000), Some(3)));
        let api = &r.processes["api"];
        assert_eq!((api.settings["timeout_ms"].as_i64(), api.settings["max_concurrent"].as_i64(), api.settings["durable"].as_bool()), (Some(10000), Some(32), Some(false)));
        assert_eq!(api.routes[0].host.as_deref(), Some("shop.example.com"));
        assert_eq!(api.routes[0].segs, [Seg::Lit("api".into()), Seg::Rest]);
        let environ: BTreeMap<_, _> = r.environ.iter().cloned().collect();
        assert_eq!(environ["PG_MAX"], "100");
        assert_eq!(environ["LOG_LEVEL"], "info");
        assert_eq!(environ["PG_MAIN_URL"], "postgres://db/x");
        assert!(!environ.contains_key("MY_FLAG") && !environ.contains_key("SIGNING_KEY"), "unset optionals aren't set");
    }

    fn errors(manifest: &str) -> String {
        load(&dir(&[("combd.toml", manifest)]).0).unwrap_err()
    }

    #[test]
    fn every_mistake_is_reported() {
        let e = errors(
            r#"
[app]
name = "Shop!"
[vars]
A = { type = "int", default = "x" }
B = {}
"C?" = { type = "int", default = 1 }
D = { type = "string", min = -1, default = "a" }
G = { type = "bool", max = 1, default = true }
E = { type = "enum", default = "a" }
PORT = { type = "int" }
lower = 1
T = { type = "datetime", default = "2026-02-30T00:00:00Z" }
[secrets]
S = { secret = "s", default = "leak" }
PORT = "port"
[processes.pg]
kind = "combd:postgres"
url = "$NOPE"
statement_timeout_ms = "$PORT"
connection.timeout_ms = 0
colour = "red"
[processes.x]
kind = "mysql"
[pools.p]
member = "missing"
on_manager_loss = "panic"
[extra]
"#,
        );
        for want in [
            "app.name",
            "vars.A: default: want int",
            "vars.B: no default, so say its type",
            "vars.C?: it has a default",
            "vars.D: min: want a length",
            "vars.E: an enum needs values",
            "vars.G: min and max go with int, float, string",
            "vars.lower: env var names",
            "vars.T: default: \"2026-02-30T00:00:00Z\"",
            "secrets.S: secrets never have defaults",
            "PORT is in both [vars] and [secrets]",
            "processes.pg.url: $NOPE isn't declared",
            "processes.pg.connection.timeout_ms: 0 is outside 1 to 600000",
            "processes.pg.colour: unknown setting",
            "processes.x.kind: no module modules/mysql.wasm",
            "pools.p.member: no process \"missing\"",
            "pools.p.on_manager_loss: \"panic\": one of finish, stop",
            "extra: unknown section",
        ] {
            assert!(e.contains(want), "missing {want:?} in:\n{e}");
        }
    }

    #[test]
    fn references_must_be_the_same_type_and_set() {
        let base = "[app]\nname = \"a\"\n[processes.api]\nkind = \"api\"\nroutes = [\"/*\"]\n[processes.pg]\nkind = \"combd:postgres\"\nurl = \"postgres://x\"\n";
        // An int var into a url setting: no conversions.
        assert!(errors(&format!("{base}[vars]\nU = 5\n[pools.p]\nmember = \"pg\"\nowners = [\"api\"]\nmax = \"$U\"\nwait_ms = \"$W\"\n")).contains("pools.p.wait_ms: $W isn't declared"));
        assert!(errors(&format!("{base}[vars]\nF = 1.5\n[pools.p]\nmember = \"pg\"\nowners = [\"api\"]\nmax = \"$F\"\n")).contains("pools.p.max: wants int, $F is float"));
        // An optional var can't fill a required setting.
        let m = "[app]\nname = \"a\"\n[secrets]\n\"U?\" = { secret = \"u\", type = \"url\" }\n[processes.pg]\nkind = \"combd:postgres\"\nurl = \"$U\"\n";
        // (It's unreachable too; the check we want is still there.)
        assert!(errors(m).contains("processes.pg.url: needs a value, and $U is optional"));
        // A var's own limits may be wider: the environment's value is checked
        // against the setting's too, before anything starts.
        let m = load(&dir(&[("combd.toml", &format!("{base}[vars]\nM = {{ type = \"int\" }}\n[pools.p]\nmember = \"pg\"\nowners = [\"api\"]\nmax = \"$M\"\n"))]).0).unwrap().unwrap();
        assert!(m.resolve(&env(&[("M", 0.into())], &[])).unwrap_err().contains("pools.p.max: 0 is outside 1 to 10000"));
        assert!(m.resolve(&env(&[], &[])).unwrap_err().contains("M: required"));
        assert!(m.resolve(&env(&[("M", "lots".into())], &[])).unwrap_err().contains("M: want int"));
        assert!(m.resolve(&env(&[("M", 4.into()), ("N", 1.into())], &[])).unwrap_err().contains("N: not declared"));
        // `$$` is a literal dollar.
        let m = load(&dir(&[("combd.toml", "[app]\nname = \"a\"\n[processes.pg]\nkind = \"combd:postgres\"\nurl = \"$$x://y\"\n[processes.api]\nkind = \"api\"\nroutes = [\"/\"]\n[pools.p]\nmember = \"pg\"\nowners = [\"api\"]\n")]).0).unwrap_err();
        assert!(m.contains("\"$x://y\": want a URL"), "{m}");
    }

    #[test]
    fn who_may_reach_whom() {
        let ok = r#"
[app]
name = "shop"
[processes.api]
kind = "api"
routes = ["shop.example.com/api/*", "/health"]
[processes.admin-api]
kind = "admin"
routes = ["admin.example.com/*"]
[processes.image-maker]
kind = "imager"
callers = ["api", "admin-api"]
timeout_ms = 120000
[processes.room]
kind = "api"
durable = true
routes = ["/room/:id/*"]
[processes.pg-conn]
kind = "combd:postgres"
url = "postgres://x"
[pools.pg-pool]
member = "pg-conn"
owners = ["api", "admin-api"]
"#;
        let m = load(&dir(&[("combd.toml", ok)]).0).unwrap().unwrap();
        assert_eq!(m.processes["image-maker"].callers, ["api", "admin-api"]);
        assert!(m.processes["image-maker"].routes.is_empty(), "internal only");
        assert_eq!(m.processes["room"].routes[0].segs, [Seg::Lit("room".into()), Seg::Param("id".into()), Seg::Rest]);
        let e = errors(
            r#"
[app]
name = "shop"
[processes.api]
kind = "api"
routes = ["/a/:x", "/a/:y", "Bad.Host/x", "/x/*/y", "/q/:x/:x", "no-slash", "/sp ace"]
callers = ["nobody"]
[processes.lonely]
kind = "admin"
[processes.ghost]
kind = "nomodule"
routes = ["/g"]
[processes.obj]
kind = "api"
durable = true
max_concurrent = 4
routes = ["/obj/:name"]
[processes.pg-conn]
kind = "combd:postgres"
url = "postgres://x"
routes = ["/pg"]
[processes.pg-unused]
kind = "combd:postgres"
url = "postgres://x"
[pools.a]
member = "pg-conn"
[pools.b]
member = "pg-conn"
owners = ["pg-unused", "missing"]
"#,
        );
        for want in [
            "processes.api.routes: \"/a/:y\" is the same route as one of api's",
            "\"Bad.Host/x\": \"Bad.Host\": want a lowercase host name",
            "\"/x/*/y\": * goes only at the end",
            "\"/q/:x/:x\": :x twice",
            "\"no-slash\": want host/path or /path",
            "\"/sp ace\"",
            "processes.api.callers: no process \"nobody\"",
            "processes.lonely: nothing can reach it",
            "processes.ghost.kind: no module modules/nomodule.wasm",
            "processes.obj.max_concurrent: a durable object takes one event at a time",
            "processes.obj.routes: \"/obj/:name\": a durable object's route names it with :id",
            "processes.pg-conn: combd:postgres is reached through a pool, so it takes no routes or callers",
            "processes.pg-unused: nothing can reach it (no pool hands it out)",
            "pools.a.owners: required",
            "pools.b.owners: pg-unused is a built-in kind",
            "pools.b.owners: no process \"missing\"",
        ] {
            assert!(e.contains(want), "missing {want:?} in:\n{e}");
        }
    }

    #[test]
    fn a_bad_secret_never_shows_its_value() {
        let m = load(&dir(&[("combd.toml", TOML)]).0).unwrap().unwrap();
        let e = m.resolve(&env(&[("REGION", "us-east".into())], &[("pg-url", "hunter2 not a url")])).unwrap_err();
        assert!(e.contains("PG_MAIN_URL (secret \"pg-url\"): its value isn't a valid url") && !e.contains("hunter2"), "{e}");
        assert!(m.resolve(&env(&[("REGION", "US".into())], &[("pg-url", "postgres://x")])).unwrap_err().contains("doesn't match"));
    }

    #[test]
    fn files_one_of_each_and_the_dev_env() {
        assert!(load(&dir(&[]).0).unwrap().is_none());
        assert!(load(&dir(&[("combd.toml", "[app]\nname=\"a\""), ("combd.json", "{}")]).0).unwrap_err().contains("more than one"));
        let d = dir(&[("combd.dev.yaml", "vars: { REGION: us-east }\nsecrets: { pg-url: \"postgres://x\" }\n")]);
        let e = load_dev(&d.0).unwrap();
        assert_eq!((e.vars["REGION"].as_str(), e.secrets["pg-url"].as_str()), (Some("us-east"), "postgres://x"));
        assert!(load_dev(&dir(&[("combd.dev.toml", "[other]\n")]).0).err().unwrap().contains("only [vars] and [secrets]"));
    }

    #[test]
    fn datetimes_are_rfc3339_with_an_offset() {
        for ok in ["2026-10-15T08:00:00Z", "2024-02-29T23:59:60.123+05:30", "2026-01-01t00:00:00-00:00"] {
            assert!(instant(ok).is_some(), "{ok}");
        }
        for bad in ["2026-10-15T08:00:00", "2026-10-15", "2025-02-29T00:00:00Z", "2026-13-01T00:00:00Z", "2026-W42-1T00:00:00Z", "2026-10-15T08:00:00.Z", "2026-10-15T08:00:00+24:00"] {
            assert!(instant(bad).is_none(), "{bad}");
        }
        assert_eq!(instant("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(instant("2000-03-01T00:00:00.5Z"), Some(951_868_800_500_000_000));
        // The same instant, two offsets.
        assert_eq!(instant("2026-10-15T10:00:00+02:00"), instant("2026-10-15T08:00:00Z"));
    }

    #[test]
    fn datetime_and_string_limits() {
        let at = |v: &str| -> Spec { spec(Side::Var, "X", &serde_json::from_str(v).unwrap()).unwrap().1 };
        let d = at(r#"{"type":"datetime","min":"2026-01-01T00:00:00Z","max":"2026-12-31T23:59:59Z"}"#);
        assert!(check_value(&d, &"2026-06-01T12:00:00+02:00".into()).is_ok());
        assert!(check_value(&d, &"2026-01-01T00:30:00+01:00".into()).unwrap_err().contains("outside"), "that's 2025-12-31T23:30Z");
        assert!(check_value(&d, &"2027-01-01T00:00:00Z".into()).is_err());
        let s = at(r#"{"type":"string","min":2,"max":3}"#);
        assert!(check_value(&s, &"éé".into()).is_ok(), "characters, not bytes");
        assert!(check_value(&s, &"abcd".into()).unwrap_err().contains("4 characters, want 2 to 3"));
        assert!(spec(Side::Var, "X", &serde_json::from_str(r#"{"type":"datetime","min":"yesterday"}"#).unwrap()).unwrap_err().contains("min: want a datetime"));
        // A hard cap, whatever the spec says.
        let big = at(r#"{"type":"string","max":100000000}"#);
        assert!(check_value(&big, &"x".repeat(MAX_VALUE + 1).into()).unwrap_err().contains("at most"));
        assert!(check_value(&big, &"x".repeat(MAX_VALUE).into()).is_ok());
        let long = Json::String("a".repeat(MAX_PATTERN + 1));
        assert!(spec(Side::Var, "X", &serde_json::json!({ "type": "string", "pattern": long })).unwrap_err().contains("longer than"));
        // No look-around or backreferences, so no pattern can run for long:
        // matching is linear in the text.
        for p in ["^(?=a)a$", "(?<=a)b", r"(a)\1", "(a+)+$"] {
            let r = spec(Side::Var, "X", &serde_json::json!({ "type": "string", "pattern": p }));
            if p == "(a+)+$" {
                // The classic catastrophic pattern compiles, and still runs in no time.
                let t = std::time::Instant::now();
                assert!(check_value(&r.unwrap().1, &format!("{}!", "a".repeat(MAX_VALUE - 1)).into()).is_err());
                assert!(t.elapsed() < std::time::Duration::from_millis(100));
            } else {
                assert!(r.unwrap_err().contains("pattern:"), "{p} must be refused");
            }
        }
    }
}
