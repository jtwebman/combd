//! Compiled modules' builds, through the bucket. Nodes don't compile:
//! builders do (`combd builder`), and a node loads what they made.
//!
//! ```text
//! _combd/nodes/<n>                       what node n runs: its pin and its CPU
//! _combd/builds/<pin>/<cpu>/<m>.wasm     a module wanted, built for that pin and CPU (the job)
//! _combd/builds/<pin>/<cpu>/<m>.claim    a builder is on it (its id and when)
//! _combd/builds/<pin>/<cpu>/<m>.so       the build
//! _combd/builds/<pin>/<cpu>/<m>.sig      its builder's signature
//! _combd/builds/<pin>/<cpu>/<m>.err      why it couldn't be built (the deploy fails)
//! _combd/builders/<id>                   a builder: its pin, CPUs, and when it last looked
//! ```
//!
//! Builders don't touch the bucket. Two ways to get them work:
//! - Pull (`combd builder`): a builder asks a node's admin API for work and
//!   sends back what it built (`POST /v1/builds/next`, `/v1/builds/done`),
//!   each request signed with its key, which the nodes must list.
//! - Push (`combd builder serve`): the node that claims a job sends it to
//!   the builder (`COMBD_BUILDER_PUSH`, `POST /build`) and writes what comes
//!   back. One build per request, so a host like Cloud Run (one request an
//!   instance) builds many at once and scales to zero.
//!   Each push is signed with the nodes' push key (`COMBD_PUSH_KEY`), which
//!   the builder must list (`COMBD_PUSH_KEYS`): no shared secret, and the
//!   key that signs builds stays on the builder.
//! A build is loaded only with a listed builder's signature either way. A builder builds for its own pin only: move it before the
//! nodes. A push of another pin's job is turned away (409) and sent again
//! later, not failed.
//!
//! `<pin>` is a short hash of what the build has to match (rustc, combd-aot,
//! the target: see combd-aot's `PIN`), `<m>` the module's hash. A build is
//! native code a node will run, so it's only loaded with a signature from a
//! builder the cluster lists (`COMBD_BUILDER_KEYS`), over the build, its
//! module, its pin, and its CPU: one can't be passed off as another.

use crate::s3::{Cond, S3};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A builder that's gone quiet this long loses its claim to another.
const CLAIM_STALE_MS: u64 = 15 * 60 * 1000;
/// How long an idle builder waits before it looks for jobs again: at first,
/// and at most. A job may wait up to `IDLE_MAX` for an idle builder.
const IDLE_FIRST: Duration = Duration::from_secs(2);
const IDLE_MAX: Duration = Duration::from_secs(30);

pub fn pin_id(pin: &str) -> String {
    format!("{:016x}", combd_aot::hash(pin.as_bytes()))
}

/// A module's name for its builds (in the bucket, on disk, what's signed and
/// loaded): 128 bits of its SHA-256, so no module can pass for another and
/// get its build. (`combd_aot::hash`, 64-bit FNV, only checks a build is of
/// the module it was asked for, after the signature.)
pub fn module_id(wasm: &[u8]) -> String {
    hex(&ring::digest::digest(&ring::digest::SHA256, wasm).as_ref()[..16])
}

/// This machine's CPU, as a build is made for: x86-64 at a feature level,
/// or ARM64. A build for a level runs on that level and above.
pub fn cpu() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        let v4 = is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512vl");
        let v3 = is_x86_feature_detected!("avx2") && is_x86_feature_detected!("bmi2") && is_x86_feature_detected!("fma");
        let v2 = is_x86_feature_detected!("sse4.2") && is_x86_feature_detected!("popcnt");
        if v4 {
            "x86-64-v4"
        } else if v3 {
            "x86-64-v3"
        } else if v2 {
            "x86-64-v2"
        } else {
            "x86-64"
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "generic"
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn key(pin: &str, cpu: &str, m: &str, ext: &str) -> String {
    format!("_combd/builds/{pin}/{cpu}/{m}.{ext}")
}

/// What a signature covers.
fn signed(pin: &str, cpu: &str, m: &str, so: &[u8]) -> Vec<u8> {
    let mut msg = format!("combd build\0{pin}\0{cpu}\0{m}\0").into_bytes();
    msg.extend_from_slice(&Sha256::digest(so));
    msg
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect()
}

/// A new builder key into `file` (PKCS#8): its public half, for the
/// cluster's `COMBD_BUILDER_KEYS`.
pub fn new_key(file: &Path) -> Result<String, String> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "no key")?;
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| "bad key")?;
    std::fs::write(file, pkcs8.as_ref()).map_err(|e| format!("{}: {e}", file.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    Ok(hex(kp.public_key().as_ref()))
}

/// Public keys, one hex key a line (`#` comments).
pub fn parse_keys(text: &str) -> Vec<Vec<u8>> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(|l| unhex(l.split_whitespace().next()?)).filter(|k| k.len() == 32).collect()
}

