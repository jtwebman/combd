//! Fuzzing the host API: the wall between a cell and the host. A cell calls
//! host functions with pointers, lengths, and SQL it picked, and it even
//! picks their types, so every argument is hostile. `combd-spike fuzz-host N
//! [start]` makes random calls (right and wrong arities, edge-value pointers
//! and lengths, nasty SQL, random bytes) against a real object database and
//! checks after each one:
//! - the host never panics,
//! - no call takes longer than `SLOW` (the cell's fuel doesn't pay for host
//!   time; the old `fd_write` took seconds with big memories, the old `sql`
//!   gigabytes),
//! - the alarm changes only through `alarm_set`,
//! - the host's transaction is still open (a cell can't commit or roll back),
//! - no file appears except the object's own database files.

use crate::engine::Import;
use crate::host::{self, HostState, Mem};
use crate::sim::Rng;
use crate::store::Db;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Well above the 100 ms a single SQL statement may take (plus its
/// rollback). The test allows more: a busy machine can pause it for 200 ms.
pub const SLOW: Duration = Duration::from_millis(250);

/// A cell's memory, bounds-checked like a real one.
struct Vm(Vec<u8>);

impl Mem for Vm {
    fn read(&self, ptr: u32, len: u32) -> Option<Vec<u8>> {
        self.0.get(ptr as usize..(ptr as usize).checked_add(len as usize)?).map(<[u8]>::to_vec)
    }
    fn write(&mut self, ptr: u32, bytes: &[u8]) -> bool {
        match self.0.get_mut(ptr as usize..(ptr as usize).saturating_add(bytes.len())) {
            Some(to) => {
                to.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

/// (module, name, how many arguments it really takes).
const IMPORTS: &[(&str, &str, usize)] = &[
    ("combd", "counter_incr", 0),
    ("combd", "sql", 2),
    ("combd", "sql_read", 1),
    ("combd", "now_ms", 0),
    ("combd", "alarm_set", 1),
    ("combd", "alarm_get", 0),
    ("combd", "ws_send", 3),
    ("combd", "ws_close", 2),
    ("combd", "ws_list", 2),
    ("env", "abort", 4),
    ("wasi_snapshot_preview1", "fd_write", 4),
    ("wasi_snapshot_preview1", "fd_read", 4),
    ("wasi_snapshot_preview1", "random_get", 2),
    ("wasi_snapshot_preview1", "clock_time_get", 3),
    ("wasi_snapshot_preview1", "clock_res_get", 2),
    ("wasi_snapshot_preview1", "args_sizes_get", 2),
    ("wasi_snapshot_preview1", "environ_sizes_get", 2),
    ("wasi_snapshot_preview1", "args_get", 2),
    ("wasi_snapshot_preview1", "path_open", 9),
    ("wasi_snapshot_preview1", "fd_close", 1),
    ("wasi_snapshot_preview1", "proc_exit", 1),
    ("wasi_snapshot_preview1", "sched_yield", 0),
    ("nope", "nothing", 0),
];

/// SQL a hostile cell might try, plus the plain kind.
const SQL: &[&str] = &[
    "SELECT 1",
    "CREATE TABLE IF NOT EXISTS t (k INTEGER PRIMARY KEY, v)",
    "INSERT INTO t VALUES (?, ?)",
    "SELECT * FROM t",
    "DELETE FROM t",
    "SELECT * FROM _combd_alarm",
    "INSERT OR REPLACE INTO _combd_alarm VALUES (1, 5)",
    "DELETE FROM _combd_alarm",
    "UPDATE _combd_alarm SET at = 1",
    "DROP TABLE _combd_alarm",
    "ALTER TABLE _combd_alarm RENAME TO x",
    "CREATE TRIGGER tr AFTER INSERT ON _combd_alarm BEGIN DELETE FROM t; END",
    "CREATE TRIGGER tr2 AFTER INSERT ON t BEGIN DELETE FROM _combd_alarm; END",
    "CREATE VIEW v AS SELECT * FROM _combd_alarm",
    "SELECT * FROM v",
    "CREATE TEMP TABLE tt (x)",
    "CREATE TABLE \"_COMBD_x\" (x)",
    "SELECT * FROM main._combd_alarm",
    "SELECT * FROM sqlite_master",
    "ATTACH DATABASE '%DIR%/x.db' AS x",
    "ATTACH ':memory:' AS m",
    "DETACH x",
    "PRAGMA writable_schema = 1",
    "PRAGMA journal_mode = DELETE",
    "PRAGMA synchronous = FULL",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "END",
    "SAVEPOINT s",
    "RELEASE s",
    "VACUUM",
    "VACUUM INTO '%DIR%/copy.db'",
    "SELECT load_extension('%DIR%/x')",
    "SELECT fts3_tokenizer('simple')",
    "SELECT readfile('/etc/passwd')",
    "SELECT writefile('%DIR%/w', 'x')",
    "CREATE VIRTUAL TABLE vt USING fts5(x)",
    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT count(*) FROM r",
    "SELECT zeroblob(1000000000)",
    "SELECT randomblob(100000000)",
    "SELECT printf('%.*c', 16000000, 'x')",
    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 200) SELECT printf('%.*c', 15000000, 'x') FROM r",
    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 200) SELECT printf('%.*c', 2000000, 'x') FROM r",
    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 200) SELECT count(DISTINCT printf('%.*c', 15000000, n)) FROM r",
    "CREATE TABLE IF NOT EXISTS big AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 500) SELECT printf('%.*c', 2000000, 'x') AS s FROM r",
    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 1000000) SELECT n, n, n, n FROM r",
    "SELECT 1; DELETE FROM t",
    "SELECT ?, ?, ?",
    "SELECT x'00ff'",
    "",
    ";",
];

fn edge(rng: &mut Rng, mem_len: u32) -> u64 {
    let picks = [0, 1, mem_len.saturating_sub(1), mem_len, mem_len.wrapping_add(1), u32::MAX, u32::MAX - 7, 0x8000_0000, 16 << 20, rng.below(mem_len as u64 + 1) as u32];
    let v = picks[rng.below(picks.len() as u64) as usize] as u64;
    if rng.chance(0.05) { rng.next() } else { v }
}

/// A request for `sql`: usually well-formed JSON, sometimes not.
fn sql_request(rng: &mut Rng, dir: &str) -> Vec<u8> {
    let q = SQL[rng.below(SQL.len() as u64) as usize].replace("%DIR%", dir);
    let params: Vec<serde_json::Value> = (0..rng.below(4)).map(|_| match rng.below(5) {
        0 => serde_json::json!(rng.next() as i64),
        1 => serde_json::json!("x".repeat(rng.below(64) as usize)),
        2 => serde_json::json!(f64::from_bits(rng.next())),
        3 => serde_json::json!({"a": 1}),
        _ => serde_json::Value::Null,
    }).collect();
    match rng.below(10) {
        0 => (0..rng.below(64)).map(|_| rng.next() as u8).collect(),
        1 => br#"{"q": 5}"#.to_vec(),
        2 => serde_json::json!({ "q": q, "p": "not a list" }).to_string().into_bytes(),
        _ => serde_json::json!({ "q": q, "p": params }).to_string().into_bytes(),
    }
}

fn files(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).map(|r| r.filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned())).collect()).unwrap_or_default();
    v.sort();
    v
}

