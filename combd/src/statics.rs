//! An app's static files: its `static/` folder, served as they are. A
//! request no route matches (GET or HEAD) gets the file at its path, or the
//! folder's `index.html`; then a `404.html` at the top, if there is one.
//! Each file has an ETag (a browser's copy still good: 304), and a `.br` or
//! `.gz` beside it is sent instead to a browser that takes it. They're held
//! in memory, so the event loop never waits on a disk.

use std::collections::HashMap;
use std::path::Path;

pub struct File {
    pub body: Vec<u8>,
    pub br: Option<Vec<u8>>,
    pub gz: Option<Vec<u8>>,
    /// The SHA-256 of `body`, its first 16 bytes in hex.
    pub hash: String,
    pub ctype: &'static str,
}

pub struct Static {
    files: HashMap<String, File>,
}

/// What a path finds.
pub enum Found<'a> {
    File(&'a File),
    /// A folder asked for without its `/`: there, so its page's links work.
    Redirect(String),
    /// Nothing: the `404.html`, if there is one.
    Missing(Option<&'a File>),
}

/// A file's name in a bundle: `static/` and plain segments (letters, digits,
/// `.`, `-`, `_`; none starts with `.`), so none climbs out of its folder.
pub fn name_ok(name: &str) -> bool {
    name.strip_prefix("static/").is_some_and(|p| {
        p.len() <= 255 && p.split('/').count() <= 16 && p.split('/').all(|s| !s.is_empty() && !s.starts_with('.') && s.bytes().all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c)))
    })
}

/// Every file under `dir`, as `(path from dir, bytes)`.
pub fn walk(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = vec![];
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
            let p = e.map_err(|e| e.to_string())?.path();
            if p.is_dir() {
                todo.push(p);
            } else {
                let rel = p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
                out.push((rel, std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?));
            }
        }
    }
    out.sort();
    Ok(out)
}

impl Static {
    /// The app's `static/` folder, if it has one.
    pub fn load(app_dir: &Path) -> Result<Option<Static>, String> {
        let dir = app_dir.join("static");
        if !dir.is_dir() {
            return Ok(None);
        }
        let mut all: HashMap<String, Vec<u8>> = walk(&dir)?.into_iter().collect();
        let names: Vec<String> = all.keys().filter(|n| !((n.ends_with(".br") || n.ends_with(".gz")) && all.contains_key(&n[..n.len() - 3]))).cloned().collect();
        let mut files = HashMap::new();
        for n in names {
            let body = all.remove(&n).unwrap();
            let hash = ring::digest::digest(&ring::digest::SHA256, &body).as_ref()[..16].iter().map(|b| format!("{b:02x}")).collect();
            let (br, gz) = (all.remove(&format!("{n}.br")), all.remove(&format!("{n}.gz")));
            files.insert(n.clone(), File { body, br, gz, hash, ctype: ctype(&n) });
        }
        Ok(Some(Static { files }))
    }

    /// The file for a request's path (no query string).
    pub fn find(&self, path: &str) -> Found<'_> {
        let p = path.trim_start_matches('/');
        let index = if p.is_empty() || p.ends_with('/') { format!("{p}index.html") } else { p.to_string() };
        if let Some(f) = self.files.get(&index) {
            return Found::File(f);
        }
        if !p.is_empty() && !p.ends_with('/') && self.files.contains_key(&format!("{p}/index.html")) {
            return Found::Redirect(format!("/{p}/"));
        }
        Found::Missing(self.files.get("404.html"))
    }
}

impl File {
    /// What to send a browser that takes `accept` (`accept-encoding`):
    /// (the bytes, `content-encoding`, the ETag).
    pub fn pick(&self, accept: &str) -> (&[u8], Option<&'static str>, String) {
        let takes = |e: &str| accept.split(',').any(|x| x.split(';').next().unwrap_or("").trim() == e);
        match (&self.br, &self.gz) {
            (Some(b), _) if takes("br") => (b, Some("br"), format!("\"{}-br\"", self.hash)),
            (_, Some(g)) if takes("gzip") => (g, Some("gzip"), format!("\"{}-gz\"", self.hash)),
            _ => (&self.body, None, format!("\"{}\"", self.hash)),
        }
    }

    /// Is there more than one way to send it (so caches keep them apart)?
    pub fn varies(&self) -> bool {
        self.br.is_some() || self.gz.is_some()
    }
}

/// A file's `content-type`, by its extension.
pub fn ctype(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        Some("pdf") => "application/pdf",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths find files, folders their index (or a redirect to it), and
    /// the rest the 404 page; a compressed copy goes only to a browser that
    /// takes it, under its own ETag; no bundle name climbs out of `static/`.
    #[test]
    fn a_path_finds_its_file_its_index_or_the_404_page() {
        let dir = crate::store::tempdir::Dir::new();
        let s = dir.0.join("static");
        std::fs::create_dir_all(s.join("guide")).unwrap();
        for (n, b) in [("index.html", "home"), ("guide/index.html", "guide"), ("app.js", "js"), ("app.js.br", "BR"), ("404.html", "nope")] {
            std::fs::write(s.join(n), b).unwrap();
        }
        let st = Static::load(&dir.0).unwrap().unwrap();
        let body = |p: &str| match st.find(p) {
            Found::File(f) => String::from_utf8(f.body.clone()).unwrap(),
            Found::Redirect(to) => format!("-> {to}"),
            Found::Missing(f) => format!("404 {}", f.map_or(String::new(), |f| String::from_utf8(f.body.clone()).unwrap())),
        };
        assert_eq!(body("/"), "home");
        assert_eq!(body("/guide/"), "guide");
        assert_eq!(body("/guide"), "-> /guide/");
        assert_eq!(body("/app.js"), "js");
        assert_eq!(body("/app.js.br"), "404 nope", "a compressed copy is the file's, not its own");
        assert_eq!(body("/../x"), "404 nope");
        let Found::File(js) = st.find("/app.js") else { panic!() };
        assert_eq!(js.ctype, "text/javascript; charset=utf-8");
        let (b, enc, tag) = js.pick("gzip, br;q=0.9");
        assert_eq!((b, enc), (&b"BR"[..], Some("br")));
        let (b, enc, plain) = js.pick("gzip");
        assert_eq!((b, enc), (&b"js"[..], None));
        assert_ne!(tag, plain);
        assert!(Static::load(&dir.0.join("nowhere")).unwrap().is_none());
        assert!(name_ok("static/a/b-c_d.html") && !name_ok("static/../x") && !name_ok("static/.env") && !name_ok("static//x") && !name_ok("modules/x"));
    }
}