/// A node's side: where its builds come from, and what it needs.
pub struct Builds {
    pub s3: S3,
    /// Builds fetched (and checked) so far, as `<pin>/<m>.so`: one for
    /// another pin (a node from before an update) is never taken for this one.
    pub dir: PathBuf,
    /// The builders it trusts.
    pub keys: Vec<Vec<u8>>,
    pub pin: String,
    pub cpu: String,
}

pub enum Fetch {
    /// The build, checked, on disk (`aot::built` loads it).
    Ready,
    /// Not built yet: the job is in.
    Building,
}

impl Builds {
    /// For this binary on this machine; and says so in the bucket
    /// (`_combd/nodes/<node>`), for deploys to know what to build.
    pub fn new(s3: S3, dir: PathBuf, keys: Vec<Vec<u8>>, node: u32) -> Result<Builds, String> {
        let pin = pin_id(combd_aot::PIN);
        let dir = dir.join(&pin);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let b = Builds { s3, dir, keys, pin, cpu: cpu().to_string() };
        let me = serde_json::json!({ "pin": b.pin, "pin_text": combd_aot::PIN, "cpu": b.cpu, "version": option_env!("COMBD_VERSION").unwrap_or("dev"), "at": now_ms() });
        b.s3.put(&format!("_combd/nodes/{node}"), me.to_string().as_bytes(), &Cond::Any).map_err(|e| e.to_string())?.map_err(|()| "can't write the node's record")?;
        b.s3.put(&format!("_combd/builds/{}/pin", b.pin), combd_aot::PIN.as_bytes(), &Cond::Any).map_err(|e| e.to_string())?.map_err(|()| "can't write the pin")?;
        Ok(b)
    }

    /// Is `sig` (hex) a trusted builder's signature over this build?
    fn trusted(&self, m: &str, so: &[u8], sig: &[u8]) -> bool {
        let msg = signed(&self.pin, &self.cpu, m, so);
        unhex(String::from_utf8_lossy(sig).trim()).is_some_and(|sig| self.keys.iter().any(|k| UnparsedPublicKey::new(&ED25519, k).verify(&msg, &sig).is_ok()))
    }

    /// The build of `m` already here, if its signature (kept beside it)
    /// still checks out: one put there by anything but a fetch, or changed
    /// since, isn't loaded (and goes, to be fetched again).
    fn local(&self, m: &str) -> Option<PathBuf> {
        let (so, sig) = (self.dir.join(format!("{m}.so")), self.dir.join(format!("{m}.so.sig")));
        let ok = matches!((std::fs::read(&so), std::fs::read(&sig)), (Ok(b), Ok(s)) if self.trusted(m, &b, &s));
        if !ok {
            let _ = std::fs::remove_file(&so);
            let _ = std::fs::remove_file(&sig);
        }
        ok.then_some(so)
    }

    /// This node's build of `wasm`: here already (its signature checked
    /// again), or from the bucket (checked first), or asked for.
    pub fn fetch(&self, wasm: &[u8]) -> Result<Fetch, String> {
        let m = module_id(wasm);
        if self.local(&m).is_some() {
            return Ok(Fetch::Ready);
        }
        let local = self.dir.join(format!("{m}.so"));
        let get = |ext: &str| self.s3.get(&key(&self.pin, &self.cpu, &m, ext)).map_err(|e| e.to_string());
        if let Some((err, _)) = get("err")? {
            return Err(format!("module {m} couldn't be built: {}", String::from_utf8_lossy(&err)));
        }
        match (get("so")?, get("sig")?) {
            (Some((so, _)), Some((sig, _))) => {
                if !self.trusted(&m, &so, &sig) {
                    return Err(format!("module {m}: its build isn't signed by a builder this node trusts (COMBD_BUILDER_KEYS)"));
                }
                // The signature first: a build is only ever here with it.
                let tmp = self.dir.join(format!("{m}.so.part"));
                std::fs::write(self.dir.join(format!("{m}.so.sig")), &sig)
                    .and_then(|()| std::fs::write(&tmp, &so))
                    .and_then(|()| std::fs::rename(&tmp, &local))
                    .map_err(|e| format!("{}: {e}", local.display()))?;
                Ok(Fetch::Ready)
            }
            _ => {
                want(&self.s3, &self.pin, &self.cpu, wasm)?;
                Ok(Fetch::Building)
            }
        }
    }
}

/// Ask for a build of `wasm` for `pin` and `cpu` (once: the job is the module),
/// and push it to the builder if there's one to push to.
pub fn want(s3: &S3, pin: &str, cpu: &str, wasm: &[u8]) -> Result<(), String> {
    s3.put(&key(pin, cpu, &module_id(wasm), "wasm"), wasm, &Cond::IfAbsent).map_err(|e| e.to_string())?.ok();
    if let Ok(url) = std::env::var("COMBD_BUILDER_PUSH") {
        push(s3, &url, &std::env::var("COMBD_PUSH_KEY").unwrap_or_default(), pin, cpu, wasm);
    }
    Ok(())
}

