//! The catalog: deploys, vars, secrets, the state of every app, as an
//! append-only log in the bucket, and a copy on every node. The decision
//! core, with no I/O: the node (and the simulator, `catsim.rs`) does what it
//! says and tells it what happened. `spec/Catalog.tla` is its model.
//!
//! The bucket holds:
//! - `catalog/log/<n>`: the n-th change. Slot n+1 is written only if it's
//!   empty (put-if-absent), so two writers can never both take it: one
//!   order, with no leader needed for safety;
//! - `catalog/snap`: the state at some slot `at`, so the log can be trimmed
//!   (slots up to `at` deleted) and a node far behind can catch up.
//!
//! A write: catch up to the head, check the change against the state there,
//! write the next slot only if empty, then read the snapshot (a trimmed slot
//! is empty again, and a write there lands where nobody reads: if the
//! snapshot covers it, it didn't count), and only then acknowledge and push
//! it to the other nodes. If the slot was taken, someone wrote first: catch
//! up, check again, retry. A lost reply to the write is followed by reading
//! the slot back (entries carry an id), never a blind retry.
//!
//! A node applies a push only if it's the next slot; a later one makes it
//! catch up from the bucket. Catching up has the same trap as writing: a
//! slot read may be a trimmed one that a slow writer has just filled with an
//! entry that will never count. So what's read is held, then the snapshot
//! is read again: if it now covers any of it, that's thrown away and the
//! node starts from the snapshot. (The simulator found this; the first spec
//! only let writers meet a trimmed slot.) One write at a time per node.

use std::collections::{BTreeMap, VecDeque};

pub type Seq = u64;
pub type OpId = u64;
pub type WriteId = u64;

/// A change: which app, what it says now (empty: the app is gone), and an
/// id, so a writer can recognize its own entry when it reads a slot back.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub app: String,
    pub value: Vec<u8>,
}

/// The state after some slots: each app's latest value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub at: Seq,
    pub apps: BTreeMap<String, Vec<u8>>,
}

impl State {
    pub fn apply(&mut self, e: &Entry) {
        self.at += 1;
        if e.value.is_empty() {
            self.apps.remove(&e.app);
        } else {
            self.apps.insert(e.app.clone(), e.value.clone());
        }
    }
}

/// May `e` go after `state`? (Deploys: its domains are free, and so on.)
pub type Check = Box<dyn Fn(&State, &Entry) -> Result<(), String>>;

pub enum Call {
    Get(String),
    /// Write only if absent (the slot is empty).
    PutIfAbsent(String, Vec<u8>),
    /// Write only if the etag still matches (`None`: only if absent).
    PutIfMatch(String, Vec<u8>, Option<String>),
    Put(String, Vec<u8>),
    Delete(String),
}

pub enum Res {
    Got(Option<(Vec<u8>, String)>),
    Put(Result<String, ()>),
    Done,
    /// No reply: the caller can't tell what happened.
    Lost,
}

pub enum Effect {
    Call { op: OpId, call: Call },
    /// Send this change to every other node.
    Push { seq: Seq, entry: Entry },
    /// A write is done: its slot, or why not.
    Ack { write: WriteId, res: Result<Seq, String> },
}

pub enum Event {
    Submit { write: WriteId, entry: Entry },
    Reply { op: OpId, res: Res },
    Push { seq: Seq, entry: Entry },
    /// Look at the bucket in case a push was missed (the slow backstop).
    Tick,
    /// Write a snapshot of this node's copy, then trim the log under it.
    Snapshot,
}

/// Known-bad versions, for the simulator: each must be caught.
#[derive(Clone, Copy, Default, Debug)]
pub struct Bugs {
    pub ack_before_write: bool,
    pub blind_slot: bool,
    pub apply_any_order: bool,
    pub uncovered_check: bool,
    pub trim_no_gate: bool,
    pub trim_early: bool,
    /// Catch up without reading the snapshot again at the end.
    pub read_no_gate: bool,
}

