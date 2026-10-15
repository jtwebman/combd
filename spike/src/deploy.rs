//! Deploys through the bucket. An app's versions, its live pointer, and its
//! vars live under `apps/<org>/<app>/`:
//! - `versions/<hash>`: a bundle (the manifest and its modules), named by
//!   the SHA-256 of its bytes, so uploading the same one twice is free;
//! - `current`: `{"version": "<hash>"}`, which version is live. It only
//!   moves (compare-and-swap) after the version passed every check;
//! - `vars`: the app's var values, as JSON. (Secrets come next, encrypted.)
//!
//! Every node lists the `current` pointers every couple of seconds and swaps
//! in whatever changed (`sync`). A node that can't load a version keeps
//! serving the one it has.

use crate::manifest::EnvValues;
use crate::s3::{Cond, S3};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A bundle's limits: the whole of it, and how many files.
pub const MAX_BUNDLE: usize = 64 << 20;
const MAX_FILES: usize = 256;

/// The files of an app's folder that make a version: its manifest (one of
/// three names) and `modules/*.wasm`. Nothing else, so nothing private (the
/// dev environment's file) can ride along.
pub fn bundle(dir: &Path) -> Result<Vec<u8>, String> {
    let mut files: Vec<(String, Vec<u8>)> = vec![];
    for n in ["combd.toml", "combd.yaml", "combd.json"] {
        if let Ok(b) = std::fs::read(dir.join(n)) {
            files.push((n.into(), b));
        }
    }
    if files.len() != 1 {
        return Err("want exactly one of combd.toml, combd.yaml, combd.json".into());
    }
    let mut mods: Vec<PathBuf> = std::fs::read_dir(dir.join("modules")).map(|r| r.filter_map(|e| Some(e.ok()?.path())).filter(|p| p.extension().is_some_and(|x| x == "wasm")).collect()).unwrap_or_default();
    mods.sort();
    for p in mods {
        let name = format!("modules/{}", p.file_name().unwrap().to_string_lossy());
        files.push((name, std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?));
    }
    encode(&files)
}

fn encode(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut b = b"combd-bundle-1\n".to_vec();
    b.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (n, data) in files {
        b.extend_from_slice(&(n.len() as u16).to_le_bytes());
        b.extend_from_slice(n.as_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
    }
    if b.len() > MAX_BUNDLE {
        return Err(format!("the bundle is {} bytes: at most {MAX_BUNDLE}", b.len()));
    }
    Ok(b)
}

/// A bundle's files. It comes from a client, so every name is checked: a
/// manifest, or `modules/<name>.wasm` with a plain name. No paths.
pub fn decode(b: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let bad = || "not a combd bundle".to_string();
    if b.len() > MAX_BUNDLE {
        return Err("too big".into());
    }
    let mut r = b.strip_prefix(b"combd-bundle-1\n".as_slice()).ok_or_else(bad)?;
    let mut take = |n: usize| -> Result<&[u8], String> {
        if r.len() < n {
            return Err(bad());
        }
        let (a, rest) = r.split_at(n);
        r = rest;
        Ok(a)
    };
    let count = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
    if count > MAX_FILES {
        return Err(format!("{count} files: at most {MAX_FILES}"));
    }
    let mut files = vec![];
    for _ in 0..count {
        let nl = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
        let name = String::from_utf8(take(nl)?.to_vec()).map_err(|_| bad())?;
        let dl = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let data = take(dl)?.to_vec();
        let module_ok = name.strip_prefix("modules/").and_then(|m| m.strip_suffix(".wasm")).is_some_and(|m| !m.is_empty() && m.len() <= 63 && m.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_'));
        if !module_ok && !["combd.toml", "combd.yaml", "combd.json"].contains(&name.as_str()) {
            return Err(format!("{name:?}: a bundle holds a manifest and modules/<name>.wasm only"));
        }
        if files.iter().any(|(n, _): &(String, Vec<u8>)| *n == name) {
            return Err(format!("{name} twice"));
        }
        files.push((name, data));
    }
    if !r.is_empty() {
        return Err(bad());
    }
    Ok(files)
}

/// A version's name: the SHA-256 of its bundle.
pub fn version_of(bundle: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bundle).iter().map(|b| format!("{b:02x}")).collect()
}

/// Write a bundle's files into `dir` (made fresh), as an app folder.
pub fn unpack(bundle: &[u8], dir: &Path) -> Result<(), String> {
    let files = decode(bundle)?;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("modules")).map_err(|e| e.to_string())?;
    for (n, data) in files {
        std::fs::write(dir.join(&n), data).map_err(|e| format!("{n}: {e}"))?;
    }
    Ok(())
}