/// Pushes from this process: (the job's claim, when it was sent, its thread).
type Pushes = Vec<(String, std::time::Instant, std::thread::JoinHandle<()>)>;
static PUSHING: std::sync::Mutex<Pushes> = std::sync::Mutex::new(Vec::new());
/// At most this many builds at once from one process.
const PUSHES: usize = 4;
/// A job isn't pushed again sooner than this after it was (it's asked for
/// every few seconds while a deploy waits).
const PUSH_AGAIN: Duration = Duration::from_secs(30);
/// How long a pushed build may take, a cold start included: under
/// `CLAIM_STALE_MS`, so no one takes the job from a push still waiting.
const PUSH_WAIT: Duration = Duration::from_secs(14 * 60);

/// Push a job to the builder at `url`, off the caller's thread, if this
/// process can claim it: the claim makes the push the job's only one, and
/// if this node dies its claim goes stale and another sends it. On a
/// timeout, a refusal, or a builder of another pin, the claim is let go and
/// the next ask sends it again; a module that can't be built is the job's
/// answer (`.err`), as from a pull.
fn push(s3: &S3, url: &str, key_file: &str, pin: &str, cpu: &str, wasm: &[u8]) {
    let m = module_id(wasm);
    let k = key(pin, cpu, &m, "claim");
    let mut p = PUSHING.lock().unwrap();
    p.retain(|(_, at, h)| !h.is_finished() || at.elapsed() < PUSH_AGAIN);
    if p.iter().filter(|(_, _, h)| !h.is_finished()).count() >= PUSHES || p.iter().any(|(j, ..)| *j == k) {
        return;
    }
    let (s3, url, key_file, pin, cpu, wasm, k2) = (s3.clone(), url.to_string(), key_file.to_string(), pin.to_string(), cpu.to_string(), wasm.to_vec(), k.clone());
    let h = std::thread::spawn(move || {
        let k = k2;
        let signer = match load_key(Path::new(&key_file)) {
            Ok(key) => key,
            Err(e) => return eprintln!("builds    can't push to {url}: COMBD_PUSH_KEY: {e}"),
        };
        if !claim(&s3, &k, "push") {
            return;
        }
        let body = serde_json::json!({ "pin": pin, "cpu": cpu, "module": m, "wasm": hex(&wasm) }).to_string().into_bytes();
        let tls = crate::s3::tls_config();
        let host = url.split_once("://").map_or(url.as_str(), |(_, r)| r).split([':', '/']).next().unwrap_or("").to_string();
        let tls = url.starts_with("https://").then_some((&tls, host.as_str()));
        let saved = match crate::admin::request_for(PUSH_WAIT, &url, tls, &sign_request(&signer, "POST", "/build", &body), "POST", "/build", &body) {
            Ok((200, v)) => match (v["error"].as_str(), unhex(v["so"].as_str().unwrap_or(""))) {
                (Some(e), _) => done(&s3, &pin, &cpu, &m, Err(e)),
                (None, Some(so)) if !so.is_empty() => done(&s3, &pin, &cpu, &m, Ok((&so, v["sig"].as_str().unwrap_or("")))),
                _ => Err("its answer has no build".into()),
            },
            Ok((status, v)) => Err(format!("{status}: {}", v["error"].as_str().unwrap_or("?"))),
            Err(e) => Err(e),
        };
        if let Err(e) = saved {
            eprintln!("builds    {m} for {cpu}: the builder at {url}: {e} (sent again on the next ask)");
            let _ = s3.delete(&k);
        }
    });
    p.push((k, std::time::Instant::now(), h));
}

/// Wait for this process's pushes (before it exits: `builds-ready`).
pub fn pushed() {
    let hs: Vec<_> = PUSHING.lock().unwrap().drain(..).map(|(.., h)| h).collect();
    for h in hs {
        let _ = h.join();
    }
}

/// Build one job: the answer a builder sends back, pushed or pulled
/// (`pin`, `cpu`, `module`, and the build and its signature, or why not).
fn build_job(key: &Ed25519KeyPair, aot: &Path, work: &Path, pin: &str, cpu: &str, m: &str, wasm: &str) -> serde_json::Value {
    let r = unhex(wasm).ok_or_else(|| "the job's Wasm isn't hex".to_string()).and_then(|wasm| {
        let out = work.join(format!("{m}-{cpu}.so"));
        let level = (cpu != "generic").then_some(cpu);
        crate::build::build(&wasm, aot, work, &out, None, level)?;
        std::fs::read(&out).map_err(|e| e.to_string())
    });
    match &r {
        Ok(so) => serde_json::json!({ "pin": pin, "cpu": cpu, "module": m, "so": hex(so), "sig": hex(key.sign(&signed(pin, cpu, m, so)).as_ref()) }),
        Err(e) => serde_json::json!({ "pin": pin, "cpu": cpu, "module": m, "error": e }),
    }
}

