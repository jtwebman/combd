//! Deploys. An app's versions are bundles in the bucket,
//! `apps/<org>/<app>/versions/<hash>` (the manifest and its modules, named
//! by the SHA-256 of its bytes, so uploading the same one twice is free).
//! Which version is live, and the app's vars, are in the catalog
//! (`catalog.rs`, `catnode.rs`): `<org>/<app>` is `{"version", "domains"}`
//! (empty when taken down), and `<org>/<app>#vars` the vars. A node swaps
//! in whatever changed whenever its copy of the catalog moves. A node that
//! can't load a version keeps serving the one it has.

use crate::catalog::State;
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

/// The catalog entry that makes `version` live, on these domains.
pub fn live_value(version: &str, domains: &[String]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "version": version, "domains": domains })).unwrap()
}

/// An app's live version, by the catalog.
pub fn live(state: &State, org: &str, app: &str) -> Option<String> {
    let v: Json = serde_json::from_slice(state.apps.get(&format!("{org}/{app}"))?).ok()?;
    v["version"].as_str().map(String::from)
}

/// An app's vars, by the catalog.
pub fn vars(state: &State, org: &str, app: &str) -> BTreeMap<String, Json> {
    state.apps.get(&format!("{org}/{app}#vars")).and_then(|b| serde_json::from_slice(b).ok()).unwrap_or_default()
}

/// What a node has loaded, per app: (version, its settings as stored).
pub type Known = BTreeMap<(String, String), (String, Vec<u8>)>;

/// One app to (re)load: its folder (unpacked in `cache`) and env values
/// (or why they can't be had, such as a secret that won't open).
pub struct Change {
    pub org: String,
    pub app: String,
    pub dir: PathBuf,
    pub env: Result<EnvValues, String>,
    pub version: String,
    pub raw: Vec<u8>,
}

/// An app's settings (vars, secrets, data key) as stored: a change to any
/// of them reloads it.
fn settings_raw(state: &State, org: &str, app: &str) -> Vec<u8> {
    let p = format!("{org}/{app}#");
    // Not a pool's manager: that changing reloads nothing.
    serde_json::to_vec(&state.apps.range(p.clone()..).take_while(|(k, _)| k.starts_with(&p)).filter(|(k, _)| !k.contains("#pool/")).collect::<Vec<_>>()).unwrap()
}

/// The app's env values: its vars, and its secrets opened.
pub fn env(state: &State, master: Option<&crate::secrets::Key>, org: &str, app: &str) -> Result<EnvValues, String> {
    Ok(EnvValues { vars: vars(state, org, app), secrets: crate::secrets::open_all(master, state, org, app)? })
}

/// Compare the catalog with what's `known`: the apps that are new or
/// changed (their version or their settings), and the ones taken down.
pub fn changes(state: &State, s3: &S3, cache: &Path, known: &Known, master: Option<&crate::secrets::Key>) -> Result<(Vec<Change>, Vec<(String, String)>), String> {
    let mut live_apps = vec![];
    let mut out = vec![];
    for k in state.apps.keys().filter(|k| !k.contains('#')) {
        let Some((org, app)) = k.split_once('/') else { continue };
        let Some(v) = live(state, org, app) else { continue };
        live_apps.push((org.to_string(), app.to_string()));
        let raw = settings_raw(state, org, app);
        if known.get(&(org.to_string(), app.to_string())) == Some(&(v.clone(), raw.clone())) {
            continue;
        }
        let dir = cache.join(org).join(app).join(&v);
        if !dir.join("modules").is_dir() {
            unpack(&get_version(s3, org, app, &v)?, &dir)?;
        }
        out.push(Change { org: org.into(), app: app.into(), dir, env: env(state, master, org, app), version: v, raw });
    }
    let gone = known.keys().filter(|k| !live_apps.contains(k)).cloned().collect();
    Ok((out, gone))
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