pub struct Stats {
    pub calls: u64,
    pub crashed: u64,
    pub slowest: (Duration, String),
    pub problems: Vec<String>,
}

/// One seed: a fresh object and cell memory, then a run of random calls.
fn one(seed: u64, slow: Duration, st: &mut Stats) {
    let mut rng = Rng::new(seed);
    let dir = std::env::temp_dir().join(format!("combd-fuzz-host-{}-{seed}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ds = dir.display().to_string();
    let db = Arc::new(Mutex::new(Db::open(&dir.join("obj.sqlite")).unwrap()));
    db.lock().unwrap().begin().unwrap();
    let mut hs = HostState { db: Some(db.clone()), ws_ids: (1..rng.below(5)).collect(), ..HostState::default() };
    // Mostly small memories; now and then a big one (a host cost that
    // scales with memory only shows there: this machine copies ~70 GB/s).
    let mem_len = [0u32, 1, 65536, 1 << 20, 1 << 20, 1 << 20, 16 << 20, 64 << 20][rng.below(8) as usize];
    let mut vm = Vm((0..mem_len).map(|_| rng.next() as u8).collect());
    let mut alarm: Option<i64> = None;
    let allowed = |f: &str| f.starts_with("obj.sqlite");
    for _ in 0..40 {
        // `sql` is where most of the host's work is: call it often.
        let (module, name, n) = if rng.chance(0.4) { IMPORTS[1] } else { IMPORTS[rng.below(IMPORTS.len() as u64) as usize] };
        // Usually the right arity; sometimes not: the cell picks its own types.
        let arity = if rng.chance(0.8) { n } else { rng.below(10) as usize };
        let mut args: Vec<u64> = (0..arity).map(|_| edge(&mut rng, mem_len)).collect();
        if name == "sql" && arity >= 2 && !vm.0.is_empty() && rng.chance(0.8) {
            // Put a request in memory and point at it.
            let req = sql_request(&mut rng, &ds);
            let at = rng.below(vm.0.len().saturating_sub(req.len()) as u64 + 1) as usize;
            let n = req.len().min(vm.0.len() - at);
            vm.0[at..at + n].copy_from_slice(&req[..n]);
            args[0] = at as u64;
            args[1] = if rng.chance(0.9) { n as u64 } else { edge(&mut rng, mem_len) };
        }
        if name == "fd_write" && arity >= 4 && vm.0.len() >= 16 && rng.chance(0.5) {
            // Real iovs: up to 20,000 buffers inside memory, some huge.
            let k = (rng.below(20_000) as usize + 1).min(vm.0.len() / 8);
            let len = vm.0.len() as u64;
            for i in 0..k {
                let p = rng.below(len) as u32;
                let l = if rng.chance(0.5) { (len - p as u64) as u32 } else { rng.below(len - p as u64 + 1) as u32 };
                vm.0[i * 8..i * 8 + 4].copy_from_slice(&p.to_le_bytes());
                vm.0[i * 8 + 4..i * 8 + 8].copy_from_slice(&l.to_le_bytes());
            }
            (args[0], args[1], args[2]) = (1, 0, k as u64);
        }
        if name == "alarm_set" && arity >= 1 && rng.chance(0.5) {
            args[0] = (rng.below(3) as i64 - 1) as u64 * rng.below(1 << 40);
        }
        let imp = Import { module: module.into(), name: name.into(), results: 1 };
        let what = format!("{module}.{name}({args:x?})");
        let t = Instant::now();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host::call(&imp, &args, &mut vm, &mut hs)));
        let took = t.elapsed();
        st.calls += 1;
        match r {
            Err(_) => st.problems.push(format!("seed {seed}: the host panicked on {what}")),
            Ok(Err(_)) => {
                // The cell crashed: the host rolls its request back and the
                // next request starts fresh, as the event loop does.
                st.crashed += 1;
                let d = db.lock().unwrap();
                d.rollback();
                d.begin().unwrap();
                alarm = d.alarm();
            }
            Ok(Ok(_)) => {
                if name == "alarm_set" && arity >= 1 {
                    let at = args[0] as i64;
                    alarm = (at > 0).then_some(at);
                }
            }
        }
        if took > st.slowest.0 {
            st.slowest = (took, what.chars().take(200).collect());
        }
        if took > slow {
            st.problems.push(format!("seed {seed}: {:?} for {}", took, what.chars().take(200).collect::<String>()));
        }
        let d = db.lock().unwrap();
        if d.alarm() != alarm {
            st.problems.push(format!("seed {seed}: the alarm is {:?}, wanted {alarm:?}, after {what}", d.alarm()));
            alarm = d.alarm();
        }
        if !d.in_transaction() {
            st.problems.push(format!("seed {seed}: the host's transaction ended, after {what}"));
            let _ = d.begin();
        }
        drop(d);
        let stray: Vec<String> = files(&dir).into_iter().filter(|f| !allowed(f)).collect();
        if !stray.is_empty() {
            st.problems.push(format!("seed {seed}: files appeared: {stray:?}, after {what}"));
            for f in stray {
                let _ = std::fs::remove_file(dir.join(f));
            }
        }
    }
    drop(hs);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

pub fn run(seeds: std::ops::Range<u64>, slow: Duration) -> Stats {
    let mut st = Stats { calls: 0, crashed: 0, slowest: (Duration::ZERO, String::new()), problems: vec![] };
    for seed in seeds {
        one(seed, slow, &mut st);
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe, not a rule: how long the worst calls we can think of take.
    #[test]
    #[ignore]
    fn probe_costly_calls() {
        let dir = std::env::temp_dir().join(format!("combd-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Arc::new(Mutex::new(Db::open(&dir.join("obj.sqlite")).unwrap()));
        db.lock().unwrap().begin().unwrap();
        let mut hs = HostState { db: Some(db), ..HostState::default() };
        // 1. fd_write: 100,000 iovs, each pointing at all 1 MiB of memory.
        let mut vm = Vm(vec![0u8; 1 << 20]);
        let n = 100_000u32;
        for i in 0..n as usize {
            vm.0[i * 8 + 4..i * 8 + 8].copy_from_slice(&(1u32 << 20).to_le_bytes());
        }
        let imp = Import { module: "wasi_snapshot_preview1".into(), name: "fd_write".into(), results: 1 };
        let t = Instant::now();
        let r = host::call(&imp, &[1, 0, n as u64, 0], &mut vm, &mut hs);
        println!("fd_write x{n} iovs: {:?} -> {r:?}", t.elapsed());
        // 2. sql: 200 rows of 15 MB each.
        let q = br#"{"q":"WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 200) SELECT printf('%.*c', 2000000, 'x') FROM r"}"#;
        vm.0[..q.len()].copy_from_slice(q);
        let imp = Import { module: "combd".into(), name: "sql".into(), results: 1 };
        let t = Instant::now();
        let r = host::call(&imp, &[0, q.len() as u64], &mut vm, &mut hs);
        println!("sql 200 x 2 MB rows: {:?} -> {r:?}, answer {} bytes", t.elapsed(), hs.sql_out.len());
        // 3. The same, sorted: SQLite's sorter must hold every row.
        for q in [
            &br#"{"q":"WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 200) SELECT printf('%.*c', 2000000, 'x') AS s FROM r ORDER BY s DESC LIMIT 1"}"#[..],
            &br#"{"q":"WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 200) SELECT count(DISTINCT printf('%.*c', 2000000, n)) FROM r"}"#[..],
            &br#"{"q":"CREATE TABLE big AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 200) SELECT printf('%.*c', 2000000, 'x') AS s FROM r"}"#[..],
        ] {
            vm.0[..q.len()].copy_from_slice(q);
            let t = Instant::now();
            let r = host::call(&imp, &[0, q.len() as u64], &mut vm, &mut hs);
            let out = String::from_utf8_lossy(&hs.sql_out).chars().take(80).collect::<String>();
            let size: u64 = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum();
            println!("{}: {:?} -> {r:?} {out}; files in the object dir {} MB", String::from_utf8_lossy(&q[8..60]), t.elapsed(), size >> 20);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_host_api_holds_under_hostile_calls() {
        let st = super::run(0..100, Duration::from_secs(1));
        assert!(st.problems.is_empty(), "{:#?}", st.problems);
        assert!(st.calls > 3000, "{}", st.calls);
    }
}