/// A push builder (`combd builder serve`): `POST /build`, signed by a push
/// key in `keys`, `{pin, cpu, module, wasm}`, built while the request waits, one at
/// a time; a job for another pin is turned away (409), for the node to
/// send again (once this builder's moved, or to another).
pub fn serve(l: std::net::TcpListener, keys: &[Vec<u8>], key: &Ed25519KeyPair, aot: &Path, work: &Path) {
    let pin = pin_id(combd_aot::PIN);
    eprintln!("builder   building for {:?} ({pin}), for what's pushed to {}", combd_aot::PIN, l.local_addr().map(|a| a.to_string()).unwrap_or_default());
    for s in l.incoming().flatten() {
        let mut s = s;
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        // The key and the time on the headers, before the body is read; the
        // signature over the body after.
        let check = |method: &str, path: &str, auth: Option<&str>| match (method, path) {
            ("POST", "/build") if check_request(keys, auth.unwrap_or(""), method, path, None) => Ok(()),
            ("POST", "/build") => Err((401, serde_json::json!({ "error": "want Authorization: Builder <key> <ms> <signature>, from a push key this builder lists" }))),
            _ => Err((404, serde_json::json!({ "error": "POST /build" }))),
        };
        let (status, answer) = match crate::admin::read_request(&mut s, check) {
            Ok(Ok((_, _, auth, body))) if !check_request(keys, auth.as_deref().unwrap_or(""), "POST", "/build", Some(&body)) => (401, serde_json::json!({ "error": "not signed by a push key this builder lists" })),
            Ok(Ok((.., body))) => match serde_json::from_slice::<serde_json::Value>(&body) {
                Ok(j) if j["pin"].as_str() != Some(pin.as_str()) => (409, serde_json::json!({ "error": format!("this builder builds for {pin}") })),
                Ok(j) => {
                    let (cpu, m) = (j["cpu"].as_str().unwrap_or(""), j["module"].as_str().unwrap_or(""));
                    match (part(cpu), part(m)) {
                        (Ok(cpu), Ok(m)) => {
                            let a = build_job(key, aot, work, &pin, cpu, m, j["wasm"].as_str().unwrap_or(""));
                            match a["error"].as_str() {
                                Some(e) => eprintln!("builder   {m} for {cpu} failed: {e}"),
                                None => eprintln!("builder   built {m} for {cpu} ({} KB)", a["so"].as_str().map_or(0, |h| h.len() / 2048)),
                            }
                            (200, a)
                        }
                        (Err(e), _) | (_, Err(e)) => (400, serde_json::json!({ "error": e })),
                    }
                }
                Err(_) => (400, serde_json::json!({ "error": "want {\"pin\", \"cpu\", \"module\", \"wasm\"}" })),
            },
            Ok(Err((status, e))) => (status, e),
            Err(e) => (400, serde_json::json!({ "error": e })),
        };
        let body = answer.to_string();
        let _ = std::io::Write::write_all(&mut s, format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes());
    }
}

