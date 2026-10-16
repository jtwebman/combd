//! The catalog on a real node: a thread around the decision core
//! (`catalog.rs`), doing its bucket calls, pushing new entries to the other
//! nodes over the wire, keeping its copy on local disk (a restart on the
//! same machine starts from it), and telling whoever waits when the copy
//! changes.
//!
//! What the catalog holds, for deploys: `<org>/<app>` is the app's live
//! version and domains (`{"version", "domains"}`; empty when it's taken
//! down), `<org>/<app>#vars` its vars, and its secrets, sealed
//! (`secrets.rs`). The check run against the log's head: no two apps may
//! hold one domain.

use crate::catalog::{decode, encode, encode_snap, decode_snap, Call, Catalog, Effect, Entry, Event, Res, Seq, State};
use crate::kv::Store;
use crate::s3::Cond;
use crate::wire::Wire;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

enum In {
    Submit(Entry, Sender<Result<Seq, String>>),
    Push(Seq, Entry),
    Reply(u64, Res),
}

pub struct CatNode {
    tx: Mutex<Sender<In>>,
    /// The copy as of now, a counter bumped each time it changes, and
    /// whether the first catch-up is done.
    state: Mutex<(State, u64, bool)>,
    changed: Condvar,
    /// Set once the node's wire is up (it starts after this does).
    pub wire: Arc<OnceLock<Wire>>,
}

/// How often a node looks at the bucket in case a push was missed.
const BACKSTOP: Duration = Duration::from_secs(30);
/// Snapshot (and trim) once the log is this far past the last snapshot.
const SNAP_EVERY: u64 = 100;

/// The check: an app's domains are free (another live app holds none).
/// And an app's data key (`#key`, see `secrets.rs`) is written once: two
/// nodes making one at once can't both win, and it's never deleted.
///
/// A pool's manager (`#pool/`, see `pg.rs`) goes up one epoch at a time: a
/// claim is "epoch e + 1, if it's still e", so of two at once, one wins.
pub fn check(s: &State, e: &Entry) -> Result<(), String> {
    if e.app.contains("#pool/") {
        let cur = s.apps.get(&e.app).and_then(|v| crate::pg::manager_of(v)).map_or(0, |m| m.1);
        return match crate::pg::manager_of(&e.value) {
            Some((_, epoch)) if epoch == cur + 1 => Ok(()),
            _ => Err(format!("the pool's manager is at epoch {cur}")),
        };
    }
    if e.app.ends_with("#key") && (e.value.is_empty() || s.apps.contains_key(&e.app)) {
        return Err("the app's data key is written once".into());
    }
    if e.app.contains('#') || e.value.is_empty() {
        return Ok(());
    }
    let mine: Vec<String> = domains(&e.value);
    for (k, v) in &s.apps {
        if k == &e.app || k.contains('#') {
            continue;
        }
        if let Some(d) = domains(v).into_iter().find(|d| mine.contains(d)) {
            return Err(format!("domain {d} is already {k}'s"));
        }
    }
    Ok(())
}

fn domains(v: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Value>(v).ok().and_then(|v| v["domains"].as_array().map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect())).unwrap_or_default()
}

fn new_id() -> u64 {
    let mut b = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    u64::from_le_bytes(b) | 1
}

impl CatNode {
    /// Start it: the copy from `local` (if any), then it catches up.
    pub fn start(store: Arc<dyn Store>, local: PathBuf, peers: Vec<crate::own::Node>) -> Arc<CatNode> {
        let copy = std::fs::read(&local).ok().and_then(|b| decode_snap(&b)).unwrap_or_default();
        let (tx, rx) = channel();
        let node = Arc::new(CatNode { tx: Mutex::new(tx.clone()), state: Mutex::new((copy.clone(), 0, false)), changed: Condvar::new(), wire: Arc::new(OnceLock::new()) });
        let n = node.clone();
        std::thread::spawn(move || n.run(Catalog::new(copy, Box::new(check), Default::default()), store, local, peers, rx, tx));
        node
    }

