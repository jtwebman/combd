//! Durable Object storage (D1, single node): one SQLite file per object, in
//! the host. The cell sends SQL as JSON and gets rows back as JSON.
//!
//! Each request is one transaction: the host opens it before the request and
//! commits it after. If the cell crashes, the transaction rolls back, so a
//! crash never leaves half a request behind.
//!
//! Durability is a group commit. The object's own connection never syncs
//! (WAL mode, synchronous=OFF), so a commit is just a write. The host fsyncs
//! the WAL file separately, off the event loop,
//! and holds each answer until an fsync has covered its commit (the output
//! gate). One fsync covers every commit made before it started, so a busy
//! object pays one fsync for many requests, and many objects' fsyncs run at
//! once. A client that saw an answer can trust its writes are on disk.
//!
//! Checkpoints (copying the WAL back into the database file) are off the
//! event loop too: SQLite's automatic ones are turned off, and the fsync
//! threads run a passive checkpoint (it never blocks the writer) once the WAL
//! holds `CHECKPOINT_PAGES` pages. The WAL file is never truncated: a file
//! that keeps its size fsyncs much faster than one that grows.
//!
//! SQLite runs in the host, so the cell's SQL is fenced in:
//! - an authorizer denies ATTACH (it could open other files), PRAGMA, the
//!   cell's own BEGIN / COMMIT / SAVEPOINT (the host owns transactions), and
//!   any table named `_combd...` (the host's own, like the alarm),
//! - limits cap the size of values (2 MB, as Cloudflare's), SQL text, and
//!   expression depth, and each object's file (`MAX_DB_BYTES`),
//! - a progress handler stops any one statement after `MAX_STEPS` or
//!   `MAX_STATEMENT`, since SQLite's CPU use isn't counted by the cell's fuel.
//!   Steps alone aren't enough: one step can build a 2 MB string.

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value, ValueRef};
use rusqlite::Connection;
use serde_json::{json, Value as Json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Arc;

/// Progress callbacks (every 1,000 SQLite VM steps) before a statement is
/// stopped: about 10 million steps, tens of milliseconds.
const MAX_STEPS: u32 = 100_000;
/// How long one of a cell's statements may run.
const MAX_STATEMENT: std::time::Duration = std::time::Duration::from_millis(100);
/// The biggest value (string or blob) a cell can store or build.
pub const MAX_VALUE: usize = 2 << 20;
/// The biggest an object's database may grow. ponytail: one cap for every
/// object; per-tenant quotas come with multi-tenancy.
pub const MAX_DB_BYTES: i64 = 1 << 30;
/// A WAL holding this many pages gets checkpointed, by the fsync threads
/// (SQLite's own default is the same).
pub const CHECKPOINT_PAGES: i32 = 1000;
/// The biggest answer a cell can get back, and the biggest SQL it can send.
pub const MAX_RESULT: usize = 16 << 20;

pub struct Db {
    conn: Connection,
    path: PathBuf,
    wal_path: PathBuf,
    /// The WAL file, opened at the first fsync. SQLite keeps it (same inode)
    /// for as long as this connection is open.
    wal: std::cell::OnceCell<Arc<std::fs::File>>,
    /// The open transaction changed something.
    wrote: std::cell::Cell<bool>,
    /// Pages in the WAL after the last commit.
    wal_pages: std::cell::Cell<i32>,
    /// D2: where the next unsealed WAL frame starts, and the WAL's salt.
    tail: std::cell::Cell<(u64, u64)>,
    /// True while the host itself runs SQL (BEGIN, COMMIT).
    host: Arc<AtomicBool>,
    steps: Arc<AtomicU32>,
    started: Arc<std::sync::Mutex<std::time::Instant>>,
}

/// Where an object's file lives. Names come from users, so the file name is
/// a hash of the name (SHA-256): no path tricks, no collisions, and no length
/// limit. An app's objects (`<org>/<app>/...`) live in its own folder,
/// `<org>/<app>/`, so an app's data can be found, sized, and deleted.
pub fn path_for(dir: &Path, name: &str) -> PathBuf {
    use sha2::Digest;
    let hex: String = sha2::Sha256::digest(name.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let ident = |s: &str| !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    let mut parts = name.splitn(4, '/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(org), Some(app), Some(_), Some(_)) if ident(org) && ident(app) => dir.join(org).join(app).join(format!("{hex}.sqlite")),
        _ => dir.join(format!("{hex}.sqlite")),
    }
}

/// The folders between `dir` and an object's file that don't exist yet:
/// made now, and fsynced (with their parents) once the object first commits,
/// or a power cut could lose the new file.
pub fn make_dirs(dir: &Path, file: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut new = vec![];
    let mut p = file.parent();
    while let Some(d) = p.filter(|d| d.starts_with(dir) && !d.exists()) {
        new.push(d.to_path_buf());
        p = d.parent();
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Each new folder, deepest first, then the one that held the topmost
    // (with no new folders: the one the new file is in).
    let top = new.last().and_then(|d| d.parent()).or(file.parent()).unwrap_or(dir).to_path_buf();
    new.push(top);
    Ok(new)
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        // This connection never syncs, not even the WAL header after a
        // restart (that was an fsync inside commits, on the event loop). It's
        // safe because every sync happens elsewhere, in the right order:
        // - commits: the fsync threads sync the whole WAL file, header
        //   included, before any answer leaves,
        // - checkpoints: `checkpoint` uses its own connection at NORMAL, which
        //   syncs the WAL before writing the database file, and that file after,
        // - close: `Drop` switches back to NORMAL first, because SQLite
        //   checkpoints when the last connection closes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "OFF")?;
        // Checkpoints happen on the fsync threads (see `checkpoint`).
        conn.pragma_update(None, "wal_autocheckpoint", 0)?;
        // Count the WAL's pages after each commit, to know when to checkpoint.
        conn.wal_hook(Some(note_wal_pages));
        // No busy timeout: a passive checkpoint never takes the write lock,
        // and when it holds a read lock the writer just skips restarting the
        // WAL. A timeout here would sleep on the event loop instead.
        conn.pragma_update(None, "trusted_schema", "OFF")?;
        // Many objects per node: keep each one's page cache small (256 KiB).
        conn.pragma_update(None, "cache_size", -256)?;
        // Cap the object's file (per connection, so set at every open).
        let page: i64 = conn.pragma_query_value(None, "page_size", |r| r.get(0))?;
        conn.pragma_update(None, "max_page_count", MAX_DB_BYTES / page)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS _combd_alarm (id INTEGER PRIMARY KEY CHECK (id = 1), at INTEGER NOT NULL)")?;
        // The object's WebSockets, wherever their connections are. Kept here,
        // in the request's transaction, so they move with the object (D2, D3).
        conn.execute_batch("CREATE TABLE IF NOT EXISTS _combd_sockets (id INTEGER PRIMARY KEY)")?;
        for (limit, v) in [
            (Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE as i32),
            (Limit::SQLITE_LIMIT_SQL_LENGTH, 1 << 20),
            (Limit::SQLITE_LIMIT_EXPR_DEPTH, 100),
            (Limit::SQLITE_LIMIT_ATTACHED, 0),
            (Limit::SQLITE_LIMIT_TRIGGER_DEPTH, 10),
        ] {
            conn.set_limit(limit, v)?;
        }
        let host = Arc::new(AtomicBool::new(false));
        let h = host.clone();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if h.load(Relaxed) {
                return Authorization::Allow;
            }
            if table_of(&ctx.action).is_some_and(|t| t.len() >= 6 && t[..6].eq_ignore_ascii_case("_combd")) {
                return Authorization::Deny;
            }
            match ctx.action {
                AuthAction::Attach { .. }
                | AuthAction::Detach { .. }
                | AuthAction::Pragma { .. }
                | AuthAction::Transaction { .. }
                | AuthAction::Savepoint { .. }
                | AuthAction::CreateVtable { .. }
                | AuthAction::DropVtable { .. }
                | AuthAction::Unknown { .. } => Authorization::Deny,
                // fts3_tokenizer with two arguments can run native code.
                AuthAction::Function { function_name } if function_name.eq_ignore_ascii_case("fts3_tokenizer") => {
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            }
        }));
        let steps = Arc::new(AtomicU32::new(0));
        let started = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let (s, t, h) = (steps.clone(), started.clone(), host.clone());
        // Every 100 VM steps (a step can be costly), for cell statements only.
        conn.progress_handler(100, Some(move || {
            !h.load(Relaxed) && (s.fetch_add(1, Relaxed) >= MAX_STEPS || t.lock().unwrap().elapsed() > MAX_STATEMENT)
        }));
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        Ok(Db { conn, path: path.to_path_buf(), wal_path: wal_path.into(), wal: Default::default(), wrote: Default::default(), wal_pages: Default::default(), tail: std::cell::Cell::new((WAL_HEADER, 0)), host, steps, started })
    }

    fn host_sql(&self, sql: &str) -> rusqlite::Result<()> {
        self.host.store(true, Relaxed);
        let r = self.conn.execute_batch(sql);
        self.host.store(false, Relaxed);
        r
    }

    pub fn begin(&self) -> rusqlite::Result<()> {
        self.wrote.set(false);
        self.host_sql("BEGIN IMMEDIATE")
    }

    /// Commit (no fsync). True if the transaction wrote anything, so the
    /// answer must wait for an fsync.
    pub fn commit(&self) -> rusqlite::Result<bool> {
        self.host_sql("COMMIT")?;
        self.wal_pages.set(WAL_PAGES.with(|p| p.get()));
        Ok(self.wrote.replace(false))
    }

    /// The WAL is big enough to checkpoint.
    pub fn wants_checkpoint(&self) -> bool {
        self.wal_pages.get() >= CHECKPOINT_PAGES
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file to fsync to make commits durable.
    pub fn wal_file(&self) -> std::io::Result<Arc<std::fs::File>> {
        if let Some(f) = self.wal.get() {
            return Ok(f.clone());
        }
        let f = Arc::new(std::fs::File::open(&self.wal_path)?);
        Ok(self.wal.get_or_init(|| f).clone())
    }

    /// The object's alarm, in ms since the epoch.
    pub fn alarm(&self) -> Option<i64> {
        self.host.store(true, Relaxed);
        let r = self.conn.query_row("SELECT at FROM _combd_alarm", [], |r| r.get(0)).ok();
        self.host.store(false, Relaxed);
        r
    }

    /// Set or clear the alarm, inside the open transaction.
    pub fn set_alarm(&self, at: Option<i64>) -> rusqlite::Result<()> {
        self.wrote.set(true);
        self.host.store(true, Relaxed);
        let r = match at {
            Some(at) => self.conn.execute("INSERT OR REPLACE INTO _combd_alarm (id, at) VALUES (1, ?)", [at]),
            None => self.conn.execute("DELETE FROM _combd_alarm", []),
        };
        self.host.store(false, Relaxed);
        r.map(|_| ())
    }

    /// The object's WebSocket ids.
    pub fn sockets(&self) -> Vec<u64> {
        self.host.store(true, Relaxed);
        let r = self.conn.prepare_cached("SELECT id FROM _combd_sockets").and_then(|mut s| s.query_map([], |r| r.get::<_, i64>(0)).and_then(|m| m.collect::<Result<Vec<_>, _>>()));
        self.host.store(false, Relaxed);
        r.unwrap_or_default().into_iter().map(|i| i as u64).collect()
    }

    /// Add one socket and drop some, inside the open transaction.
    pub fn update_sockets(&self, add: Option<u64>, drop: &[u64]) -> rusqlite::Result<()> {
        self.host.store(true, Relaxed);
        let r = (|| {
            if let Some(id) = add {
                self.conn.execute("INSERT OR IGNORE INTO _combd_sockets (id) VALUES (?)", [id as i64])?;
                self.wrote.set(true);
            }
            for &id in drop {
                if self.conn.execute("DELETE FROM _combd_sockets WHERE id = ?", [id as i64])? > 0 {
                    self.wrote.set(true);
                }
            }
            Ok(())
        })();
        self.host.store(false, Relaxed);
        r
    }

    /// The host's transaction is open (a cell can't end it).
    pub fn in_transaction(&self) -> bool {
        !self.conn.is_autocommit()
    }

    pub fn rollback(&self) {
        self.wrote.set(false);
        if !self.conn.is_autocommit() {
            let _ = self.host_sql("ROLLBACK");
        }
    }

    /// One statement from the cell: `{"q": sql, "p": [params]}` in,
    /// `{"rows": [[...]], "changes": n}` or `{"error": "..."}` out.
    pub fn exec_json(&self, req: &[u8]) -> Vec<u8> {
        let out = match self.exec(req) {
            Ok(v) => v,
            Err(e) => json!({ "error": e }),
        };
        let bytes = serde_json::to_vec(&out).unwrap_or_default();
        if bytes.len() > MAX_RESULT {
            return br#"{"error":"result too large"}"#.to_vec();
        }
        bytes
    }

    fn exec(&self, req: &[u8]) -> Result<Json, String> {
        if req.len() > MAX_RESULT {
            return Err("request too large".into());
        }
        let req: Json = serde_json::from_slice(req).map_err(|e| format!("bad request: {e}"))?;
        let sql = req["q"].as_str().ok_or("missing q")?;
        let params: Vec<Value> = match &req["p"] {
            Json::Null => vec![],
            Json::Array(a) => a.iter().map(to_sql).collect::<Result<_, _>>()?,
            _ => return Err("p must be an array".into()),
        };
        self.steps.store(0, Relaxed);
        *self.started.lock().unwrap() = std::time::Instant::now();
        let mut st = self.conn.prepare_cached(sql).map_err(|e| e.to_string())?;
        if !st.readonly() {
            self.wrote.set(true);
        }
        let cols = st.column_count();
        let mut rows = st.query(rusqlite::params_from_iter(params)).map_err(|e| e.to_string())?;
        let mut out = vec![];
        // Count the answer as it grows, and stop at MAX_RESULT: checking only
        // at the end let one query hold gigabytes in the host first.
        let mut size = 0usize;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut r = Vec::with_capacity(cols);
            for i in 0..cols {
                let v = row.get_ref(i).map_err(|e| e.to_string())?;
                size += match v {
                    ValueRef::Text(t) | ValueRef::Blob(t) => t.len() + 8,
                    _ => 24,
                };
                if size > MAX_RESULT {
                    return Err("result too large".into());
                }
                r.push(match v {
                    ValueRef::Null => Json::Null,
                    ValueRef::Integer(n) => json!(n),
                    ValueRef::Real(x) => json!(x),
                    ValueRef::Text(t) => Json::String(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(_) => return Err("blobs aren't supported yet".into()),
                });
            }
            out.push(Json::Array(r));
        }
        drop(rows);
        Ok(json!({ "rows": out, "changes": self.conn.changes() }))
    }
}

thread_local! {
    /// Set by SQLite's WAL hook during a commit, on the committing thread.
    static WAL_PAGES: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

fn note_wal_pages(_: &rusqlite::hooks::Wal, pages: std::ffi::c_int) -> rusqlite::Result<()> {
    WAL_PAGES.with(|p| p.set(pages));
    Ok(())
}

/// Copy an object's WAL back into its database file. Runs on an fsync thread
/// with its own connection. Passive: it copies what it can without blocking
/// the object's writer, which then restarts the WAL from the top.
const WAL_HEADER: u64 = 32;
const FRAME_HEADER: u64 = 24;

impl Db {
    /// D2: every page changed since the last seal, from the WAL's committed
    /// frames, newest copy of each. Format: page size, database size in
    /// pages, count, then (page number, page) pairs, all u32 little-endian.
    ///
    /// Only call between requests, and checkpoint (with `checkpoint_local`)
    /// only right after a seal, so no change is ever skipped.
    pub fn seal(&self) -> std::io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = match std::fs::File::open(&self.wal_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(encode_pages(4096, 0, &BTreeMap::new())),
            Err(e) => return Err(e),
        };
        let mut hdr = [0u8; WAL_HEADER as usize];
        if f.read_exact(&mut hdr).is_err() {
            return Ok(encode_pages(4096, 0, &BTreeMap::new())); // empty: truncated by a checkpoint
        }
        let be = |b: &[u8]| u32::from_be_bytes(b.try_into().unwrap());
        let page = be(&hdr[8..12]) as u64;
        let salt = u64::from_be_bytes(hdr[16..24].try_into().unwrap());
        let (mut pos, old_salt) = self.tail.get();
        if salt != old_salt {
            pos = WAL_HEADER; // a new WAL, after a checkpoint: start from the top
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut pages = BTreeMap::new();
        let mut pending: Vec<(u32, Vec<u8>)> = vec![];
        let mut size = 0u32;
        let mut frame = vec![0u8; (FRAME_HEADER + page) as usize];
        let mut at = pos;
        loop {
            if f.read_exact(&mut frame).is_err() {
                break;
            }
            // Frames left over from before the WAL restarted have the old salt.
            if u64::from_be_bytes(frame[8..16].try_into().unwrap()) != salt {
                break;
            }
            let pgno = be(&frame[0..4]);
            let commit = be(&frame[4..8]);
            pending.push((pgno, frame[FRAME_HEADER as usize..].to_vec()));
            at += FRAME_HEADER + page;
            if commit > 0 {
                // A commit frame: everything up to here is a whole transaction.
                for (n, p) in pending.drain(..) {
                    pages.insert(n, p);
                }
                size = commit;
                pos = at;
            }
        }
        self.tail.set((pos, salt));
        Ok(encode_pages(page as u32, size, &pages))
    }

    /// D2: copy the WAL into the database file and empty it. No fsync: in D2
    /// the local file is a cache, rebuilt from the bucket after a restart.
    /// Only right after a `seal`.
    pub fn checkpoint_local(&self) -> rusqlite::Result<()> {
        self.host.store(true, Relaxed);
        let r = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        self.host.store(false, Relaxed);
        self.tail.set((WAL_HEADER, 0));
        r
    }
}

fn encode_pages(page: u32, size: u32, pages: &BTreeMap<u32, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + pages.len() * (4 + page as usize));
    for v in [page, size, pages.len() as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for (n, p) in pages {
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(p);
    }
    out
}

/// The database file as one entry: every page and its size, the way `seal`
/// writes one. Restored on its own (`restore`), it is the whole object.
/// `None` for an empty one.
pub fn image(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let file = std::fs::read(path)?;
    // The page size, from the header (1 means 65536).
    let page = match file.get(16..18).map(|b| u16::from_be_bytes([b[0], b[1]])) {
        Some(1) => 65536usize,
        Some(n) if (512..=32768).contains(&n) && n.is_power_of_two() => n as usize,
        _ => return Ok(None),
    };
    if file.len() % page != 0 {
        return Err(std::io::Error::other("database isn't whole pages"));
    }
    let n = file.len() / page;
    let mut out = Vec::with_capacity(12 + n * (4 + page));
    for v in [page as u32, n as u32, n as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for (i, p) in file.chunks(page).enumerate() {
        out.extend_from_slice(&(i as u32 + 1).to_le_bytes());
        out.extend_from_slice(p);
    }
    Ok(Some(out))
}

/// D2: build a database file from sealed segments, in order, from nothing.
pub fn restore(path: &Path, segs: &[Vec<u8>]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let bad = || std::io::Error::other("bad segment");
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    for p in [path.as_os_str(), &wal, &shm] {
        let _ = std::fs::remove_file(p);
    }
    let mut f = std::fs::File::create(path)?;
    let mut size = 0u64;
    let mut page_size = 4096u64;
    for seg in segs {
        let u = |i: usize| -> std::io::Result<u32> { Ok(u32::from_le_bytes(seg.get(i..i + 4).ok_or_else(bad)?.try_into().unwrap())) };
        let (page, db, n) = (u(0)? as u64, u(4)? as u64, u(8)? as usize);
        if n == 0 {
            continue;
        }
        // What SQLite allows, and our cap: a damaged segment is refused,
        // never written past the database it describes.
        if !(512..=65536).contains(&page) || !page.is_power_of_two() || db * page > MAX_DB_BYTES as u64 {
            return Err(bad());
        }
        page_size = page;
        let mut at = 12;
        for _ in 0..n {
            let pgno = u(at)? as u64;
            if !(1..=db).contains(&pgno) {
                return Err(bad());
            }
            let body = seg.get(at + 4..at + 4 + page as usize).ok_or_else(bad)?;
            f.seek(SeekFrom::Start((pgno - 1) * page))?;
            f.write_all(body)?;
            at += 4 + page as usize;
        }
        size = db;
    }
    f.set_len(size * page_size)?;
    Ok(())
}

pub fn checkpoint(path: &Path) -> rusqlite::Result<()> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
}

/// Which objects have alarms, so a restarted node finds them without opening
/// every object. Each object's own storage is the truth; this is a hint. It's
/// written before the object commits, so it can hold an extra entry (checked
/// and skipped when it fires) but never miss one.
pub struct AlarmIndex(Connection);

impl AlarmIndex {
    pub fn open(dir: &Path) -> rusqlite::Result<AlarmIndex> {
        let conn = Connection::open(dir.join("alarms.sqlite"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS alarms (name TEXT PRIMARY KEY, at INTEGER NOT NULL)")?;
        Ok(AlarmIndex(conn))
    }

    pub fn set(&self, name: &str, at: Option<i64>) -> rusqlite::Result<()> {
        match at {
            Some(at) => self.0.execute("INSERT OR REPLACE INTO alarms (name, at) VALUES (?, ?)", rusqlite::params![name, at]),
            None => self.0.execute("DELETE FROM alarms WHERE name = ?", [name]),
        }
        .map(|_| ())
    }

    pub fn all(&self) -> rusqlite::Result<Vec<(String, i64)>> {
        let mut st = self.0.prepare("SELECT name, at FROM alarms")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        self.host.store(true, Relaxed);
        let _ = self.conn.pragma_update(None, "synchronous", "NORMAL");
    }
}

fn table_of<'a>(a: &AuthAction<'a>) -> Option<&'a str> {
    use AuthAction::*;
    Some(match *a {
        CreateIndex { table_name, .. }
        | CreateTable { table_name }
        | CreateTempIndex { table_name, .. }
        | CreateTempTable { table_name }
        | CreateTempTrigger { table_name, .. }
        | CreateTrigger { table_name, .. }
        | Delete { table_name }
        | DropIndex { table_name, .. }
        | DropTable { table_name }
        | DropTempIndex { table_name, .. }
        | DropTempTable { table_name }
        | DropTempTrigger { table_name, .. }
        | DropTrigger { table_name, .. }
        | Insert { table_name }
        | Read { table_name, .. }
        | Update { table_name, .. }
        | AlterTable { table_name, .. }
        | Analyze { table_name }
        | CreateVtable { table_name, .. }
        | DropVtable { table_name, .. } => table_name,
        _ => return None,
    })
}

fn to_sql(v: &Json) -> Result<Value, String> {
    Ok(match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Integer(*b as i64),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i),
            None => Value::Real(n.as_f64().ok_or("bad number")?),
        },
        Json::String(s) => Value::Text(s.clone()),
        _ => return Err("params must be null, booleans, numbers, or strings".into()),
    })
}

