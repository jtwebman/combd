//! The admin API: deploys, vars, rollbacks, on its own port. combd knows
//! nothing about people, only tokens and what each may do: a scope like
//! `acme/shop:deploy`, `acme/*:vars`, or `*/*:*`. Users, orgs, invites, and
//! logins live outside (your CI and identity system, or a hosted dashboard
//! that holds the tokens).
//!
//! Tokens are stored as SHA-256 hashes, never as themselves, and compared in
//! constant time. With no TLS the API listens on localhost only; on any other
//! address it needs TLS, so a token never crosses a network in the clear.
//!
//! ```text
//! PUT    /v1/apps/<org>/<app>           a bundle: check it, upload it, make it live
//! GET    /v1/apps/<org>/<app>           the live version
//! POST   /v1/apps/<org>/<app>/rollback  {"version"}: make an earlier one live
//! DELETE /v1/apps/<org>/<app>           take it down
//! PUT    /v1/apps/<org>/<app>/vars      {"NAME": value, ...}: set vars
//! PUT    /v1/apps/<org>/<app>/secrets/<name>   the value (the body): set it
//! DELETE /v1/apps/<org>/<app>/secrets/<name>   delete it
//! ```
//!
//! Secrets are write-only: nothing reads one back.

use serde_json::{json, Value as Json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Arc;

pub struct AdminCfg {
    pub port: u16,
    /// Bind to every address (needs `tls`), or localhost only.
    pub public: bool,
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// (SHA-256 of a token, its scopes).
    pub tokens: Vec<([u8; 32], Vec<String>)>,
}

impl AdminCfg {
    /// `COMBD_ADMIN_PORT`, and `COMBD_ADMIN_TOKENS` (a file of lines
    /// `<sha256 hex> <scope> [scope...]`). TLS (and listening beyond
    /// localhost) with the wire's certificates.
    pub fn from_env(certs: Option<&std::path::Path>, node: u32) -> Result<Option<AdminCfg>, String> {
        let Some(port) = std::env::var("COMBD_ADMIN_PORT").ok() else { return Ok(None) };
        let port = port.parse().map_err(|_| "COMBD_ADMIN_PORT: want a port")?;
        let file = std::env::var("COMBD_ADMIN_TOKENS").map_err(|_| "COMBD_ADMIN_PORT needs COMBD_ADMIN_TOKENS (a file of token hashes)")?;
        let tokens = parse_tokens(&std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?)?;
        let tls = certs.map(|d| crate::wire::admin_tls(d, node)).transpose()?;
        Ok(Some(AdminCfg { port, public: tls.is_some(), tls, tokens }))
    }
}

pub fn parse_tokens(text: &str) -> Result<Vec<([u8; 32], Vec<String>)>, String> {
    let mut out = vec![];
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty() && !l.trim_start().starts_with('#')) {
        let mut w = line.split_whitespace();
        let hex = w.next().unwrap_or("");
        let hash: Vec<u8> = (0..hex.len()).step_by(2).filter_map(|j| hex.get(j..j + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
        let hash: [u8; 32] = hash.try_into().map_err(|_| format!("line {}: want a SHA-256 in hex", i + 1))?;
        let scopes: Vec<String> = w.map(String::from).collect();
        for s in &scopes {
            if !s.contains(':') || s.split(['/', ':']).count() != 3 {
                return Err(format!("line {}: scope {s:?}: want <org>/<app>:<action>", i + 1));
            }
        }
        out.push((hash, scopes));
    }
    Ok(out)
}

/// A new random token, and its line for the tokens file.
pub fn new_token(scopes: &[&str]) -> (String, String) {
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("random bytes");
    let token = format!("combd_{}", hex(&b));
    (token.clone(), format!("{} {}", hex(&sha(&token)), scopes.join(" ")))
}

fn sha(s: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes()).into()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// May this token do `action` on `org/app`?
fn allowed(tokens: &[([u8; 32], Vec<String>)], token: &str, org: &str, app: &str, action: &str) -> bool {
    let h = sha(token);
    // Constant time: every byte of every hash, whatever matches.
    let mut found: Option<&Vec<String>> = None;
    for (t, scopes) in tokens {
        let same = t.iter().zip(h.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
        if same {
            found = Some(scopes);
        }
    }
    let ok = |want: &str, have: &str| have == "*" || have == want;
    found.is_some_and(|scopes| {
        scopes.iter().any(|s| {
            let (path, act) = s.split_once(':').unwrap_or(("", ""));
            let (o, a) = path.split_once('/').unwrap_or(("", ""));
            ok(org, o) && ok(app, a) && ok(action, act)
        })
    })
}

/// What the API does, given a request it has checked: (status, JSON body).
pub trait Api: Sync {
    fn deploy(&self, org: &str, app: &str, bundle: &[u8]) -> Result<String, String>;
    fn current(&self, org: &str, app: &str) -> Result<Option<String>, String>;
    fn rollback(&self, org: &str, app: &str, version: &str) -> Result<(), String>;
    fn remove(&self, org: &str, app: &str) -> Result<(), String>;
    fn set_vars(&self, org: &str, app: &str, vars: BTreeMap<String, Json>) -> Result<(), String>;
    /// `None`: delete it.
    fn set_secret(&self, org: &str, app: &str, name: &str, value: Option<&[u8]>) -> Result<(), String>;
}

/// Serve the admin API (blocking: a thread per connection; it's for
/// deploys, not traffic).
pub fn serve(cfg: AdminCfg, api: &dyn Api, stop: &std::sync::atomic::AtomicBool) -> std::io::Result<()> {
    if cfg.public && cfg.tls.is_none() {
        return Err(std::io::Error::other("the admin API listens beyond localhost only with TLS"));
    }
    let addr = if cfg.public { "0.0.0.0" } else { "127.0.0.1" };
    let l = std::net::TcpListener::bind((addr, cfg.port))?;
    l.set_nonblocking(true)?;
    std::thread::scope(|sc| {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match l.accept() {
                Ok((s, _)) => {
                    let cfg = &cfg;
                    sc.spawn(move || {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                        let Ok(raw) = s.try_clone() else { return };
                        match &cfg.tls {
                            Some(tls) => {
                                let Ok(conn) = rustls::ServerConnection::new(tls.clone()) else { return };
                                let mut t = rustls::StreamOwned::new(conn, s);
                                handle(&mut t, &raw, cfg, api);
                            }
                            None => {
                                let mut s = s;
                                handle(&mut s, &raw, cfg, api);
                            }
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => {}
            }
        }
    });
    Ok(())
}

fn handle(s: &mut (impl Read + Write), raw: &std::net::TcpStream, cfg: &AdminCfg, api: &dyn Api) {
    // The token is checked on the headers, before any of a body is read.
    let (status, body, early) = match read_request(s, |m, p, t| authorize(cfg, m, p, t)) {
        Ok(Ok((method, path, body))) => {
            let (st, b) = run(api, &method, &path, &body);
            (st, b, false)
        }
        Ok(Err((st, b))) => (st, b, true),
        Err(e) => (400, json!({ "error": e }), false),
    };
    let body = body.to_string();
    let _ = write!(s, "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
    let _ = s.flush();
    if early {
        // Refused before its body: read a little of what the client is still
        // sending (1 MB, 2 s at most) before closing, or the close resets the
        // connection and the client may never see why. Never all of it.
        let _ = raw.set_read_timeout(Some(std::time::Duration::from_millis(200)));
        let t = std::time::Instant::now();
        let mut junk = [0u8; 16 * 1024];
        let mut n = 0;
        while n < 1 << 20 && t.elapsed() < std::time::Duration::from_secs(2) {
            match s.read(&mut junk) {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k,
            }
        }
    }
}

type Refused = (u16, Json);

/// One request: method, path, body (at most a bundle's size). `check` sees
/// the method, path, and token first; if it refuses, no body is read.
fn read_request(s: &mut impl Read, check: impl Fn(&str, &str, Option<&str>) -> Result<(), Refused>) -> Result<Result<(String, String, Vec<u8>), Refused>, String> {
    let mut buf = vec![];
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("closed early".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Partial) if buf.len() < 64 * 1024 => continue,
            Ok(httparse::Status::Partial) => return Err("headers too long".into()),
            Err(_) => return Err("bad request".into()),
            Ok(httparse::Status::Complete(hlen)) => {
                let h = |name: &str| req.headers.iter().find(|x| x.name.eq_ignore_ascii_case(name)).and_then(|x| std::str::from_utf8(x.value).ok());
                let token = h("authorization").and_then(|v| v.strip_prefix("Bearer ")).map(|t| t.trim().to_string());
                let clen: usize = h("content-length").map_or(Ok(0), |v| v.trim().parse().map_err(|_| "bad content-length".to_string()))?;
                if clen > crate::deploy::MAX_BUNDLE {
                    return Err("too big".into());
                }
                let (method, path) = (req.method.unwrap_or("").to_string(), req.path.unwrap_or("").to_string());
                if let Err(refused) = check(&method, &path, token.as_deref()) {
                    return Ok(Err(refused));
                }
                let mut body = buf[hlen..].to_vec();
                while body.len() < clen {
                    let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
                    if n == 0 {
                        return Err("closed early".into());
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                body.truncate(clen);
                return Ok(Ok((method, path, body)));
            }
        }
    }
}

/// The endpoint: (org, app, the rest of the path, the action it needs).
fn endpoint<'a>(method: &str, path: &'a str) -> Option<(&'a str, &'a str, Vec<&'a str>, &'static str)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (org, app, rest) = match parts.as_slice() {
        ["v1", "apps", org, app, rest @ ..] => (*org, *app, rest.to_vec()),
        _ => return None,
    };
    let action = match (method, rest.as_slice()) {
        ("GET", []) => "read",
        ("PUT", ["vars"]) => "vars",
        ("PUT", ["secrets", _]) | ("DELETE", ["secrets", _]) => "secrets",
        ("PUT", []) | ("DELETE", []) | ("POST", ["rollback"]) => "deploy",
        _ => return None,
    };
    Some((org, app, rest, action))
}

fn authorize(cfg: &AdminCfg, method: &str, path: &str, token: Option<&str>) -> Result<(), Refused> {
    let Some((org, app, _, action)) = endpoint(method, path) else { return Err((404, json!({ "error": "no such endpoint" }))) };
    let Some(token) = token else { return Err((401, json!({ "error": "want Authorization: Bearer <token>" }))) };
    if !allowed(&cfg.tokens, token, org, app, action) {
        // The same answer for a bad token and a weak one: no hints.
        return Err((403, json!({ "error": format!("this token may not {action} {org}/{app}") })));
    }
    Ok(())
}

fn run(api: &dyn Api, method: &str, path: &str, body: &[u8]) -> (u16, Json) {
    let Some((org, app, rest, _)) = endpoint(method, path) else { return (404, json!({ "error": "no such endpoint" })) };
    let r = match (method, rest.as_slice()) {
        ("PUT", []) => api.deploy(org, app, body).map(|v| json!({ "version": v })),
        ("GET", []) => api.current(org, app).map(|v| json!({ "version": v })),
        ("POST", ["rollback"]) => match serde_json::from_slice::<Json>(body).ok().and_then(|v| v["version"].as_str().map(String::from)) {
            Some(v) => api.rollback(org, app, &v).map(|()| json!({ "version": v })),
            None => Err("want {\"version\": \"...\"}".into()),
        },
        ("DELETE", []) => api.remove(org, app).map(|()| json!({ "removed": true })),
        ("PUT", ["secrets", name]) => api.set_secret(org, app, name, Some(body)).map(|()| json!({ "ok": true })),
        ("DELETE", ["secrets", name]) => api.set_secret(org, app, name, None).map(|()| json!({ "ok": true })),
        _ => match serde_json::from_slice::<BTreeMap<String, Json>>(body) {
            Ok(vars) => api.set_vars(org, app, vars).map(|()| json!({ "ok": true })),
            Err(_) => Err("want {\"NAME\": value, ...}".into()),
        },
    };
    match r {
        Ok(v) => (200, v),
        Err(e) => (400, json!({ "error": e })),
    }
}

/// A client call, for the CLI and tests: `url` is `http://host:port` (for
/// localhost) or `https://host:port` (then `tls` and the server's name).
pub fn request(url: &str, tls: Option<(&Arc<rustls::ClientConfig>, &str)>, token: &str, method: &str, path: &str, body: &[u8]) -> Result<(u16, Json), String> {
    let hostport = url.split_once("://").map_or(url, |(_, r)| r).trim_end_matches('/');
    let s = std::net::TcpStream::connect(hostport).map_err(|e| format!("{hostport}: {e}"))?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(120)));
    let head = format!("{method} {path} HTTP/1.1\r\nhost: {hostport}\r\nauthorization: Bearer {token}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
    let mut out = vec![];
    let go = |s: &mut dyn ReadWrite, out: &mut Vec<u8>| -> std::io::Result<()> {
        // A server may refuse before the body is all sent: still read why.
        let sent = s.write_all(head.as_bytes()).and_then(|()| s.write_all(body)).and_then(|()| s.flush());
        match s.read_to_end(out) {
            Ok(_) => Ok(()),
            // A peer that closes without close_notify, or resets after its
            // answer: we have the answer.
            Err(_) if !out.is_empty() => Ok(()),
            Err(e) => Err(sent.err().unwrap_or(e)),
        }
    };
    match (url.starts_with("https://"), tls) {
        (true, Some((cfg, name))) => {
            let name = rustls::pki_types::ServerName::try_from(name.to_string()).map_err(|e| e.to_string())?;
            let conn = rustls::ClientConnection::new(cfg.clone(), name).map_err(|e| e.to_string())?;
            go(&mut rustls::StreamOwned::new(conn, s), &mut out).map_err(|e| e.to_string())?;
        }
        (true, None) => return Err("https needs the fleet CA".into()),
        (false, _) => go(&mut { s }, &mut out).map_err(|e| e.to_string())?,
    }
    let text = String::from_utf8_lossy(&out);
    let status = text.get(9..12).and_then(|c| c.parse().ok()).ok_or("no answer")?;
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    Ok((status, serde_json::from_str(body).unwrap_or(Json::Null)))
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests come from the internet: damaged at random, the reader must
    /// never panic (and never read past a refusal).
    #[test]
    fn mangled_requests_never_panic() {
        let good = b"PUT /v1/apps/acme/shop/secrets/pg-url HTTP/1.1\r\nauthorization: Bearer combd_abc\r\ncontent-length: 12\r\n\r\nhello world!";
        let mut rng = crate::sim::Rng::new(19);
        for i in 0..100_000 {
            let b = crate::sim::mangle(&mut rng, good);
            let refuse = i % 2 == 0;
            let r = std::panic::catch_unwind(|| read_request(&mut &b[..], |_, _, _| if refuse { Err((403, json!({}))) } else { Ok(()) })).expect("the request reader panicked");
            if let Ok(Ok((_, _, body))) = r {
                assert!(body.len() <= crate::deploy::MAX_BUNDLE);
            }
        }
    }

    #[test]
    fn tokens_and_scopes() {
        let (t, line) = new_token(&["acme/shop:deploy", "acme/*:vars"]);
        let (admin, admin_line) = new_token(&["*/*:*"]);
        let tokens = parse_tokens(&format!("# a comment\n{line}\n{admin_line}\n")).unwrap();
        assert!(!line.contains(&t), "the file holds hashes, not tokens");
        assert!(allowed(&tokens, &t, "acme", "shop", "deploy"));
        assert!(allowed(&tokens, &t, "acme", "blog", "vars"));
        assert!(!allowed(&tokens, &t, "acme", "blog", "deploy"));
        assert!(!allowed(&tokens, &t, "globex", "shop", "deploy"));
        assert!(!allowed(&tokens, &t, "acme", "shop", "read"));
        assert!(allowed(&tokens, &admin, "globex", "x", "read"));
        assert!(!allowed(&tokens, "combd_guess", "acme", "shop", "deploy"));
        assert!(parse_tokens("abc acme/shop:deploy").is_err());
        assert!(parse_tokens(&format!("{} acme", "00".repeat(32))).is_err());
    }
}