fn key(org: &str, app: &str, what: &str) -> String {
    format!("apps/{org}/{app}/{what}")
}

/// The live version of an app, and the pointer's etag (for a CAS).
pub fn current(s3: &S3, org: &str, app: &str) -> Result<Option<(String, String)>, String> {
    let Some((b, etag)) = s3.get(&key(org, app, "current")).map_err(|e| e.to_string())? else { return Ok(None) };
    let v: Json = serde_json::from_slice(&b).map_err(|_| "a bad current pointer".to_string())?;
    Ok(v["version"].as_str().map(|s| (s.to_string(), etag)))
}

pub fn put_version(s3: &S3, org: &str, app: &str, bundle: &[u8]) -> Result<String, String> {
    let v = version_of(bundle);
    s3.put(&key(org, app, &format!("versions/{v}")), bundle, &Cond::Any).map_err(|e| e.to_string())?.map_err(|()| "upload refused".to_string())?;
    Ok(v)
}

pub fn get_version(s3: &S3, org: &str, app: &str, v: &str) -> Result<Vec<u8>, String> {
    let b = s3.get(&key(org, app, &format!("versions/{v}"))).map_err(|e| e.to_string())?.ok_or(format!("no version {v}"))?.0;
    if version_of(&b) != v {
        return Err(format!("version {v}: its bytes don't match its name"));
    }
    Ok(b)
}

/// Make `v` live, if the pointer is still at `was` (its etag; `None`: no
/// version yet). `Err` if someone else moved it first: look again.
pub fn set_current(s3: &S3, org: &str, app: &str, v: &str, was: Option<&str>) -> Result<(), String> {
    let cond = match was {
        Some(e) => Cond::IfMatch(e.into()),
        None => Cond::IfAbsent,
    };
    let body = serde_json::json!({ "version": v }).to_string();
    s3.put(&key(org, app, "current"), body.as_bytes(), &cond).map_err(|e| e.to_string())?.map_err(|()| "another deploy moved the pointer first: try again".to_string())?;
    Ok(())
}

/// Take an app down: no version is live.
pub fn remove(s3: &S3, org: &str, app: &str) -> Result<(), String> {
    s3.delete(&key(org, app, "current")).map_err(|e| e.to_string())
}

/// An app's env values, and their etag.
pub fn env(s3: &S3, org: &str, app: &str) -> Result<(EnvValues, String), String> {
    match s3.get(&key(org, app, "vars")).map_err(|e| e.to_string())? {
        None => Ok((EnvValues::default(), String::new())),
        Some((b, etag)) => {
            let vars: BTreeMap<String, Json> = serde_json::from_slice(&b).map_err(|_| "bad vars".to_string())?;
            Ok((EnvValues { vars, secrets: BTreeMap::new() }, etag))
        }
    }
}

pub fn put_vars(s3: &S3, org: &str, app: &str, vars: &BTreeMap<String, Json>) -> Result<(), String> {
    let body = serde_json::to_vec(vars).unwrap();
    s3.put(&key(org, app, "vars"), &body, &Cond::Any).map_err(|e| e.to_string())?.map_err(|()| "refused".to_string())?;
    Ok(())
}

/// Every app with a live version: (org, app).
pub fn apps(s3: &S3) -> Result<Vec<(String, String)>, String> {
    let keys = s3.list("apps/").map_err(|e| e.to_string())?;
    Ok(keys
        .iter()
        .filter_map(|k| {
            let rest = k.strip_prefix("apps/")?.strip_suffix("/current")?;
            let (org, app) = rest.split_once('/')?;
            (!app.contains('/')).then(|| (org.to_string(), app.to_string()))
        })
        .collect())
}