#[cfg(test)]
mod tests {
    /// Segments come from the bucket: damaged at random, a restore refuses
    /// them or writes a file no bigger than they say, and never panics.
    #[test]
    fn mangled_segments_never_panic_or_grow_past_their_size() {
        let page = 512u32;
        let mut seg = vec![];
        for v in [page, 3, 2] {
            seg.extend_from_slice(&v.to_le_bytes());
        }
        for pgno in [1u32, 3] {
            seg.extend_from_slice(&pgno.to_le_bytes());
            seg.extend(std::iter::repeat_n(pgno as u8, page as usize));
        }
        let d = super::tempdir::Dir::new();
        let path = d.0.join("db");
        restore(&path, &[seg.clone()]).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 3 * 512);
        // What random damage rarely hits, by hand: page 0, a page past the
        // end, a page size SQLite can't have, a database past the cap.
        let with = |at: usize, v: u32| {
            let mut b = seg.clone();
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
            b
        };
        for (at, v, what) in [(12, 0, "page 0"), (12, 4, "a page past the end"), (0, 1000, "page size 1000"), (4, u32::MAX, "past the cap")] {
            assert!(restore(&path, &[with(at, v)]).is_err(), "{what}");
        }
        let mut rng = crate::sim::Rng::new(29);
        for _ in 0..1_500 {
            let b = crate::sim::mangle(&mut rng, &seg);
            let r = std::panic::catch_unwind(|| restore(&path, &[b])).expect("restore panicked");
            if r.is_ok() {
                assert!(std::fs::metadata(&path).unwrap().len() <= MAX_DB_BYTES as u64);
            }
        }
    }

    use super::*;

    fn db() -> (tempdir::Dir, Db) {
        let d = tempdir::Dir::new();
        let db = Db::open(&path_for(&d.0, "room/../../etc:42")).unwrap();
        (d, db)
    }

    fn q(db: &Db, sql: &str) -> String {
        String::from_utf8(db.exec_json(json!({ "q": sql }).to_string().as_bytes())).unwrap()
    }

    #[test]
    fn names_never_become_paths() {
        let p = path_for(Path::new("/data"), "../../etc/passwd");
        assert_eq!(p.parent(), Some(Path::new("/data")));
        assert!(!p.to_string_lossy().contains(".."));
        // An app's objects go in its folder; anything odd doesn't.
        assert_eq!(path_for(Path::new("/data"), "acme/shop/room/r1").parent(), Some(Path::new("/data/acme/shop")));
        assert_eq!(path_for(Path::new("/data"), "../x/room/r1").parent(), Some(Path::new("/data")));
        assert_eq!(path_for(Path::new("/data"), "Acme/shop/room/r1").parent(), Some(Path::new("/data")));
        assert_ne!(path_for(Path::new("/d"), "acme/shop/room/r1"), path_for(Path::new("/d"), "acme/shop/room/r2"));
        // No length limit: the file name is always 64 hex digits.
        assert_eq!(path_for(Path::new("/d"), &"x".repeat(5000)).file_name().unwrap().len(), 64 + ".sqlite".len());
    }

    #[test]
    fn the_cell_cannot_escape_its_database() {
        let (_d, db) = db();
        for bad in [
            "ATTACH DATABASE '/tmp/x.db' AS x",
            "PRAGMA journal_mode = DELETE",
            "PRAGMA writable_schema = ON",
            "BEGIN",
            "COMMIT",
            "SAVEPOINT s",
            "SELECT fts3_tokenizer('simple')",
            "SELECT at FROM _combd_alarm",
            "DELETE FROM _COMBD_alarm",
            "CREATE TRIGGER t AFTER INSERT ON _combd_alarm BEGIN SELECT 1; END",
            "DROP TABLE _combd_alarm",
        ] {
            assert!(q(&db, bad).contains("error"), "{bad} was allowed");
        }
        assert!(q(&db, "CREATE TABLE t(a)").contains("rows"));
        assert!(q(&db, "SELECT 1 + 1").contains("[[2]]"));
    }

    #[test]
    fn alarms_commit_with_the_request() {
        let (_d, db) = db();
        db.begin().unwrap();
        db.set_alarm(Some(123)).unwrap();
        db.rollback();
        assert_eq!(db.alarm(), None);
        db.begin().unwrap();
        db.set_alarm(Some(456)).unwrap();
        db.commit().unwrap();
        assert_eq!(db.alarm(), Some(456));
    }

    #[test]
    fn checkpoints_keep_the_wal_small() {
        let (_d, db) = db();
        q(&db, "CREATE TABLE t(a)");
        let wal_len = || db.wal_file().unwrap().metadata().unwrap().len();
        let mut max = 0;
        for _ in 0..400 {
            db.begin().unwrap();
            let big = format!("{{\"q\":\"INSERT INTO t VALUES (?)\",\"p\":[\"{}\"]}}", "x".repeat(40_000));
            db.exec_json(big.as_bytes());
            db.commit().unwrap();
            if db.wants_checkpoint() {
                checkpoint(db.path()).unwrap();
            }
            max = max.max(wal_len());
        }
        // 16 MB written; the WAL never held much more than one checkpoint's worth.
        let page = 4096;
        assert!(max < 2 * CHECKPOINT_PAGES as u64 * page, "WAL grew to {max} bytes");
        eprintln!("largest WAL: {max} bytes");
        assert!(q(&db, "SELECT count(*) FROM t").contains("[[400]]"));
    }

    /// An image (the restored file as one entry) restores the same database
    /// on its own, and later entries go on top of it as on the long list.
    #[test]
    fn an_image_restores_like_its_segments() {
        let (d, db) = db();
        let mut segs = vec![db.seal().unwrap()];
        q(&db, "CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
        let put = |db: &Db, round: usize| {
            db.begin().unwrap();
            for i in 0..20 {
                let sql = format!("{{\"q\":\"INSERT OR REPLACE INTO t VALUES (?, ?)\",\"p\":[{}, \"v{round}-{}\"]}}", (round * 7 + i) % 97, "x".repeat(round * 40));
                db.exec_json(sql.as_bytes());
            }
            db.commit().unwrap();
        };
        for round in 0..20 {
            put(&db, round);
            segs.push(db.seal().unwrap());
        }
        // Restored from the long list, then imaged before anything opens it.
        let first = d.0.join("first.sqlite");
        restore(&first, &segs).unwrap();
        let img = image(&first).unwrap().expect("an image");
        // More writes after, as the next epoch's uploads.
        let mut later = vec![];
        for round in 20..30 {
            put(&db, round);
            later.push(db.seal().unwrap());
        }
        let want = q(&db, "SELECT k, v FROM t ORDER BY k");
        let long = d.0.join("long.sqlite");
        restore(&long, &[segs.clone(), later.clone()].concat()).unwrap();
        let short = d.0.join("short.sqlite");
        restore(&short, &[vec![img.clone()], later].concat()).unwrap();
        assert_eq!(std::fs::read(&long).unwrap(), std::fs::read(&short).unwrap(), "image + later isn't the long list");
        let rows = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap()["rows"].clone();
        assert_eq!(rows(&q(&Db::open(&short).unwrap(), "SELECT k, v FROM t ORDER BY k")), rows(&want));
        // The image alone is what was restored.
        let alone = d.0.join("alone.sqlite");
        restore(&alone, &[img]).unwrap();
        assert_eq!(std::fs::read(&alone).unwrap(), std::fs::read(&first).unwrap());
        // Nothing there: no image.
        let empty = d.0.join("empty.sqlite");
        std::fs::write(&empty, b"").unwrap();
        assert!(image(&empty).unwrap().is_none());
    }

    /// D2: pages sealed from the WAL rebuild the same database, through
    /// schema changes, many seals, and checkpoints in between.
    #[test]
    fn sealed_pages_restore_the_database() {
        let (d, db) = db();
        let mut segs = vec![db.seal().unwrap()]; // what opening wrote
        q(&db, "CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
        for round in 0..30 {
            db.begin().unwrap();
            for i in 0..20 {
                let sql = format!("{{\"q\":\"INSERT OR REPLACE INTO t VALUES (?, ?)\",\"p\":[{}, \"v{round}-{}\"]}}", (round * 7 + i) % 97, "x".repeat(round * 50));
                db.exec_json(sql.as_bytes());
            }
            db.commit().unwrap();
            if round % 3 == 0 {
                segs.push(db.seal().unwrap());
            }
            if round % 10 == 9 {
                segs.push(db.seal().unwrap());
                db.checkpoint_local().unwrap();
            }
            if round == 15 {
                q(&db, "CREATE INDEX tv ON t(v)");
            }
        }
        segs.push(db.seal().unwrap());
        let want = q(&db, "SELECT k, v FROM t ORDER BY k");
        // A transaction in progress isn't sealed.
        db.begin().unwrap();
        q(&db, "INSERT INTO t VALUES (1000, 'not committed')");
        let unsealed = db.seal().unwrap();
        assert_eq!(&unsealed[8..12], &0u32.to_le_bytes(), "uncommitted pages were sealed");
        db.rollback();

        let copy = d.0.join("restored.sqlite");
        restore(&copy, &segs).unwrap();
        let back = Db::open(&copy).unwrap();
        let got = q(&back, "SELECT k, v FROM t ORDER BY k");
        let rows = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap()["rows"].as_array().unwrap().clone();
        let (g, w) = (rows(&got), rows(&want));
        assert_eq!(w.len(), 97);
        if g != w {
            let first = g.iter().zip(&w).position(|(a, b)| a != b);
            let short = |v: Option<&serde_json::Value>| v.map(|v| v.to_string().chars().take(40).collect::<String>());
            panic!("restored {} rows, want {}; first difference at {first:?}: {:?} vs {:?}", g.len(), w.len(), short(first.and_then(|i| g.get(i))), short(first.and_then(|i| w.get(i))));
        }
        let conn = Connection::open(&copy).unwrap();
        let ok: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
    }

    #[test]
    fn runaway_queries_are_stopped() {
        let (_d, db) = db();
        let t = std::time::Instant::now();
        let r = q(&db, "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c");
        assert!(r.contains("interrupted"), "{r}");
        assert!(t.elapsed().as_millis() < 2000, "took {:?}", t.elapsed());
    }

    #[test]
    fn rollback_drops_the_request() {
        let (_d, db) = db();
        q(&db, "CREATE TABLE t(a)");
        db.begin().unwrap();
        q(&db, "INSERT INTO t VALUES (1)");
        db.rollback();
        db.begin().unwrap();
        assert!(!db.commit().unwrap(), "a read-only request needs no fsync");
        db.begin().unwrap();
        q(&db, "INSERT INTO t VALUES (2)");
        assert!(db.commit().unwrap());
        db.wal_file().unwrap().sync_data().unwrap();
        assert_eq!(q(&db, "SELECT a FROM t"), r#"{"changes":1,"rows":[[2]]}"#);
    }
}

/// A temp directory that cleans up after itself (tests only).
#[cfg(test)]
pub mod tempdir {
    pub struct Dir(pub std::path::PathBuf);

    impl Dir {
        pub fn new() -> Dir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("combd-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