/// Every (pin, CPU) the fleet's nodes run, from their records.
pub fn targets(s3: &S3) -> Result<Vec<(String, String)>, String> {
    let mut out = vec![];
    for k in s3.list("_combd/nodes/").map_err(|e| e.to_string())? {
        if let Some((b, _)) = s3.get(&k).map_err(|e| e.to_string())? {
            let v: serde_json::Value = serde_json::from_slice(&b).unwrap_or_default();
            if let (Some(p), Some(c)) = (v["pin"].as_str(), v["cpu"].as_str()) {
                out.push((p.to_string(), c.to_string()));
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Where each of these modules' builds stands, for every target the fleet
/// has: `Ok(true)` when all are built, `Ok(false)` while some aren't (they're
/// asked for), `Err` if one couldn't be.
pub fn all_built(s3: &S3, wasms: &[Vec<u8>]) -> Result<bool, String> {
    let mut done = true;
    for (pin, cpu) in targets(s3)? {
        done &= built_for(s3, &pin, &cpu, wasms)?;
    }
    Ok(done)
}

/// The same for one target: a node's (`pin`, `cpu`), or a new combd's
/// before a node moves to it.
pub fn built_for(s3: &S3, pin: &str, cpu: &str, wasms: &[Vec<u8>]) -> Result<bool, String> {
    let mut done = true;
    for w in wasms {
        let m = module_id(w);
        if let Some((e, _)) = s3.get(&key(pin, cpu, &m, "err")).map_err(|e| e.to_string())? {
            return Err(format!("module {m} couldn't be built for {cpu}: {}", String::from_utf8_lossy(&e)));
        }
        if s3.get(&key(pin, cpu, &m, "sig")).map_err(|e| e.to_string())?.is_none() {
            want(s3, pin, cpu, w)?;
            done = false;
        }
    }
    Ok(done)
}

/// The builds a deploy waits for, for `/v1/deploys`: (module, pin, cpu, state).
pub fn status(s3: &S3, wasms: &[Vec<u8>]) -> Vec<serde_json::Value> {
    let Ok(targets) = targets(s3) else { return vec![] };
    let mut out = vec![];
    for (pin, cpu) in targets {
        for w in wasms {
            let m = module_id(w);
            let has = |ext: &str| s3.get(&key(&pin, &cpu, &m, ext)).ok().flatten();
            let state = if let Some((e, _)) = has("err") {
                format!("failed: {}", String::from_utf8_lossy(&e))
            } else if has("sig").is_some() {
                "built".into()
            } else if let Some((c, _)) = has("claim") {
                format!("building ({})", String::from_utf8_lossy(&c).split(' ').next().unwrap_or("?"))
            } else {
                "waiting for a builder".into()
            };
            out.push(serde_json::json!({ "module": m, "pin": pin, "cpu": cpu, "state": state }));
        }
    }
    out
}

/// The builders that have looked for work: their id, pin, and how long ago.
pub fn builders(s3: &S3) -> Vec<serde_json::Value> {
    let now = now_ms();
    s3.list("_combd/builders/")
        .unwrap_or_default()
        .iter()
        .filter_map(|k| {
            let (b, _) = s3.get(k).ok()??;
            let v: serde_json::Value = serde_json::from_slice(&b).ok()?;
            Some(serde_json::json!({ "id": k.trim_start_matches("_combd/builders/"), "pin": v["pin"], "seen_s": now.saturating_sub(v["at"].as_u64().unwrap_or(0)) / 1000 }))
        })
        .collect()
}

/// How far a builder's clock may be from a node's: a request signed longer
/// ago (or ahead) than this is refused.
const REQUEST_SKEW_MS: u64 = 5 * 60 * 1000;

/// What a builder signs for a request: all of it.
fn request_msg(method: &str, path: &str, at: u64, body: &[u8]) -> Vec<u8> {
    let mut msg = format!("combd builder request\0{method}\0{path}\0{at}\0").into_bytes();
    msg.extend_from_slice(&Sha256::digest(body));
    msg
}

/// A builder's request, signed with its key: the `authorization` header.
pub fn sign_request(key: &Ed25519KeyPair, method: &str, path: &str, body: &[u8]) -> String {
    let at = now_ms();
    format!("Builder {} {at} {}", hex(key.public_key().as_ref()), hex(key.sign(&request_msg(method, path, at, body)).as_ref()))
}

/// Is `auth` from a builder in `keys`, signed lately? Without the body,
/// only that much (so a stranger's body is never read); with it, the
/// signature over the whole request too.
pub fn check_request(keys: &[Vec<u8>], auth: &str, method: &str, path: &str, body: Option<&[u8]>) -> bool {
    let mut w = auth.strip_prefix("Builder ").unwrap_or("").split(' ');
    let (Some(k), Some(at), Some(sig)) = (w.next().and_then(unhex), w.next().and_then(|t| t.parse::<u64>().ok()), w.next().and_then(unhex)) else { return false };
    keys.contains(&k) && now_ms().abs_diff(at) <= REQUEST_SKEW_MS && body.is_none_or(|b| UnparsedPublicKey::new(&ED25519, &k).verify(&request_msg(method, path, at, b), &sig).is_ok())
}

/// A pin, CPU, or module as a builder names it: one part of a bucket key.
fn part(s: &str) -> Result<&str, String> {
    let ok = !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
    if ok { Ok(s) } else { Err(format!("{s:?}: not a pin, CPU, or module")) }
}

/// A node's side of `POST /v1/builds/next`: a job for the builder's pin that
/// nobody has built or claimed, claimed for it now, as (CPU, module, its
/// Wasm); or none. Notes the builder was here (`_combd/builders/<id>`), once
/// a minute at most from each node: a PUT is a billed write.
pub fn next(s3: &S3, id: &str, pin: &str) -> Result<Option<(String, String, Vec<u8>)>, String> {
    static SAID: std::sync::Mutex<Vec<(String, u64)>> = std::sync::Mutex::new(Vec::new());
    let (id, pin, now) = (part(id)?, part(pin)?, now_ms());
    let say = {
        let mut said = SAID.lock().unwrap();
        said.retain(|(_, t)| now.saturating_sub(*t) < 60_000);
        let due = !said.iter().any(|(i, _)| i == id);
        if due {
            said.push((id.to_string(), now));
        }
        due
    };
    if say {
        let _ = s3.put(&format!("_combd/builders/{id}"), serde_json::json!({ "pin": pin, "at": now }).to_string().as_bytes(), &Cond::Any);
    }
    let prefix = format!("_combd/builds/{pin}/");
    let jobs = s3.list(&prefix).map_err(|e| e.to_string())?;
    let has = |k: &str| jobs.iter().any(|j| j == k);
    for job in jobs.iter().filter(|j| j.ends_with(".wasm")) {
        let base = job.trim_end_matches(".wasm");
        if has(&format!("{base}.sig")) || has(&format!("{base}.err")) {
            continue;
        }
        let Some((cpu, m)) = base.strip_prefix(&prefix).and_then(|r| r.split_once('/')) else { continue };
        if !claim(s3, &format!("{base}.claim"), id) {
            continue;
        }
        match s3.get(job).map_err(|e| e.to_string())? {
            Some((wasm, _)) => return Ok(Some((cpu.to_string(), m.to_string(), wasm))),
            None => drop(s3.delete(&format!("{base}.claim"))),
        }
    }
    Ok(None)
}

/// A node's side of `POST /v1/builds/done`: the build and its signature
/// (checked when a node loads it, not here), or why it couldn't be built.
/// The claim goes.
pub fn done(s3: &S3, pin: &str, cpu: &str, m: &str, r: Result<(&[u8], &str), &str>) -> Result<(), String> {
    let (pin, cpu, m) = (part(pin)?, part(cpu)?, part(m)?);
    let put = |ext: &str, b: &[u8]| s3.put(&key(pin, cpu, m, ext), b, &Cond::Any).map_err(|e| e.to_string())?.map_err(|()| format!("can't write the {ext}"));
    match r {
        // The build first, its signature last: a node sees a build as
        // there only once it's signed.
        Ok((so, sig)) => put("so", so).and_then(|_| put("sig", sig.as_bytes()))?,
        Err(e) => put("err", e.as_bytes())?,
    };
    let _ = s3.delete(&key(pin, cpu, m, "claim"));
    Ok(())
}

/// A builder: asks the fleet (`fleet`, any node's admin API, or a name for
/// all of them) for jobs for its own pin (it can make no other), builds
/// them one at a time, and sends each back, until `stop`. It signs every
/// request with its key; it never sees the bucket. `aot` is the combd-aot
/// sources it builds against, `work` its cargo directory (kept: the runtime
/// compiles once).
pub fn builder(fleet: &str, id: &str, key: &Ed25519KeyPair, aot: &Path, work: &Path, stop: &std::sync::atomic::AtomicBool) {
    let pin = pin_id(combd_aot::PIN);
    eprintln!("builder   {id}: building for {:?} ({pin}), from {fleet}", combd_aot::PIN);
    let host = fleet.split_once("://").map_or(fleet, |(_, r)| r).split([':', '/']).next().unwrap_or("").to_string();
    let tls = crate::s3::tls_config();
    let call = |path: &str, body: serde_json::Value| -> Result<serde_json::Value, String> {
        let body = body.to_string().into_bytes();
        let auth = sign_request(key, "POST", path, &body);
        let tls = fleet.starts_with("https://").then_some((&tls, host.as_str()));
        match crate::admin::request_as(fleet, tls, &auth, "POST", path, &body)? {
            (200, v) => Ok(v),
            (status, v) => Err(format!("{status}: {}", v["error"].as_str().unwrap_or("?"))),
        }
    };
    // An idle builder asks less often (2 s, doubling to 30 s): each ask is
    // a LIST, a billed write.
    let (mut wait, mut failing) = (IDLE_FIRST, false);
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let job = call("/v1/builds/next", serde_json::json!({ "id": id, "pin": pin }));
        // Said once when it stops working, and once when it works again.
        if job.is_err() != failing {
            failing = job.is_err();
            match &job {
                Err(e) => eprintln!("builder   {id}: can't ask {fleet} for work: {e}"),
                Ok(_) => eprintln!("builder   {id}: asking {fleet} for work again"),
            }
        }
        let job = job.ok().filter(|v| v["job"].is_object());
        if let Some(j) = &job {
            let (cpu, m) = (j["job"]["cpu"].as_str().unwrap_or(""), j["job"]["module"].as_str().unwrap_or(""));
            let answer = build_job(key, aot, work, &pin, cpu, m, j["job"]["wasm"].as_str().unwrap_or(""));
            let (kb, failed) = (answer["so"].as_str().map_or(0, |h| h.len() / 2048), answer["error"].as_str().map(String::from));
            match (call("/v1/builds/done", answer), failed) {
                (Ok(_), None) => eprintln!("builder   {id}: built {m} for {cpu} ({kb} KB)"),
                (Ok(_), Some(e)) => eprintln!("builder   {id}: {m} for {cpu} failed: {e}"),
                // Its claim goes stale, and it (or another) builds it again.
                (Err(e), _) => eprintln!("builder   {id}: built {m} for {cpu}, but couldn't send it: {e}"),
            }
            wait = IDLE_FIRST;
        } else {
            // In steps, so `stop` is still seen within 2 s.
            let until = std::time::Instant::now() + wait;
            while std::time::Instant::now() < until && !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(IDLE_FIRST.min(until - std::time::Instant::now()));
            }
            wait = (wait * 2).min(IDLE_MAX);
        }
    }
}

/// Take a job: no claim yet, or one gone stale (its builder went quiet).
fn claim(s3: &S3, k: &str, id: &str) -> bool {
    let mine = format!("{id} {}", now_ms());
    match s3.get(k) {
        Ok(None) => matches!(s3.put(k, mine.as_bytes(), &Cond::IfAbsent), Ok(Ok(_))),
        Ok(Some((c, etag))) => {
            let at: u64 = String::from_utf8_lossy(&c).split(' ').nth(1).and_then(|t| t.parse().ok()).unwrap_or(0);
            now_ms().saturating_sub(at) > CLAIM_STALE_MS && matches!(s3.put(k, mine.as_bytes(), &Cond::IfMatch(etag)), Ok(Ok(_)))
        }
        Err(_) => false,
    }
}

pub fn load_key(file: &Path) -> Result<Ed25519KeyPair, String> {
    let b = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ed25519KeyPair::from_pkcs8(&b).map_err(|_| format!("{}: not a builder key (combd builder-key)", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signature covers the build, its module, pin, and CPU: change any
    /// and it no longer verifies.
    #[test]
    fn a_signature_binds_the_build_to_its_module_pin_and_cpu() {
        let dir = crate::store::tempdir::Dir::new();
        let public = unhex(&new_key(&dir.0.join("k")).unwrap()).unwrap();
        let k = load_key(&dir.0.join("k")).unwrap();
        let sig = k.sign(&signed("p", "x86-64-v3", "m", b"so"));
        let ok = |pin, cpu, m, so: &[u8]| UnparsedPublicKey::new(&ED25519, &public).verify(&signed(pin, cpu, m, so), sig.as_ref()).is_ok();
        assert!(ok("p", "x86-64-v3", "m", b"so"));
        assert!(!ok("q", "x86-64-v3", "m", b"so"), "another pin");
        assert!(!ok("p", "x86-64-v2", "m", b"so"), "another cpu");
        assert!(!ok("p", "x86-64-v3", "n", b"so"), "another module");
        assert!(!ok("p", "x86-64-v3", "m", b"os"), "another build");
        assert_eq!(parse_keys(&format!("# builders\n{}\nnot hex\n", hex(&public))), [public]);
    }

    /// A builder's request is taken only from a key the node lists, signed
    /// lately, over exactly what was sent.
    #[test]
    fn a_request_is_a_listed_builders_signed_lately_over_all_of_it() {
        let dir = crate::store::tempdir::Dir::new();
        let keys = vec![unhex(&new_key(&dir.0.join("k")).unwrap()).unwrap()];
        let (k, stranger) = (load_key(&dir.0.join("k")).unwrap(), load_key(&{
            new_key(&dir.0.join("s")).unwrap();
            dir.0.join("s")
        })
        .unwrap());
        let auth = sign_request(&k, "POST", "/v1/builds/next", b"body");
        assert!(check_request(&keys, &auth, "POST", "/v1/builds/next", None));
        assert!(check_request(&keys, &auth, "POST", "/v1/builds/next", Some(b"body")));
        assert!(!check_request(&keys, &auth, "POST", "/v1/builds/next", Some(b"bodY")), "another body");
        assert!(!check_request(&keys, &auth, "POST", "/v1/builds/done", Some(b"body")), "another path");
        assert!(!check_request(&keys, &sign_request(&stranger, "POST", "/v1/builds/next", b"body"), "POST", "/v1/builds/next", None), "a key it doesn't list");
        let at = now_ms() - REQUEST_SKEW_MS - 1000;
        let old = format!("Builder {} {at} {}", hex(k.public_key().as_ref()), hex(k.sign(&request_msg("POST", "/v1/builds/next", at, b"body")).as_ref()));
        assert!(!check_request(&keys, &old, "POST", "/v1/builds/next", Some(b"body")), "signed too long ago");
        assert!(!check_request(&keys, "Bearer combd_x", "POST", "/v1/builds/next", None));
        assert!(part("../x").is_err() && part("").is_err() && part("x86-64-v3").is_ok());
    }

    /// A push builder builds only what a node it lists signed, over this
    /// very body, for its own pin: anything else is turned away before a
    /// build starts.
    #[test]
    fn a_push_builder_takes_only_its_nodes_signed_jobs_for_its_pin() {
        let dir = crate::store::tempdir::Dir::new();
        let nodes = vec![unhex(&new_key(&dir.0.join("node")).unwrap()).unwrap()];
        new_key(&dir.0.join("builder")).unwrap();
        new_key(&dir.0.join("stranger")).unwrap();
        let (node, stranger, builder) = (load_key(&dir.0.join("node")).unwrap(), load_key(&dir.0.join("stranger")).unwrap(), load_key(&dir.0.join("builder")).unwrap());
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let work = dir.0.clone();
        std::thread::spawn(move || serve(l, &nodes, &builder, &work, &work));
        let job = |pin: &str| serde_json::json!({ "pin": pin, "cpu": "x86-64-v3", "module": "m", "wasm": "00" }).to_string().into_bytes();
        let ours = job(&pin_id(combd_aot::PIN));
        let send = |auth: &str, body: &[u8]| crate::admin::request_as(&url, None, auth, "POST", "/build", body).unwrap().0;
        assert_eq!(send("", &ours), 401, "unsigned");
        assert_eq!(send(&sign_request(&stranger, "POST", "/build", &ours), &ours), 401, "a key it doesn't list");
        assert_eq!(send(&sign_request(&node, "POST", "/build", &job("other")), &ours), 401, "signed over another body");
        assert_eq!(send(&sign_request(&node, "POST", "/build", &job("other")), &job("other")), 409, "another pin: sent again later, not failed");
    }

    /// A pushed job's answer is written as a pull's would be (the build and
    /// its signature, its claim let go); a builder of another pin leaves
    /// the job unanswered and unclaimed, for the next ask.
    #[test]
    fn a_pushed_build_is_written_or_its_job_let_go() {
        let Some(base) = S3::from_env() else {
            return eprintln!("skipping: set COMBD_S3 to test pushes against a real bucket");
        };
        let s3 = S3 { bucket: format!("combd-test-push-{}-{}", std::process::id(), now_ms()), ..base };
        s3.make_bucket().unwrap();
        let dir = crate::store::tempdir::Dir::new();
        new_key(&dir.0.join("node")).unwrap();
        let keyf = dir.0.join("node").to_string_lossy().to_string();
        // A builder that gives one answer, whatever it's sent.
        let builder = |answer: String| {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            std::thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                let _ = crate::admin::read_request(&mut s, |_, _, _| Ok(()));
                let _ = std::io::Write::write_all(&mut s, answer.as_bytes());
            });
            url
        };
        let reply = |status: u16, body: serde_json::Value| format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.to_string().len());
        let got = |m: &str, ext: &str| s3.get(&key("p1", "x86-64-v3", m, ext)).unwrap().map(|(b, _)| b);

        push(&s3, &builder(reply(200, serde_json::json!({ "so": hex(b"native"), "sig": "abcd" }))), &keyf, "p1", "x86-64-v3", b"one");
        pushed();
        let m = module_id(b"one");
        assert_eq!(got(&m, "so").as_deref(), Some(&b"native"[..]));
        assert_eq!(got(&m, "sig").as_deref(), Some(&b"abcd"[..]));
        assert!(got(&m, "claim").is_none(), "its claim is let go");

        push(&s3, &builder(reply(409, serde_json::json!({ "error": "this builder builds for another pin" }))), &keyf, "p1", "x86-64-v3", b"two");
        pushed();
        let m = module_id(b"two");
        assert!(got(&m, "so").is_none() && got(&m, "err").is_none(), "another pin's builder doesn't answer the job");
        assert!(got(&m, "claim").is_none(), "and the next ask may send it again");
    }

    /// A build already on the node is loaded only with a trusted builder's
    /// signature beside it, checked again: one dropped there (unsigned),
    /// changed since, or signed by a key the node doesn't trust, isn't, and
    /// goes.
    #[test]
    fn a_build_on_disk_loads_only_if_its_signature_still_checks() {
        let dir = crate::store::tempdir::Dir::new();
        let public = unhex(&new_key(&dir.0.join("k")).unwrap()).unwrap();
        let k = load_key(&dir.0.join("k")).unwrap();
        let s3 = S3 { pool: Default::default(), host: "unused".into(), tls: false, bucket: "unused".into(), key: String::new(), secret: String::new(), region: "us-east-1".into() };
        let b = Builds { s3, dir: dir.0.join("builds"), keys: vec![public], pin: "p".into(), cpu: "generic".into() };
        std::fs::create_dir_all(&b.dir).unwrap();
        let put = |so: &[u8], sig: Option<String>| {
            std::fs::write(b.dir.join("m.so"), so).unwrap();
            match sig {
                Some(s) => std::fs::write(b.dir.join("m.so.sig"), s).unwrap(),
                None => drop(std::fs::remove_file(b.dir.join("m.so.sig"))),
            }
        };
        let good = hex(k.sign(&signed("p", "generic", "m", b"so")).as_ref());
        put(b"so", None);
        assert!(b.local("m").is_none(), "unsigned");
        assert!(!b.dir.join("m.so").exists(), "and gone");
        put(b"evil", Some(good.clone()));
        assert!(b.local("m").is_none(), "changed since it was signed");
        let stranger = load_key(&{
            new_key(&dir.0.join("k2")).unwrap();
            dir.0.join("k2")
        })
        .unwrap();
        put(b"so", Some(hex(stranger.sign(&signed("p", "generic", "m", b"so")).as_ref())));
        assert!(b.local("m").is_none(), "a builder it doesn't trust");
        put(b"so", Some(good));
        assert_eq!(b.local("m"), Some(b.dir.join("m.so")));
    }
}