pub fn slot_key(n: Seq) -> String {
    format!("catalog/log/{n:020}")
}
pub const SNAP_KEY: &str = "catalog/snap";

/// Retries before a write gives up (someone else keeps winning the slot).
const MAX_TRIES: u32 = 20;

enum Wait {
    /// Catching up: the snapshot pointer.
    Snap,
    /// Catching up: slot n.
    Slot(Seq),
    /// Catching up, at the end: does a snapshot now cover what was read?
    ReadGate,
    /// The write of slot n.
    Write(Seq),
    /// After a lost write reply: what's in slot n?
    ReadBack(Seq),
    /// After the write: does the snapshot cover slot n?
    Gate(Seq),
    /// Writing a snapshot at `at` (it replaces one at `prev`).
    SnapWrite { at: Seq, prev: Seq },
    Trim,
}

struct Writing {
    write: WriteId,
    entry: Entry,
    tries: u32,
    /// The slot it's going to (after catching up and checking).
    slot: Seq,
}

pub struct Catalog {
    pub copy: State,
    check: Check,
    bugs: Bugs,
    out: Vec<Effect>,
    next_op: OpId,
    waits: BTreeMap<OpId, Wait>,
    /// One write at a time; the rest wait here.
    queue: VecDeque<(WriteId, Entry)>,
    writing: Option<Writing>,
    /// Catching up now (one at a time), and the slots read so far: they
    /// count only once the snapshot is read again (`ReadGate`).
    catching: bool,
    fetched: Vec<Entry>,
    fetched_from: Seq,
    /// What to do once caught up: write (the current one), or nothing.
    then_write: bool,
    /// The snapshot as last read: (at, etag).
    snap: (Seq, Option<String>),
    snapping: bool,
}

impl Catalog {
    pub fn new(copy: State, check: Check, bugs: Bugs) -> Catalog {
        Catalog { copy, check, bugs, out: vec![], next_op: 0, waits: BTreeMap::new(), queue: VecDeque::new(), writing: None, catching: false, fetched: vec![], fetched_from: 0, then_write: false, snap: (0, None), snapping: false }
    }

    fn call(&mut self, w: Wait, c: Call) {
        self.next_op += 1;
        self.waits.insert(self.next_op, w);
        self.out.push(Effect::Call { op: self.next_op, call: c });
    }

