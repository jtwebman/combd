//! Secrets: kept sealed in the catalog, and opened only on a node, in
//! memory, to hand to an app's instances as they start.
//!
//! Envelope encryption: each app has its own data key, made with its first
//! secret and sealed by the fleet's master key (`COMBD_MASTER_KEY_FILE`; a
//! KMS can hold it later, and then only data keys go to it). Each secret is
//! sealed by its app's data key with the app and its name bound in, so a
//! sealed value copied to another name or app won't open. AES-256-GCM, with
//! a random nonce each time.
//!
//! In the catalog: `<org>/<app>#key` (the sealed data key, written once) and
//! `<org>/<app>#secret/<name>` (a sealed value; empty once deleted). The
//! bucket and every node's copy on disk only ever hold these.

use crate::catalog::State;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::BTreeMap;
use std::path::Path;

/// The longest secret value: every node holds the catalog.
pub const MAX_VALUE: usize = 64 * 1024;

pub struct Key(LessSafeKey);

impl Key {
    fn new(b: &[u8; 32]) -> Key {
        Key(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, b).expect("a 32-byte key")))
    }

    /// The master key: a file of 64 hex characters, that only its owner may
    /// read (`combd master-key <file>` makes one).
    pub fn load(path: &Path) -> Result<Key, String> {
        let at = path.display();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).map_err(|e| format!("{at}: {e}"))?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(format!("{at}: others may read it (mode {:o}): chmod 600 {at}", mode & 0o777));
            }
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{at}: {e}"))?;
        let b = unhex(text.trim()).ok_or(format!("{at}: want 64 hex characters"))?;
        Ok(Key::new(&b))
    }

    fn seal(&self, plain: &[u8], aad: &str) -> Vec<u8> {
        let mut n = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut n).expect("the system's random source");
        let mut out = plain.to_vec();
        self.0.seal_in_place_append_tag(Nonce::assume_unique_for_key(n), Aad::from(aad.as_bytes()), &mut out).expect("sealing");
        [n.to_vec(), out].concat()
    }

    fn open(&self, sealed: &[u8], aad: &str) -> Option<Vec<u8>> {
        let (n, c) = sealed.split_at_checked(NONCE_LEN)?;
        let mut c = c.to_vec();
        Some(self.0.open_in_place(Nonce::try_assume_unique_for_key(n).ok()?, Aad::from(aad.as_bytes()), &mut c).ok()?.to_vec())
    }
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    let b: Vec<u8> = (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok())).collect::<Option<_>>()?;
    b.try_into().ok()
}

fn random() -> [u8; 32] {
    let mut b = [0u8; 32];
    SystemRandom::new().fill(&mut b).expect("the system's random source");
    b
}

/// A new master key, written to `path` (never over one that's there).
pub fn new_master(path: &Path) -> Result<(), String> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let hex: String = random().iter().map(|b| format!("{b:02x}")).collect();
    writeln!(f, "{hex}").map_err(|e| e.to_string())
}

/// Secret names: a-z, 0-9, `-`, `_`, and `.`, up to 128.
pub fn valid_name(n: &str) -> bool {
    (1..=128).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
}

pub fn key_entry(org: &str, app: &str) -> String {
    format!("{org}/{app}#key")
}

pub fn secret_entry(org: &str, app: &str, name: &str) -> String {
    format!("{org}/{app}#secret/{name}")
}

/// The app's data key, opened (`None`: it has none yet).
pub fn open_key(master: &Key, state: &State, org: &str, app: &str) -> Result<Option<Key>, String> {
    let Some(sealed) = state.apps.get(&key_entry(org, app)) else { return Ok(None) };
    let b = master.open(sealed, &format!("combd-key:{org}/{app}")).ok_or(format!("{org}/{app}: its data key won't open with this node's master key"))?;
    Ok(Some(Key::new(&b.try_into().map_err(|_| "a data key is 32 bytes")?)))
}

/// A new data key for the app, and how it goes in the catalog (sealed).
pub fn new_key(master: &Key, org: &str, app: &str) -> (Key, Vec<u8>) {
    let b = random();
    (Key::new(&b), master.seal(&b, &format!("combd-key:{org}/{app}")))
}