    fn run(&self, mut core: Catalog, store: Arc<dyn Store>, local: PathBuf, peers: Vec<crate::own::Node>, rx: Receiver<In>, tx: Sender<In>) {
        let mut waiting: Vec<(u64, Sender<Result<Seq, String>>)> = vec![];
        let mut fx = core.on(Event::Tick);
        loop {
            let mut acks = vec![];
            for f in std::mem::take(&mut fx) {
                match f {
                    Effect::Call { op, call } => {
                        let (store, tx) = (store.clone(), tx.clone());
                        std::thread::spawn(move || {
                            let res = match call {
                                Call::Get(k) => store.get(&k).map(Res::Got),
                                Call::Put(k, v) => store.put(&k, &v, &Cond::Any).map(Res::Put),
                                Call::PutIfAbsent(k, v) => store.put(&k, &v, &Cond::IfAbsent).map(Res::Put),
                                Call::PutIfMatch(k, v, e) => store.put(&k, &v, &e.map_or(Cond::IfAbsent, Cond::IfMatch)).map(Res::Put),
                                Call::Delete(k) => store.delete(&k).map(|()| Res::Done),
                            };
                            let _ = tx.send(In::Reply(op, res.unwrap_or(Res::Lost)));
                        });
                    }
                    Effect::Push { seq, entry } => {
                        if let Some(w) = self.wire.get() {
                            let b = encode(&entry);
                            for &p in &peers {
                                w.push(p, crate::fleet::Frame::Catalog { seq, entry: b.clone() });
                            }
                        }
                    }
                    Effect::Ack { write, res } => acks.push((write, res)),
                }
            }
            // The copy moved: keep it on disk, and tell whoever waits.
            {
                let mut st = self.state.lock().unwrap();
                if st.0 != core.copy {
                    st.0 = core.copy.clone();
                    st.1 += 1;
                    let _ = std::fs::write(local.with_extension("tmp"), encode_snap(&core.copy)).and_then(|()| std::fs::rename(local.with_extension("tmp"), &local));
                    self.changed.notify_all();
                }
                if !st.2 && !core.busy() {
                    st.2 = true;
                    self.changed.notify_all();
                }
            }
            // Acks after the copy moves: once `submit` returns, `now` has it.
            for (write, res) in acks {
                if let Some(i) = waiting.iter().position(|(w, _)| *w == write) {
                    let _ = waiting.swap_remove(i).1.send(res);
                }
            }
            // A long log since the last snapshot: snapshot and trim it.
            if core.copy.at >= core.snap_at() + SNAP_EVERY && !core.busy() {
                fx = core.on(Event::Snapshot);
                if !fx.is_empty() {
                    continue;
                }
            }
            fx = match rx.recv_timeout(BACKSTOP) {
                Ok(In::Submit(e, reply)) => {
                    let id = e.id;
                    waiting.push((id, reply));
                    core.on(Event::Submit { write: id, entry: e })
                }
                Ok(In::Push(seq, entry)) => core.on(Event::Push { seq, entry }),
                Ok(In::Reply(op, res)) => core.on(Event::Reply { op, res }),
                Err(RecvTimeoutError::Timeout) => core.on(Event::Tick),
                Err(RecvTimeoutError::Disconnected) => return,
            };
        }
    }

    /// Write a change: `Ok(slot)` once it's in the log, or why not (the check
    /// failed against the log's head, or the catalog is busy).
    pub fn submit(&self, app: &str, value: Vec<u8>) -> Result<Seq, String> {
        let (tx, rx) = channel();
        let entry = Entry { id: new_id(), app: app.to_string(), value };
        self.tx.lock().unwrap().send(In::Submit(entry, tx)).map_err(|_| "the catalog stopped".to_string())?;
        rx.recv_timeout(Duration::from_secs(60)).map_err(|_| "the catalog didn't answer in time".to_string())?
    }

    /// A push from another node.
    pub fn push(&self, seq: Seq, entry: &[u8]) {
        if let Some(e) = decode(entry) {
            let _ = self.tx.lock().unwrap().send(In::Push(seq, e));
        }
    }

    pub fn now(&self) -> (State, u64) {
        let st = self.state.lock().unwrap();
        (st.0.clone(), st.1)
    }

    /// Wait until the copy changes past `gen`, or `timeout` passes.
    pub fn wait(&self, gen: u64, timeout: Duration) -> (State, u64) {
        let st = self.state.lock().unwrap();
        let (st, _) = self.changed.wait_timeout_while(st, timeout, |s| s.1 == gen).unwrap();
        (st.0.clone(), st.1)
    }

    /// Wait for the first catch-up (`false`: not within `timeout`).
    pub fn ready(&self, timeout: Duration) -> bool {
        let st = self.state.lock().unwrap();
        self.changed.wait_timeout_while(st, timeout, |s| !s.2).unwrap().0 .2
    }
}
