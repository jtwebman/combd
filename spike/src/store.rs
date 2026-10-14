//! Durable Object storage (D1, single node): one SQLite file per object, in
//! the host. The cell sends SQL as JSON and gets rows back as JSON.
//!
//! Each request is one transaction: the host opens it before the request and
//! commits it, with an fsync, before the response leaves (the output gate).
//! If the cell crashes, the transaction rolls back. So a client that saw an
//! answer can trust its writes are on disk, and a crash never leaves half a
//! request behind.
//!
//! SQLite runs in the host, so the cell's SQL is fenced in:
//! - an authorizer denies ATTACH (it could open other files), PRAGMA, and
//!   the cell's own BEGIN / COMMIT / SAVEPOINT (the host owns transactions),
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
        // WAL with synchronous=FULL: a commit returns only after its fsync.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "trusted_schema", "OFF")?;
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
        Ok(Db { conn, host, steps })
    }

    fn host_sql(&self, sql: &str) -> rusqlite::Result<()> {
        self.host.store(true, Relaxed);
        let r = self.conn.execute_batch(sql);
        self.host.store(false, Relaxed);
        r
    }

    pub fn begin(&self) -> rusqlite::Result<()> {
        self.host_sql("BEGIN IMMEDIATE")
    }

    /// The output gate: returns once the request's writes are on disk.
    pub fn commit(&self) -> rusqlite::Result<()> {
        self.host_sql("COMMIT")
    }

    pub fn rollback(&self) {
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
        ] {
            assert!(q(&db, bad).contains("error"), "{bad} was allowed");
        }
        assert!(q(&db, "CREATE TABLE t(a)").contains("rows"));
        assert!(q(&db, "SELECT 1 + 1").contains("[[2]]"));
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
        q(&db, "INSERT INTO t VALUES (2)");
        db.commit().unwrap();
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
