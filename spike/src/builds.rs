//! Compiled modules' builds, through the bucket. Nodes don't compile:
//! builders do (`combd-spike builder`), and a node loads what they made.
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

fn unhex(s: &str) -> Option<Vec<u8>> {
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
    /// The build, checked, on disk.
    Ready(PathBuf),
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

    /// This node's build of `wasm`: here already, or from the bucket (its
    /// signature checked first), or asked for.
    pub fn fetch(&self, wasm: &[u8]) -> Result<Fetch, String> {
        let m = module_id(wasm);
        let local = self.dir.join(format!("{m}.so"));
        if local.exists() {
            return Ok(Fetch::Ready(local));
        }
        let get = |ext: &str| self.s3.get(&key(&self.pin, &self.cpu, &m, ext)).map_err(|e| e.to_string());
        if let Some((err, _)) = get("err")? {
            return Err(format!("module {m} couldn't be built: {}", String::from_utf8_lossy(&err)));
        }
        match (get("so")?, get("sig")?) {
            (Some((so, _)), Some((sig, _))) => {
                let sig = unhex(String::from_utf8_lossy(&sig).trim()).ok_or("a signature that isn't hex")?;
                let msg = signed(&self.pin, &self.cpu, &m, &so);
                if !self.keys.iter().any(|k| UnparsedPublicKey::new(&ED25519, k).verify(&msg, &sig).is_ok()) {
                    return Err(format!("module {m}: its build isn't signed by a builder this node trusts (COMBD_BUILDER_KEYS)"));
                }
                let tmp = self.dir.join(format!("{m}.so.part"));
                std::fs::write(&tmp, &so).and_then(|()| std::fs::rename(&tmp, &local)).map_err(|e| format!("{}: {e}", local.display()))?;
                Ok(Fetch::Ready(local))
            }
            _ => {
                want(&self.s3, &self.pin, &self.cpu, wasm)?;
                Ok(Fetch::Building)
            }
        }
    }
}

/// Ask for a build of `wasm` for `pin` and `cpu` (once: the job is the module).
pub fn want(s3: &S3, pin: &str, cpu: &str, wasm: &[u8]) -> Result<(), String> {
    s3.put(&key(pin, cpu, &module_id(wasm), "wasm"), wasm, &Cond::IfAbsent).map_err(|e| e.to_string())?.ok();
    Ok(())
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

/// A builder: takes jobs for its own pin (it can make no other), one at a
/// time, until `stop`. `aot` is the combd-aot sources it builds against,
/// `work` its cargo directory (kept: the runtime compiles once).
pub fn builder(s3: &S3, id: &str, key: &Ed25519KeyPair, aot: &Path, work: &Path, stop: &std::sync::atomic::AtomicBool) {
    let pin = pin_id(combd_aot::PIN);
    let prefix = format!("_combd/builds/{pin}/");
    eprintln!("builder   {id}: building for {:?} ({pin})", combd_aot::PIN);
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let jobs: Vec<String> = s3.list(&prefix).unwrap_or_default();
        let _ = s3.put(&format!("_combd/builders/{id}"), serde_json::json!({ "pin": pin, "at": now_ms() }).to_string().as_bytes(), &Cond::Any);
        let has = |k: &str| jobs.iter().any(|j| j == k);
        let mut did = false;
        for job in jobs.iter().filter(|j| j.ends_with(".wasm")) {
            let base = job.trim_end_matches(".wasm");
            if has(&format!("{base}.sig")) || has(&format!("{base}.err")) {
                continue;
            }
            let Some((cpu, m)) = base.strip_prefix(&prefix).and_then(|r| r.split_once('/')) else { continue };
            if !claim(s3, &format!("{base}.claim"), id) {
                continue;
            }
            did = true;
            let r = s3.get(job).map_err(|e| e.to_string()).and_then(|w| w.ok_or_else(|| "the job is gone".to_string())).and_then(|(wasm, _)| {
                let out = work.join(format!("{m}-{cpu}.so"));
                let level = (cpu != "generic").then_some(cpu);
                crate::build::build(&wasm, aot, work, &out, None, level)?;
                std::fs::read(&out).map_err(|e| e.to_string())
            });
            match r {
                Ok(so) => {
                    let sig = key.sign(&signed(&pin, cpu, m, &so));
                    let put = |ext: &str, b: &[u8]| s3.put(&format!("{base}.{ext}"), b, &Cond::Any);
                    // The build first, its signature last: a node sees a
                    // build as there only once it's signed.
                    if put("so", &so).is_ok() && put("sig", hex(sig.as_ref()).as_bytes()).is_ok() {
                        eprintln!("builder   {id}: built {m} for {cpu} ({} KB)", so.len() / 1024);
                    }
                }
                Err(e) => {
                    eprintln!("builder   {id}: {m} for {cpu} failed: {e}");
                    let _ = s3.put(&format!("{base}.err"), e.as_bytes(), &Cond::Any);
                }
            }
            let _ = s3.delete(&format!("{base}.claim"));
        }
        if !did {
            std::thread::sleep(Duration::from_secs(2));
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
    Ed25519KeyPair::from_pkcs8(&b).map_err(|_| format!("{}: not a builder key (combd-spike builder-key)", file.display()))
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
}