/// A value sealed for the catalog.
pub fn seal(key: &Key, org: &str, app: &str, name: &str, value: &str) -> Vec<u8> {
    key.seal(value.as_bytes(), &format!("combd-secret:{org}/{app}/{name}"))
}

/// Every secret the app has, opened, by name. Errors never show a value.
pub fn open_all(master: Option<&Key>, state: &State, org: &str, app: &str) -> Result<BTreeMap<String, String>, String> {
    let prefix = secret_entry(org, app, "");
    let sealed: Vec<(&str, &Vec<u8>)> = state.apps.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).filter(|(_, v)| !v.is_empty()).map(|(k, v)| (&k[prefix.len()..], v)).collect();
    if sealed.is_empty() {
        return Ok(BTreeMap::new());
    }
    let master = master.ok_or("it has secrets, and this node has no master key (COMBD_MASTER_KEY_FILE)")?;
    let key = open_key(master, state, org, app)?.ok_or("it has secrets but no data key")?;
    let mut out = BTreeMap::new();
    for (name, v) in sealed {
        let plain = key.open(v, &format!("combd-secret:{org}/{app}/{name}")).ok_or(format!("secret {name:?} won't open: it was sealed for another app or name, or damaged"))?;
        out.insert(name.to_string(), String::from_utf8(plain).map_err(|_| format!("secret {name:?} isn't text"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_secrets_open_only_where_they_belong() {
        let master = Key::new(&random());
        let mut s = State::default();
        let (key, sealed_key) = new_key(&master, "acme", "shop");
        s.apps.insert(key_entry("acme", "shop"), sealed_key.clone());
        s.apps.insert(secret_entry("acme", "shop", "pg-url"), seal(&key, "acme", "shop", "pg-url", "postgres://db"));
        s.apps.insert(secret_entry("acme", "shop", "gone"), vec![]);
        assert_eq!(open_all(Some(&master), &s, "acme", "shop").unwrap(), BTreeMap::from([("pg-url".to_string(), "postgres://db".to_string())]));
        assert!(!sealed_key.windows(4).any(|w| w == b"post") && !s.apps.values().any(|v| v.windows(8).any(|w| w == b"postgres")));
        assert!(open_all(Some(&master), &s, "acme", "blog").unwrap().is_empty(), "another app: nothing");
        // Another master key, or none: refused.
        assert!(open_all(Some(&Key::new(&random())), &s, "acme", "shop").unwrap_err().contains("won't open with this node's master key"));
        assert!(open_all(None, &s, "acme", "shop").unwrap_err().contains("no master key"));
        // A sealed value copied to another name, or the data key to another
        // app, won't open.
        let mut moved = s.clone();
        moved.apps.insert(secret_entry("acme", "shop", "other"), s.apps[&secret_entry("acme", "shop", "pg-url")].clone());
        assert!(open_all(Some(&master), &moved, "acme", "shop").unwrap_err().contains("\"other\" won't open"));
        let mut moved = s.clone();
        moved.apps.insert(key_entry("acme", "blog"), sealed_key);
        moved.apps.insert(secret_entry("acme", "blog", "pg-url"), s.apps[&secret_entry("acme", "shop", "pg-url")].clone());
        assert!(open_all(Some(&master), &moved, "acme", "blog").is_err());
        // One flipped bit: refused.
        let mut bad = s.clone();
        bad.apps.get_mut(&secret_entry("acme", "shop", "pg-url")).unwrap()[20] ^= 1;
        assert!(open_all(Some(&master), &bad, "acme", "shop").is_err());
    }

    #[test]
    fn master_key_files() {
        let d = crate::store::tempdir::Dir::new();
        let p = d.0.join("master.key");
        new_master(&p).unwrap();
        assert!(new_master(&p).is_err(), "never over one that's there");
        Key::load(&p).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(Key::load(&p).err().unwrap().contains("chmod 600"));
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(&p, "abc\n").unwrap();
        assert!(Key::load(&p).err().unwrap().contains("64 hex"));
        assert!(valid_name("pg-url") && valid_name("a.b_c") && !valid_name("") && !valid_name("PG") && !valid_name("a/b") && !valid_name(&"a".repeat(129)));
    }
}