/// What a node has loaded, per app: (version, vars etag).
pub type Known = BTreeMap<(String, String), (String, String)>;

/// One app to (re)load: its folder (unpacked in `cache`) and env values.
pub struct Change {
    pub org: String,
    pub app: String,
    pub dir: PathBuf,
    pub env: EnvValues,
    pub version: String,
    pub vars_etag: String,
}

/// Compare the bucket with what's `known`: the apps that are new or changed
/// (their version or their vars), and the ones taken down.
pub fn sync(s3: &S3, cache: &Path, known: &Known) -> Result<(Vec<Change>, Vec<(String, String)>), String> {
    let live = apps(s3)?;
    let mut changes = vec![];
    for (org, app) in &live {
        let Some((v, _)) = current(s3, org, app)? else { continue };
        let (env, etag) = env(s3, org, app)?;
        if known.get(&(org.clone(), app.clone())) == Some(&(v.clone(), etag.clone())) {
            continue;
        }
        let dir = cache.join(org).join(app).join(&v);
        if !dir.join("modules").is_dir() {
            unpack(&get_version(s3, org, app, &v)?, &dir)?;
        }
        changes.push(Change { org: org.clone(), app: app.clone(), dir, env, version: v, vars_etag: etag });
    }
    let gone = known.keys().filter(|k| !live.contains(k)).cloned().collect();
    Ok((changes, gone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_carry_a_manifest_and_modules_only() {
        let d = crate::store::tempdir::Dir::new();
        std::fs::create_dir(d.0.join("modules")).unwrap();
        std::fs::write(d.0.join("combd.toml"), "[app]\nname = \"a\"\n").unwrap();
        std::fs::write(d.0.join("combd.dev.toml"), "[secrets]\nk = \"never shipped\"\n").unwrap();
        std::fs::write(d.0.join("modules").join("api.wasm"), b"\0asm").unwrap();
        let b = bundle(&d.0).unwrap();
        let files = decode(&b).unwrap();
        assert_eq!(files.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), ["combd.toml", "modules/api.wasm"]);
        assert!(!String::from_utf8_lossy(&b).contains("never shipped"));
        assert_eq!(version_of(&b), version_of(&bundle(&d.0).unwrap()), "the same files, the same version");
        // From a client: no paths, no strangers, no tricks.
        for name in ["../etc/passwd", "modules/../../x.wasm", "modules/A.wasm", "combd.dev.toml", "modules/.wasm"] {
            let b = encode(&[(name.to_string(), vec![1])]).unwrap();
            assert!(decode(&b).is_err(), "{name}");
        }
        assert!(decode(&encode(&[("combd.toml".into(), vec![]), ("combd.toml".into(), vec![])]).unwrap()).is_err());
        assert!(decode(b"combd-bundle-1\n\xff\xff\xff\xff").is_err());
        let mut cut = b.clone();
        cut.pop();
        assert!(decode(&cut).is_err());
    }

    /// Bundles come from clients: damaged at random, the decoder must never
    /// panic, and never accept a name it shouldn't.
    #[test]
    fn mangled_bundles_never_panic() {
        let good = encode(&[("combd.toml".into(), b"[app]".to_vec()), ("modules/api.wasm".into(), vec![0, 97, 115, 109])]).unwrap();
        let mut rng = crate::sim::Rng::new(7);
        for _ in 0..100_000 {
            let mut b = good.clone();
            for _ in 0..1 + rng.below(4) {
                if b.is_empty() {
                    b.push(0);
                }
                let at = rng.below(b.len() as u64) as usize;
                match rng.below(4) {
                    0 => b[at] ^= 1 << rng.below(8),
                    1 => b.truncate(at),
                    2 => b.insert(at, rng.next() as u8),
                    _ => b.splice(at..at, [0xff, 0xff, 0xff, 0x7f]).for_each(drop),
                }
            }
            if let Ok(files) = std::panic::catch_unwind(|| decode(&b)).expect("the decoder panicked") {
                for (n, _) in files {
                    assert!(n == "combd.toml" || n == "combd.yaml" || n == "combd.json" || (n.starts_with("modules/") && n.ends_with(".wasm") && !n.contains("..")), "{n:?}");
                }
            }
        }
    }
}
