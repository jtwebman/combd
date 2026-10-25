//! `combd new`, `combd build`, `combd dev`: an app from nothing to running
//! here, the way the quickstart (`docs/quickstart.md`) goes.

use std::path::Path;

/// An app's name: it names its package, its module, and its folder.
fn ident(s: &str) -> bool {
    !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

const MANIFEST: &str = r#"# A combd app: its processes, and the routes that reach them.
[app]
name = "NAME"
# Locally, browsers (and curl) send NAME.localhost to 127.0.0.1.
domains = ["NAME.localhost"]

# A stateless process: the page.
[http.web]
module = "NAME"
routes = ["/"]

# A durable object per id: a counter, with its own SQLite database.
[durable.counter]
module = "NAME"
routes = ["/counter/:id/*"]
"#;

const CARGO: &str = r#"[package]
name = "NAME"
version = "0.1.0"
edition = "2021"
publish = false

# Its own workspace: it builds for wasm32 on its own.
[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
combd-sdk = { git = "https://github.com/jtwebman/combd" }

[profile.release]
opt-level = "s"
lto = true
panic = "abort"
strip = true
"#;

const LIB: &str = r##"//! NAME: a page (`web`, stateless) and a counter (`counter`, a durable
//! object: one per id, each with its own SQLite). See combd.toml.

use combd_sdk::{cell, json, sql, Request, Response};

const PAGE: &str = r#"<!doctype html>
<title>NAME</title>
<h1>NAME</h1>
<p>Clicked <b id="n">?</b> times. <button id="b">Click</button></p>
<script>
const show = r => r.json().then(v => n.textContent = v.count);
fetch("/counter/demo/").then(show);
b.onclick = () => fetch("/counter/demo/incr", { method: "POST" }).then(show);
</script>
"#;

fn handle(req: Request) -> Response {
    match (req.kind, req.method, req.route()) {
        ("api", "GET", "/") => Response::html(PAGE),
        ("do", "POST", "/incr") => {
            sql::run("CREATE TABLE IF NOT EXISTS counter (n INTEGER NOT NULL)", &[]).unwrap();
            sql::run("INSERT INTO counter (rowid, n) VALUES (1, 1) ON CONFLICT (rowid) DO UPDATE SET n = n + 1", &[]).unwrap();
            count()
        }
        ("do", "GET", "" | "/") => count(),
        _ => Response::text(404, "not found\n"),
    }
}

fn count() -> Response {
    let n = sql::run("SELECT n FROM counter", &[]).ok().and_then(|rows| rows.first()?.first().cloned()).unwrap_or(json!(0));
    Response::json(200, &json!({ "count": n }))
}

cell!(handle);
"##;

/// `combd new <name>`: a folder with an app that runs.
pub fn new(name: &str) -> Result<(), String> {
    if !ident(name) {
        return Err(format!("{name:?}: an app's name is lowercase letters, digits, - and _"));
    }
    let dir = Path::new(name);
    if dir.exists() {
        return Err(format!("{name} is already there"));
    }
    let files = [
        ("combd.toml", MANIFEST.replace("NAME", name)),
        ("Cargo.toml", CARGO.replace("NAME", name)),
        ("src/lib.rs", LIB.replace("NAME", name)),
        (".gitignore", "/target\n/modules\n/data\n".into()),
    ];
    for (path, text) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| format!("{}: {e}", p.display()))?;
        std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    println!("made {name}/. Next:\n  cd {name}\n  combd dev\nthen open http://{name}.localhost:8080");
    Ok(())
}

/// `combd build [dir]`: its crate, for wasm32, into `modules/<package>.wasm`
/// (where `module = "<package>"` in combd.toml finds it).
pub fn build(dir: &Path) -> Result<(), String> {
    let cargo = std::fs::read_to_string(dir.join("Cargo.toml")).map_err(|e| format!("{}: {e}", dir.join("Cargo.toml").display()))?;
    let pkg: toml::Table = toml::from_str(&cargo).map_err(|e| format!("Cargo.toml: {e}"))?;
    let name = pkg.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()).ok_or("Cargo.toml: no package name")?.to_string();
    let status = std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(dir)
        .status()
        .map_err(|e| format!("cargo: {e} (combd builds Rust apps with cargo: https://rustup.rs)"))?;
    if !status.success() {
        return Err("cargo build failed (is the target there? rustup target add wasm32-unknown-unknown)".into());
    }
    let wasm = dir.join(format!("target/wasm32-unknown-unknown/release/{}.wasm", name.replace('-', "_")));
    std::fs::create_dir_all(dir.join("modules")).map_err(|e| e.to_string())?;
    let to = dir.join(format!("modules/{name}.wasm"));
    std::fs::copy(&wasm, &to).map_err(|e| format!("{}: {e}", wasm.display()))?;
    println!("built {}", to.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    /// `combd new` makes an app whose manifest reads, and whose source is
    /// the template with its name in (the raw strings intact).
    #[test]
    fn a_new_app_is_whole() {
        let dir = crate::store::tempdir::Dir::new();
        let here = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir.0).unwrap();
        let r = super::new("hello-app");
        std::env::set_current_dir(here).unwrap();
        r.unwrap();
        let app = dir.0.join("hello-app");
        let lib = std::fs::read_to_string(app.join("src/lib.rs")).unwrap();
        assert!(lib.contains("<title>hello-app</title>") && lib.contains("const PAGE: &str = r#\"<!doctype html>") && lib.contains("</script>\n\"#;"), "{lib}");
        // Its module where `combd build` puts it.
        std::fs::create_dir(app.join("modules")).unwrap();
        std::fs::write(app.join("modules/hello-app.wasm"), b"").unwrap();
        let m = crate::manifest::load(&app).unwrap().unwrap();
        assert!(m.processes.contains_key("web") && m.processes.contains_key("counter"));
        assert!(super::new("Bad Name").is_err());
    }
}