    pub fn on(&mut self, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Submit { write, entry } => {
                if self.bugs.ack_before_write {
                    self.out.push(Effect::Ack { write, res: Ok(0) });
                }
                self.queue.push_back((write, entry));
                self.next_write();
            }
            Event::Reply { op, res } => {
                if let Some(w) = self.waits.remove(&op) {
                    self.reply(w, res);
                }
            }
            Event::Push { seq, entry } => {
                if seq == self.copy.at + 1 || (self.bugs.apply_any_order && seq > self.copy.at) {
                    self.copy.apply(&entry);
                } else if seq > self.copy.at + 1 {
                    self.catch_up(false);
                }
            }
            Event::Tick => self.catch_up(false),
            Event::Snapshot => {
                if !self.snapping && !self.catching && self.copy.at > self.snap.0 {
                    self.snapping = true;
                    // Read the pointer first, for its etag: a snapshot only
                    // ever moves forward.
                    self.call(Wait::SnapWrite { at: 0, prev: 0 }, Call::Get(SNAP_KEY.into()));
                }
            }
        }
        std::mem::take(&mut self.out)
    }

    fn next_write(&mut self) {
        if self.writing.is_some() {
            return;
        }
        let Some((write, entry)) = self.queue.pop_front() else { return };
        self.writing = Some(Writing { write, entry, tries: 0, slot: 0 });
        if self.bugs.uncovered_check {
            // The bug: check against our copy as it is, then write wherever
            // the head turns out to be, without checking again.
            let w = self.writing.as_ref().unwrap();
            if let Err(e) = (self.check)(&self.copy, &w.entry) {
                return self.done(Err(e));
            }
        }
        self.catch_up(true);
    }

    fn done(&mut self, res: Result<Seq, String>) {
        let w = self.writing.take().unwrap();
        if !self.bugs.ack_before_write {
            self.out.push(Effect::Ack { write: w.write, res });
        }
        self.next_write();
    }

    /// Catch up to the bucket's head: the snapshot if we're behind it, then
    /// every slot after our copy until an empty one.
    fn catch_up(&mut self, then_write: bool) {
        self.then_write |= then_write;
        if self.catching {
            return;
        }
        self.catching = true;
        self.call(Wait::Snap, Call::Get(SNAP_KEY.into()));
    }

    fn caught_up(&mut self) {
        self.catching = false;
        if !std::mem::take(&mut self.then_write) {
            return;
        }
        let Some(w) = self.writing.as_mut() else { return };
        if !self.bugs.uncovered_check {
            if let Err(e) = (self.check)(&self.copy, &w.entry) {
                return self.done(Err(e));
            }
        }
        let w = self.writing.as_mut().unwrap();
        w.slot = self.copy.at + 1;
        let (slot, body) = (w.slot, encode(&w.entry));
        let call = if self.bugs.blind_slot { Call::Put(slot_key(slot), body) } else { Call::PutIfAbsent(slot_key(slot), body) };
        self.call(Wait::Write(slot), call);
    }

    /// Start over: someone else took the slot (or it didn't count).
    fn retry(&mut self) {
        let w = self.writing.as_mut().unwrap();
        w.tries += 1;
        if w.tries > MAX_TRIES {
            return self.done(Err("the catalog is busy: try again".into()));
        }
        self.catch_up(true);
    }

    fn reply(&mut self, w: Wait, res: Res) {
        match (w, res) {
            // Catching up.
            (Wait::Snap, Res::Got(g)) => {
                if let Some((b, etag)) = g {
                    if let Some(s) = decode_snap(&b) {
                        self.snap = (s.at, Some(etag));
                        if s.at > self.copy.at {
                            self.copy = s;
                        }
                    }
                }
                self.fetched.clear();
                self.fetched_from = self.copy.at + 1;
                let next = self.fetched_from;
                self.call(Wait::Slot(next), Call::Get(slot_key(next)));
            }
            (Wait::Snap, _) => self.call(Wait::Snap, Call::Get(SNAP_KEY.into())),
            (Wait::Slot(n), Res::Got(Some((b, _)))) => {
                match decode(&b) {
                    Some(e) => self.fetched.push(e),
                    None => return self.call(Wait::Snap, Call::Get(SNAP_KEY.into())),
                }
                let next = n + 1;
                self.call(Wait::Slot(next), Call::Get(slot_key(next)));
            }
            (Wait::Slot(_), Res::Got(None)) => self.call(Wait::ReadGate, Call::Get(SNAP_KEY.into())),
            (Wait::Slot(n), _) => self.call(Wait::Slot(n), Call::Get(slot_key(n))),
            (Wait::ReadGate, Res::Got(g)) => {
                let snap = g.and_then(|(b, _)| decode_snap(&b));
                if !self.bugs.read_no_gate && snap.as_ref().is_some_and(|s| s.at >= self.fetched_from) {
                    // Trimmed while we read: what we read may not count.
                    // Start again from the snapshot.
                    return self.call(Wait::Snap, Call::Get(SNAP_KEY.into()));
                }
                // It all counts: apply what this copy doesn't have yet (a
                // push may have brought some of it already).
                for (i, e) in std::mem::take(&mut self.fetched).into_iter().enumerate() {
                    if self.fetched_from + i as u64 == self.copy.at + 1 {
                        self.copy.apply(&e);
                    }
                }
                self.caught_up();
            }
            (Wait::ReadGate, _) => self.call(Wait::ReadGate, Call::Get(SNAP_KEY.into())),
            // The write.
            (Wait::Write(n), Res::Put(Ok(_))) => self.gate(n),
            (Wait::Write(_), Res::Put(Err(()))) => self.retry(),
            (Wait::Write(n), _) => self.call(Wait::ReadBack(n), Call::Get(slot_key(n))),
            (Wait::ReadBack(n), Res::Got(g)) => {
                let mine = g.and_then(|(b, _)| decode(&b)).is_some_and(|e| Some(e.id) == self.writing.as_ref().map(|w| w.entry.id));
                if mine {
                    self.gate(n)
                } else {
                    self.retry()
                }
            }
            (Wait::ReadBack(n), _) => self.call(Wait::ReadBack(n), Call::Get(slot_key(n))),
            // The gate: does a snapshot cover our slot? Then nobody will
            // ever read it: it didn't count.
            (Wait::Gate(n), Res::Got(g)) => {
                let covered = g.and_then(|(b, _)| decode_snap(&b)).is_some_and(|s| s.at >= n);
                if covered && !self.bugs.trim_no_gate {
                    return self.retry();
                }
                let entry = self.writing.as_ref().unwrap().entry.clone();
                if n == self.copy.at + 1 {
                    self.copy.apply(&entry);
                }
                self.out.push(Effect::Push { seq: n, entry });
                self.done(Ok(n));
            }
            (Wait::Gate(n), _) => self.call(Wait::Gate(n), Call::Get(SNAP_KEY.into())),
            // A snapshot: read the pointer, write ours if it's newer (only
            // if the pointer hasn't moved), then trim what it covers.
            (Wait::SnapWrite { at: 0, .. }, Res::Got(g)) => {
                let (prev, etag) = match &g {
                    Some((b, e)) => (decode_snap(b).map_or(0, |s| s.at), Some(e.clone())),
                    None => (0, None),
                };
                if self.copy.at <= prev {
                    self.snapping = false;
                    return;
                }
                let at = self.copy.at;
                self.call(Wait::SnapWrite { at, prev }, Call::PutIfMatch(SNAP_KEY.into(), encode_snap(&self.copy), etag));
            }
            (Wait::SnapWrite { at, prev }, Res::Put(Ok(etag))) if at > 0 => {
                self.snap = (at, Some(etag));
                // Trim: the slots the snapshot covers (the bug: past it).
                let upto = if self.bugs.trim_early { at + 2 } else { at };
                for n in prev + 1..=upto {
                    self.call(Wait::Trim, Call::Delete(slot_key(n)));
                }
                self.snapping = false;
            }
            (Wait::SnapWrite { .. }, _) => self.snapping = false,
            (Wait::Trim, _) => {}
            (Wait::Gate(_) | Wait::Write(_) | Wait::Slot(_) | Wait::Snap | Wait::ReadBack(_) | Wait::ReadGate, Res::Done) => {}
        }
    }

    fn gate(&mut self, n: Seq) {
        self.call(Wait::Gate(n), Call::Get(SNAP_KEY.into()));
    }

    /// Writes in flight or waiting.
    pub fn busy(&self) -> bool {
        self.writing.is_some() || !self.queue.is_empty() || self.catching
    }
}

// The wire format of entries and snapshots: small and plain.
pub fn encode(e: &Entry) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "id": e.id, "app": e.app, "value": hex(&e.value) })).unwrap()
}

pub fn decode(b: &[u8]) -> Option<Entry> {
    let v: serde_json::Value = serde_json::from_slice(b).ok()?;
    Some(Entry { id: v["id"].as_u64()?, app: v["app"].as_str()?.into(), value: unhex(v["value"].as_str()?)? })
}

pub fn encode_snap(s: &State) -> Vec<u8> {
    let apps: serde_json::Map<String, serde_json::Value> = s.apps.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(hex(v)))).collect();
    serde_json::to_vec(&serde_json::json!({ "at": s.at, "apps": apps })).unwrap()
}

pub fn decode_snap(b: &[u8]) -> Option<State> {
    let v: serde_json::Value = serde_json::from_slice(b).ok()?;
    let apps = v["apps"].as_object()?.iter().map(|(k, v)| Some((k.clone(), unhex(v.as_str()?)?))).collect::<Option<_>>()?;
    Some(State { at: v["at"].as_u64()?, apps })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect()
}
