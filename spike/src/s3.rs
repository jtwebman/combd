//! A small S3 client: just what D2 needs. Get, put with conditions
//! (`If-None-Match: *`, `If-Match: <etag>`), list, and make a bucket, signed
//! with SigV4, over plain HTTP/1.1.
//!
//! ponytail: no TLS and one connection per call. Fine for a bucket on the
//! same network; real S3 needs TLS and a keep-alive pool.
//!
//! `check` is the startup storage test: before a node serves, it proves the
//! bucket really honors conditional writes, including when 16 writers race.
//! SeaweedFS 3.97 passes one-at-a-time checks and fails the race (2 to 5
//! "winners" of one CAS), which would give two owners of one object.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct S3 {
    /// `host:port`
    pub host: String,
    pub bucket: String,
    pub key: String,
    pub secret: String,
    pub region: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cond {
    Any,
    IfAbsent,
    IfMatch(String),
}

pub struct Resp {
    pub status: u16,
    pub etag: Option<String>,
    pub body: Vec<u8>,
}

type Io<T> = std::io::Result<T>;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

fn hmac(k: &[u8], m: &str) -> Vec<u8> {
    let mut h = Hmac::<Sha256>::new_from_slice(k).unwrap();
    h.update(m.as_bytes());
    h.finalize().into_bytes().to_vec()
}

