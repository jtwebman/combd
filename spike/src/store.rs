//! Durable Object storage (D1, single node): one SQLite file per object, in
//! the host. The cell sends SQL as JSON and gets rows back as JSON.
//!
//! Each request is one transaction: the host opens it before the request and
//! commits it after. If the cell crashes, the transaction rolls back, so a
//! crash never leaves half a request behind.
//!
//! Durability is a group commit. The commit itself doesn't fsync (WAL mode,
//! synchronous=NORMAL, which can lose recent commits on power loss but never
//! corrupts). The host fsyncs the WAL file separately, off the event loop,
//! and holds each answer until an fsync has covered its commit (the output
//! gate). One fsync covers every commit made before it started, so a busy
//! object pays one fsync for many requests, and many objects' fsyncs run at
//! once. A client that saw an answer can trust its writes are on disk.
//!
//! SQLite runs in the host, so the cell's SQL is fenced in:
//! - an authorizer denies ATTACH (it could open other files), PRAGMA, the
//!   cell's own BEGIN / COMMIT / SAVEPOINT (the host owns transactions), and
//!   any table named `_combd...` (the host's own, like the alarm),
//! - limits cap the size of strings, SQL text, and expression depth,
//! - a progress handler stops any one statement after `MAX_STEPS`, since
//!   SQLite's CPU use isn't counted by the cell's fuel.

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value, ValueRef};
use rusqlite::Connection;
use serde_json::{json, Value as Json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Arc;

/// Progress callbacks (every 1,000 SQLite VM steps) before a statement is
/// stopped: about 10 million steps, tens of milliseconds.
const MAX_STEPS: u32 = 10_000;
/// The biggest answer a cell can get back, and the biggest SQL it can send.
pub const MAX_RESULT: usize = 16 << 20;

pub struct Db {
    conn: Connection,
    wal_path: PathBuf,
    /// The WAL file, opened at the first fsync. SQLite keeps it (same inode)
    /// for as long as this connection is open.
    wal: std::cell::OnceCell<Arc<std::fs::File>>,
    /// The open transaction changed something.
    wrote: std::cell::Cell<bool>,
    /// True while the host itself runs SQL (BEGIN, COMMIT).
    host: Arc<AtomicBool>,
    steps: Arc<AtomicU32>,
}

/// Where an object's file lives. Names come from users, so the file name is
/// the name in hex: no path tricks, no collisions.
pub fn path_for(dir: &Path, name: &str) -> PathBuf {
    let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{hex}.sqlite"))
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        // Commits don't fsync; `wal_file` is fsynced by the host instead.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "trusted_schema", "OFF")?;
        // Many objects per node: keep each one's page cache small (256 KiB).
        conn.pragma_update(None, "cache_size", -256)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS _combd_alarm (id INTEGER PRIMARY KEY CHECK (id = 1), at INTEGER NOT NULL)")?;
        for (limit, v) in [
            (Limit::SQLITE_LIMIT_LENGTH, MAX_RESULT as i32),
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
        let s = steps.clone();
        conn.progress_handler(1000, Some(move || s.fetch_add(1, Relaxed) >= MAX_STEPS));
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        Ok(Db { conn, wal_path: wal_path.into(), wal: Default::default(), wrote: Default::default(), host, steps })
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
        Ok(self.wrote.replace(false))
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
        let mut st = self.conn.prepare_cached(sql).map_err(|e| e.to_string())?;
        if !st.readonly() {
            self.wrote.set(true);
        }
        let cols = st.column_count();
        let mut rows = st.query(rusqlite::params_from_iter(params)).map_err(|e| e.to_string())?;
        let mut out = vec![];
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut r = Vec::with_capacity(cols);
            for i in 0..cols {
                r.push(match row.get_ref(i).map_err(|e| e.to_string())? {
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