/// RFC 3986 encoding, as SigV4 wants it. `/` stays in paths.
fn enc(s: &str, path: bool) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b'/' if path => o.push('/'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// `YYYYMMDD'T'HHMMSS'Z'` for now, without a date crate.
fn amz_date() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

impl S3 {
    pub fn from_env() -> Option<S3> {
        let url = std::env::var("COMBD_S3").ok()?;
        Some(S3 {
            host: url.trim_start_matches("http://").trim_end_matches('/').to_string(),
            bucket: std::env::var("COMBD_S3_BUCKET").unwrap_or_else(|_| "combd".into()),
            key: std::env::var("COMBD_S3_KEY").unwrap_or_default(),
            secret: std::env::var("COMBD_S3_SECRET").unwrap_or_default(),
            region: std::env::var("COMBD_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        })
    }

    /// One signed request. `path` is the object key (empty for the bucket).
    fn send(&self, method: &str, path: &str, query: &[(&str, &str)], extra: &[(&str, String)], body: &[u8]) -> Io<Resp> {
        let date = amz_date();
        let day = &date[..8];
        let uri = if path.is_empty() { format!("/{}", self.bucket) } else { format!("/{}/{}", self.bucket, enc(path, true)) };
        let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (enc(k, false), enc(v, false))).collect();
        q.sort();
        let qs: String = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let payload = sha(body);
        let mut hs: Vec<(String, String)> = vec![
            ("host".into(), self.host.clone()),
            ("x-amz-content-sha256".into(), payload.clone()),
            ("x-amz-date".into(), date.clone()),
        ];
        hs.extend(extra.iter().map(|(k, v)| (k.to_lowercase(), v.clone())));
        hs.sort();
        let canon_h: String = hs.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
        let signed: String = hs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
        let canon = format!("{method}\n{uri}\n{qs}\n{canon_h}\n{signed}\n{payload}");
        let scope = format!("{day}/{}/s3/aws4_request", self.region);
        let to_sign = format!("AWS4-HMAC-SHA256\n{date}\n{scope}\n{}", sha(canon.as_bytes()));
        let mut k = hmac(format!("AWS4{}", self.secret).as_bytes(), day);
        for part in [self.region.as_str(), "s3", "aws4_request"] {
            k = hmac(&k, part);
        }
        let sig = hex(&hmac(&k, &to_sign));
        let auth = format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}", self.key);

        let mut s = TcpStream::connect(&self.host)?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        s.set_nodelay(true)?;
        let target = if qs.is_empty() { uri } else { format!("{uri}?{qs}") };
        let mut req = format!("{method} {target} HTTP/1.1\r\nauthorization: {auth}\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
        for (k, v) in &hs {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes())?;
        s.write_all(body)?;
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line)?;
        let status = line.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| std::io::Error::other(format!("bad status line {line:?}")))?;
        let (mut etag, mut len, mut chunked) = (None, None, false);
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let Some((k, v)) = l.split_once(':') else { continue };
            match k.trim().to_ascii_lowercase().as_str() {
                "etag" => etag = Some(v.trim().to_string()),
                "content-length" => len = v.trim().parse().ok(),
                "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
                _ => {}
            }
        }
        let mut body = vec![];
        if method == "HEAD" {
        } else if chunked {
            loop {
                line.clear();
                r.read_line(&mut line)?;
                let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
                if n == 0 {
                    break;
                }
                let mut chunk = vec![0; n + 2];
                r.read_exact(&mut chunk)?;
                body.extend_from_slice(&chunk[..n]);
            }
        } else if let Some(n) = len {
            body.resize(n, 0);
            r.read_exact(&mut body)?;
        } else {
            r.read_to_end(&mut body)?;
        }
        Ok(Resp { status, etag, body })
    }

    fn fail(what: &str, r: &Resp) -> std::io::Error {
        std::io::Error::other(format!("{what}: HTTP {}: {}", r.status, String::from_utf8_lossy(&r.body[..r.body.len().min(300)])))
    }

    pub fn make_bucket(&self) -> Io<()> {
        let r = self.send("PUT", "", &[], &[], b"")?;
        // 409: it's already there.
        if r.status == 200 || r.status == 409 {
            Ok(())
        } else {
            Err(Self::fail("make bucket", &r))
        }
    }

    /// The object and its etag, or `None` if it isn't there.
    pub fn get(&self, key: &str) -> Io<Option<(Vec<u8>, String)>> {
        let r = self.send("GET", key, &[], &[], b"")?;
        match r.status {
            200 => Ok(Some((r.body, r.etag.unwrap_or_default()))),
            404 => Ok(None),
            _ => Err(Self::fail("get", &r)),
        }
    }

    /// The new etag, or `Err(())` if the condition failed.
    pub fn put(&self, key: &str, body: &[u8], cond: &Cond) -> Io<Result<String, ()>> {
        let extra: Vec<(&str, String)> = match cond {
            Cond::Any => vec![],
            Cond::IfAbsent => vec![("if-none-match", "*".into())],
            Cond::IfMatch(e) => vec![("if-match", e.clone())],
        };
        let r = self.send("PUT", key, &[], &extra, body)?;
        match r.status {
            200 => Ok(Ok(r.etag.unwrap_or_default())),
            412 => Ok(Err(())),
            _ => Err(Self::fail("put", &r)),
        }
    }

    /// Every key under `prefix`, in order.
    pub fn list(&self, prefix: &str) -> Io<Vec<String>> {
        let mut keys = vec![];
        let mut token = String::new();
        loop {
            let mut q = vec![("list-type", "2"), ("prefix", prefix)];
            if !token.is_empty() {
                q.push(("continuation-token", token.as_str()));
            }
            let r = self.send("GET", "", &q, &[], b"")?;
            if r.status != 200 {
                return Err(Self::fail("list", &r));
            }
            let x = String::from_utf8_lossy(&r.body);
            keys.extend(tags(&x, "Key").into_iter().map(|k| unxml(&k)));
            match tags(&x, "NextContinuationToken").pop() {
                Some(t) if x.contains("<IsTruncated>true</IsTruncated>") => token = unxml(&t),
                _ => return Ok(keys),
            }
        }
    }

    /// The startup storage test. A node that fails it must not serve.
    pub fn check(&self) -> Result<(), String> {
        let e = |x: std::io::Error| x.to_string();
        self.make_bucket().map_err(e)?;
        // Unique per run: several nodes check at once (and in containers
        // their process ids are all 1).
        let mut nonce = [0u8; 16];
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut nonce)).map_err(e)?;
        let nonce = hex(&nonce);
        let k = format!("_combd/check/{nonce}");
        let first = self.put(&k, b"one", &Cond::IfAbsent).map_err(e)?.map_err(|_| "a create-if-absent of a new key failed")?;
        if self.put(&k, b"two", &Cond::IfAbsent).map_err(e)?.is_ok() {
            return Err("If-None-Match: * overwrote an existing object".into());
        }
        if self.put(&k, b"three", &Cond::IfMatch("\"nope\"".into())).map_err(e)?.is_ok() {
            return Err("If-Match with a wrong etag succeeded".into());
        }
        let second = self.put(&k, b"four", &Cond::IfMatch(first.clone())).map_err(e)?.map_err(|_| "If-Match with the right etag failed")?;
        if self.get(&k).map_err(e)?.map(|(b, _)| b) != Some(b"four".to_vec()) {
            return Err("an object didn't read back as written".into());
        }
        // The race: exactly one of 16 writers may win, for each kind.
        let race = |cond: Cond, key: String| -> Result<usize, String> {
            let wins: Vec<bool> = std::thread::scope(|sc| {
                let hs: Vec<_> = (0..16)
                    .map(|i| {
                        let (cond, key) = (cond.clone(), key.clone());
                        sc.spawn(move || matches!(self.put(&key, format!("racer {i}").as_bytes(), &cond), Ok(Ok(_))))
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            Ok(wins.into_iter().filter(|w| *w).count())
        };
        let n = race(Cond::IfMatch(second), k.clone())?;
        if n != 1 {
            return Err(format!("{n} of 16 racing If-Match writers won (must be 1): CAS isn't atomic"));
        }
        let n = race(Cond::IfAbsent, format!("{k}-fresh"))?;
        if n != 1 {
            return Err(format!("{n} of 16 racing If-None-Match writers won (must be 1): create-if-absent isn't atomic"));
        }
        Ok(())
    }
}

fn tags(x: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = vec![];
    let mut rest = x;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].to_string());
        rest = &after[j + close.len()..];
    }
    out
}

fn unxml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#34;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real bucket: `COMBD_S3=http://localhost:9000 COMBD_S3_KEY=.. COMBD_S3_SECRET=..`.
    #[test]
    fn a_real_bucket_passes_the_storage_test() {
        let Some(s3) = S3::from_env() else {
            eprintln!("skipping: set COMBD_S3 to test against a real bucket");
            return;
        };
        s3.check().unwrap();
        for i in 0..1005 {
            s3.put(&format!("_combd/list/{i:05}"), b"x", &Cond::Any).unwrap().unwrap();
        }
        let keys = s3.list("_combd/list/").unwrap();
        assert_eq!(keys.len(), 1005, "listing pages past 1,000 keys");
        assert_eq!(keys[0], "_combd/list/00000");
    }

    #[test]
    fn dates_and_encoding() {
        let d = amz_date();
        assert_eq!(d.len(), 16);
        assert!(d.starts_with("20") && d.ends_with('Z'));
        assert_eq!(enc("obj/1/e2/s 000001", true), "obj/1/e2/s%20000001");
        assert_eq!(enc("a/b", false), "a%2Fb");
    }
}
