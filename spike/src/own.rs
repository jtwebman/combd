//! The ownership decision core for D2: who serves a Durable Object when the
//! bucket is the only coordinator. It implements the protocol checked in
//! `spec/Ownership.tla`.
//!
//! Pure: `Core::on(now, event) -> effects`. No I/O, no clock of its own, no
//! randomness. The real node and the simulator (`sim.rs`) both drive it, so
//! the simulator tests the very code that runs.
//!
//! The core never touches an object's data. It tells the executor what to do
//! and when (run a request, seal a segment, restore from segments, drop the
//! object), and the executor owns the data: a list of writes in the
//! simulator, a cell and SQLite in a real node.
//!
//! Bucket keys, per object `o`:
//! - `own/o`: the ownership record `{node, epoch}`, changed only by CAS.
//! - `obj/o/e<E>/m`: the manifest: the segment keys the owner of epoch E
//!   started from, in order.
//! - `obj/o/e<E>/s<K>`: segment K of epoch E: the changes of one upload.
//!
//! And per node `n`: `lease/n`, a heartbeat the node bumps. A node that hasn't
//! bumped it for `LEASE_MS`, by the *observer's* clock, can be taken over.
//! Leases are for liveness only: safety holds with any clocks.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// An object's id: a 128-bit hash of its name (`obj_id`), like Cloudflare's
/// `idFromName`. Bucket keys use it in hex.
pub type Obj = u128;
pub type Node = u32;
pub type ReqId = u64;
pub type OpId = u64;
/// One activation of one object on this node. Events from an earlier one
/// are ignored.
pub type Act = u64;

/// How long a node may go without a heartbeat before others take over, in
/// the simulator. Real nodes set it (`COMBD_LEASE_MS`): it must be several
/// times the worst round trip to the bucket, and shorter means more bucket
/// writes (one per node per third of a lease).
pub const LEASE_MS: u64 = 1000;
pub const RENEW_MS: u64 = 200;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rec {
    pub node: Node,
    pub epoch: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Rec(Rec),
    Lease(u64),
    /// Segment keys, in order.
    Manifest(Vec<String>),
    /// Opaque changes, from the executor.
    Seg(Vec<u8>),
}

impl Val {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Val::Rec(r) => serde_json::to_vec(r).unwrap(),
            Val::Lease(n) => n.to_string().into_bytes(),
            Val::Manifest(m) => serde_json::to_vec(m).unwrap(),
            Val::Seg(b) => b.clone(),
        }
    }

    /// The key says what kind of value it holds.
    pub fn decode(key: &str, b: &[u8]) -> Option<Val> {
        if key.starts_with("own/") {
            serde_json::from_slice(b).ok().map(Val::Rec)
        } else if key.starts_with("lease/") {
            std::str::from_utf8(b).ok()?.parse().ok().map(Val::Lease)
        } else if key.ends_with("/m") {
            serde_json::from_slice(b).ok().map(Val::Manifest)
        } else {
            Some(Val::Seg(b.to_vec()))
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cond {
    Any,
    IfAbsent,
    /// An etag.
    IfMatch(String),
}

#[derive(Clone, Debug)]
pub enum Call {
    Get(String),
    Put(String, Val, Cond),
    /// The keys under a prefix, in order.
    List(String),
}

#[derive(Clone, Debug)]
pub enum Res {
    /// Value and etag.
    Got(Option<(Val, String)>),
    /// The new etag, or `Err` if the condition failed.
    Put(Result<String, ()>),
    Listed(Vec<String>),
    /// No reply in time: the call may or may not have happened.
    Lost,
}

pub enum Event {
    Request { obj: Obj, req: ReqId },
    /// The executor ran a request; `changed`: it wrote something.
    Ran { obj: Obj, act: Act, req: ReqId, changed: bool },
    /// The changes since the last seal, ready to upload.
    Sealed { obj: Obj, act: Act, changes: Vec<u8> },
    /// The executor rebuilt the object from its segments (or couldn't).
    Restored { obj: Obj, act: Act, ok: bool },
    Reply { op: OpId, res: Res },
    /// Time passes: renew our lease.
    Tick,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// Done and proven: send the answer the executor made.
    Done { epoch: u32 },
    /// Another node owns it; ask there.
    Redirect(Node),
    /// Not done, or not known to be done: try again.
    Failed,
}

#[derive(Debug)]
pub enum Effect {
    Call { op: OpId, call: Call },
    Answer { req: ReqId, ans: Answer },
    /// Build the object from these segments, in order, from nothing.
    Restore { obj: Obj, act: Act, segs: Vec<String> },
    /// Run a request, in order, after the ones before it.
    Run { obj: Obj, act: Act, req: ReqId },
    /// Hand over the changes made since the last seal.
    Seal { obj: Obj, act: Act },
    /// Forget the object's state: we don't own it now.
    Drop { obj: Obj },
}

/// Known-bad versions, for proving the simulator can catch them. Never set
/// outside tests.
#[derive(Clone, Copy, Default, Debug)]
pub struct Bugs {
    pub skip_proof: bool,
    pub load_before_cas: bool,
    pub no_manifest: bool,
    pub reuse_epoch: bool,
}

/// An object's id from its name: the first 128 bits of SHA-256.
pub fn obj_id(name: &str) -> Obj {
    use sha2::{Digest, Sha256};
    u128::from_be_bytes(Sha256::digest(name.as_bytes())[..16].try_into().unwrap())
}

pub fn own_key(o: Obj) -> String {
    format!("own/{o:032x}")
}
pub fn lease_key(n: Node) -> String {
    format!("lease/{n}")
}
pub fn obj_prefix(o: Obj) -> String {
    format!("obj/{o:032x}/")
}
pub fn manifest_key(o: Obj, e: u32) -> String {
    format!("obj/{o:032x}/e{e:08}/m")
}
fn seg_key(o: Obj, e: u32, k: u32) -> String {
    format!("obj/{o:032x}/e{e:08}/s{k:08}")
}
/// The epoch and the part (`m` or `s...`) of an object key.
pub fn parse_key(o: Obj, k: &str) -> Option<(u32, &str)> {
    let rest = k.strip_prefix(&format!("obj/{o:032x}/e"))?;
    let (e, part) = rest.split_once('/')?;
    Some((e.parse().ok()?, part))
}

/// What a bucket call was for.
#[derive(Clone, Debug)]
enum Wait {
    ReadRec(Obj),
    OwnerLease(Obj, Rec, String),
    Cas(Obj, u32),
    ReadBack(Obj, u32),
    /// Only with `Bugs::load_before_cas`: the history, read before the CAS.
    EarlyList(Obj, Option<String>, u32),
    EarlyManifest(Obj, Option<String>, u32, Vec<String>),
    List(Obj, u32),
    /// Reading the manifest of an earlier epoch, plus its segments.
    PrevManifest(Obj, u32, Vec<String>),
    Manifest(Obj, u32),
    Upload(Obj, u32),
    Prove(Obj, u32),
    LeaseRead,
    LeaseWrite(u64),
}

impl Wait {
    fn obj(&self) -> Option<Obj> {
        use Wait::*;
        match self {
            ReadRec(o) | OwnerLease(o, ..) | Cas(o, _) | ReadBack(o, _) | EarlyList(o, ..) | EarlyManifest(o, ..) | List(o, _)
            | PrevManifest(o, ..) | Manifest(o, _) | Upload(o, _) | Prove(o, _) => Some(*o),
            LeaseRead | LeaseWrite(_) => None,
        }
    }
}

#[derive(Debug)]
enum Phase {
    Idle,
    /// Claiming, loading, writing the manifest, or restoring.
    Starting,
    Serving(Serve),
}

#[derive(Debug)]
struct Serve {
    epoch: u32,
    act: Act,
    next_seg: u32,
    /// Sent to the executor, not yet run.
    running: HashSet<ReqId>,
    /// Run, waiting for the next upload and proof.
    batch: Vec<ReqId>,
    dirty: bool,
    /// Asked for a seal; waiting for the changes.
    sealing: bool,
    /// In an upload and proof now.
    flight: Option<Vec<ReqId>>,
}

struct ObjSt {
    phase: Phase,
    /// Requests waiting for the object to start.
    waiting: VecDeque<ReqId>,
    /// Segment keys read before the CAS (only with `Bugs::load_before_cas`).
    early: Option<Vec<String>>,
    /// The activation being started, and its epoch.
    act: Act,
    epoch: u32,
}

pub struct Core {
    pub node: Node,
    bugs: Bugs,
    lease_ms: u64,
    next_op: OpId,
    next_act: Act,
    ops: HashMap<OpId, Wait>,
    objs: BTreeMap<Obj, ObjSt>,
    /// The segment list of a manifest being written, until it's written.
    pending_base: HashMap<(Obj, u32), Vec<String>>,
    /// Our heartbeat: the value we last wrote, and whether a renew is out.
    lease: u64,
    renewing: bool,
    /// Other nodes' heartbeats as we last saw them: (value, when it changed, by our clock).
    seen: HashMap<Node, (u64, u64)>,
    /// Who owned an object when we last checked, and when (our clock). For
    /// routing only: a fresh hint skips the bucket and redirects at once.
    hints: HashMap<Obj, (Node, u64)>,
    out: Vec<Effect>,
}

impl Core {
    pub fn new(node: Node, bugs: Bugs) -> Core {
        Core::with_lease(node, bugs, LEASE_MS)
    }

    pub fn with_lease(node: Node, bugs: Bugs, lease_ms: u64) -> Core {
        Core {
            node,
            bugs,
            lease_ms,
            next_op: 0,
            next_act: 0,
            ops: HashMap::new(),
            objs: BTreeMap::new(),
            pending_base: HashMap::new(),
            lease: 0,
            renewing: false,
            seen: HashMap::new(),
            hints: HashMap::new(),
            out: vec![],
        }
    }

    fn call(&mut self, w: Wait, c: Call) {
        self.next_op += 1;
        self.ops.insert(self.next_op, w);
        self.out.push(Effect::Call { op: self.next_op, call: c });
    }

    fn answer(&mut self, req: ReqId, ans: Answer) {
        self.out.push(Effect::Answer { req, ans });
    }

    fn st(&mut self, o: Obj) -> &mut ObjSt {
        self.objs.entry(o).or_insert_with(|| ObjSt { phase: Phase::Idle, waiting: VecDeque::new(), early: None, act: 0, epoch: 0 })
    }

    /// The object's serving state, if this event belongs to its activation.
    fn serving(&mut self, o: Obj, act: Act) -> Option<&mut Serve> {
        match &mut self.st(o).phase {
            Phase::Serving(s) if s.act == act => Some(s),
            _ => None,
        }
    }

    pub fn on(&mut self, now: u64, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Tick => {
                if !self.renewing {
                    self.renewing = true;
                    self.call(Wait::LeaseRead, Call::Get(lease_key(self.node)));
                }
            }
            Event::Request { obj, req } => self.request(now, obj, req),
            Event::Ran { obj, act, req, changed } => {
                if let Some(s) = self.serving(obj, act) {
                    if s.running.remove(&req) {
                        s.batch.push(req);
                        s.dirty |= changed;
                        self.flush(obj);
                    }
                }
                // From an activation that's over: its request was failed then.
            }
            Event::Sealed { obj, act, changes } => {
                let write_once = self.write_once();
                if let Some(s) = self.serving(obj, act) {
                    s.sealing = false;
                    let (e, k) = (s.epoch, s.next_seg);
                    s.next_seg += 1;
                    self.call(Wait::Upload(obj, e), Call::Put(seg_key(obj, e, k), Val::Seg(changes), write_once));
                }
            }
            Event::Restored { obj, act, ok } => self.restored(obj, act, ok),
            Event::Reply { op, res } => {
                if let Some(w) = self.ops.remove(&op) {
                    self.reply(now, w, res);
                }
            }
        }
        std::mem::take(&mut self.out)
    }

    fn restored(&mut self, o: Obj, act: Act, ok: bool) {
        let st = self.st(o);
        if !matches!(st.phase, Phase::Starting) || st.act != act {
            return;
        }
        if !ok {
            return self.drop_obj(o);
        }
        let epoch = st.epoch;
        st.phase = Phase::Serving(Serve {
            epoch,
            act,
            next_seg: 1,
            running: HashSet::new(),
            batch: vec![],
            dirty: false,
            sealing: false,
            flight: None,
        });
        let waiting: Vec<ReqId> = st.waiting.drain(..).collect();
        for r in waiting {
            self.run(o, r);
        }
    }

    fn request(&mut self, now: u64, o: Obj, req: ReqId) {
        if matches!(self.st(o).phase, Phase::Idle) {
            if let Some(&(n, t)) = self.hints.get(&o) {
                if now.saturating_sub(t) < self.lease_ms {
                    return self.answer(req, Answer::Redirect(n));
                }
                self.hints.remove(&o);
            }
        }
        let st = self.st(o);
        match &st.phase {
            Phase::Serving(_) => self.run(o, req),
            Phase::Starting => st.waiting.push_back(req),
            Phase::Idle => {
                st.waiting.push_back(req);
                st.phase = Phase::Starting;
                self.call(Wait::ReadRec(o), Call::Get(own_key(o)));
            }
        }
    }

    fn run(&mut self, o: Obj, req: ReqId) {
        let Phase::Serving(s) = &mut self.st(o).phase else { unreachable!() };
        s.running.insert(req);
        let act = s.act;
        self.out.push(Effect::Run { obj: o, act, req });
    }

    /// Start the next upload and proof, if none is in flight.
    fn flush(&mut self, o: Obj) {
        let skip_proof = self.bugs.skip_proof;
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        if s.flight.is_some() || s.sealing || s.batch.is_empty() {
            return;
        }
        let (e, act) = (s.epoch, s.act);
        s.flight = Some(std::mem::take(&mut s.batch));
        if std::mem::take(&mut s.dirty) {
            s.sealing = true;
            self.out.push(Effect::Seal { obj: o, act });
        } else if skip_proof {
            self.proved(o, e);
        } else {
            // Reads only: nothing to upload, but they still need a proof.
            self.call(Wait::Prove(o, e), Call::Get(own_key(o)));
        }
    }

    /// Give the object up: fail what's in hand, send waiters away.
    fn drop_obj(&mut self, o: Obj) {
        let st = self.st(o);
        let mut failed = vec![];
        if let Phase::Serving(s) = std::mem::replace(&mut st.phase, Phase::Idle) {
            failed.extend(s.running);
            failed.extend(s.batch);
            failed.extend(s.flight.into_iter().flatten());
        }
        failed.extend(st.waiting.drain(..));
        st.early = None;
        self.out.push(Effect::Drop { obj: o });
        for r in failed {
            self.answer(r, Answer::Failed);
        }
    }

    fn mine(&self, e: u32) -> Val {
        Val::Rec(Rec { node: self.node, epoch: e })
    }

    fn reply(&mut self, now: u64, w: Wait, res: Res) {
        match (w, res) {
            // ---- the lease ----
            (Wait::LeaseRead, Res::Got(v)) => {
                let cur = match v {
                    Some((Val::Lease(n), _)) => n,
                    _ => 0,
                };
                let next = cur.max(self.lease) + 1;
                self.call(Wait::LeaseWrite(next), Call::Put(lease_key(self.node), Val::Lease(next), Cond::Any));
            }
            (Wait::LeaseWrite(n), Res::Put(Ok(_))) => {
                self.lease = n;
                self.renewing = false;
            }
            (Wait::LeaseRead | Wait::LeaseWrite(_), _) => self.renewing = false,

            // ---- 1. claim ----
            (Wait::ReadRec(o), Res::Got(None)) => self.cas(o, None, 1),
            (Wait::ReadRec(o), Res::Got(Some((Val::Rec(r), etag)))) => {
                if r.node == self.node {
                    // Our own node, from an earlier process: a new epoch anyway.
                    let next = if self.bugs.reuse_epoch { r.epoch } else { r.epoch + 1 };
                    self.cas(o, Some(etag), next);
                } else {
                    let n = r.node;
                    self.call(Wait::OwnerLease(o, r, etag), Call::Get(lease_key(n)));
                }
            }
            (Wait::OwnerLease(o, r, etag), Res::Got(v)) => {
                let beat = match v {
                    Some((Val::Lease(n), _)) => n,
                    _ => 0,
                };
                // A heartbeat that changed is fresh; one that hasn't changed
                // for LEASE_MS of our own time means the node is gone.
                let e = self.seen.entry(r.node).or_insert((beat, now));
                if e.0 != beat {
                    *e = (beat, now);
                }
                if now.saturating_sub(e.1) >= self.lease_ms {
                    self.cas(o, Some(etag), r.epoch + 1);
                } else {
                    self.hints.insert(o, (r.node, now));
                    let st = self.st(o);
                    st.phase = Phase::Idle;
                    let rs: Vec<ReqId> = st.waiting.drain(..).collect();
                    for q in rs {
                        self.answer(q, Answer::Redirect(r.node));
                    }
                }
            }
            (Wait::Cas(o, e), Res::Put(Ok(_))) => self.load(o, e),
            (Wait::Cas(o, _), Res::Put(Err(()))) => self.drop_obj(o),
            // A lost CAS reply: read back, never retry blind.
            (Wait::Cas(o, e), Res::Lost) => self.call(Wait::ReadBack(o, e), Call::Get(own_key(o))),
            (Wait::ReadBack(o, e), Res::Got(v)) => {
                if v.is_some_and(|(v, _)| v == self.mine(e)) {
                    self.load(o, e);
                } else {
                    self.drop_obj(o);
                }
            }

            // Wrong on purpose (`load_before_cas`): read the history first.
            (Wait::EarlyList(o, etag, e), Res::Listed(keys)) => self.history(o, e, keys, Some(etag)),
            (Wait::EarlyManifest(o, etag, e, segs), Res::Got(v)) => {
                let mut base = match v {
                    Some((Val::Manifest(m), _)) => m,
                    _ => vec![],
                };
                base.extend(segs);
                self.st(o).early = Some(base);
                self.send_cas(o, etag, e);
            }

            // ---- 2. load, then write the manifest, then restore ----
            (Wait::List(o, e), Res::Listed(keys)) => self.history(o, e, keys, None),
            (Wait::PrevManifest(o, e, segs), Res::Got(Some((Val::Manifest(m), _)))) => {
                let mut base = m;
                base.extend(segs);
                self.write_manifest(o, e, base);
            }
            (Wait::Manifest(o, e), Res::Put(Ok(_))) => {
                let segs = self.pending_base.remove(&(o, e)).unwrap_or_default();
                self.next_act += 1;
                let act = self.next_act;
                let st = self.st(o);
                st.act = act;
                st.epoch = e;
                self.out.push(Effect::Restore { obj: o, act, segs });
            }

            // ---- 3. serve: upload, then prove ----
            (Wait::Upload(o, e), Res::Put(Ok(_))) => {
                if self.bugs.skip_proof {
                    self.proved(o, e);
                } else {
                    self.call(Wait::Prove(o, e), Call::Get(own_key(o)));
                }
            }
            (Wait::Prove(o, e), Res::Got(v)) => {
                if v.is_some_and(|(v, _)| v == self.mine(e)) {
                    self.proved(o, e);
                } else {
                    self.drop_obj(o);
                }
            }
            // Bare reads are safe to repeat.
            (w @ (Wait::Prove(..) | Wait::ReadRec(_) | Wait::ReadBack(..) | Wait::List(..)), Res::Lost) => {
                let call = match &w {
                    Wait::Prove(o, _) | Wait::ReadRec(o) | Wait::ReadBack(o, _) => Call::Get(own_key(*o)),
                    Wait::List(o, _) => Call::List(obj_prefix(*o)),
                    _ => unreachable!(),
                };
                self.call(w, call);
            }
            // Anything else (a lost upload, a lost manifest, a failed
            // condition, a missing manifest): give the object up. Safe, and
            // the client retries.
            (w, _) => {
                if let Some(o) = w.obj() {
                    self.drop_obj(o);
                }
            }
        }
    }

    fn cas(&mut self, o: Obj, etag: Option<String>, next: u32) {
        if self.bugs.load_before_cas {
            self.call(Wait::EarlyList(o, etag, next), Call::List(obj_prefix(o)));
        } else {
            self.send_cas(o, etag, next);
        }
    }

    fn send_cas(&mut self, o: Obj, etag: Option<String>, next: u32) {
        let cond = match etag {
            Some(t) => Cond::IfMatch(t),
            None => Cond::IfAbsent,
        };
        let v = self.mine(next);
        self.call(Wait::Cas(o, next), Call::Put(own_key(o), v, cond));
    }

    /// After winning the CAS: read what came before. Never before the CAS:
    /// a stale owner may still be adding to it until then.
    fn load(&mut self, o: Obj, e: u32) {
        if let Some(base) = self.st(o).early.take().filter(|_| self.bugs.load_before_cas) {
            self.write_manifest(o, e, base);
            return;
        }
        self.call(Wait::List(o, e), Call::List(obj_prefix(o)));
    }

    /// From a listing: the last epoch before `e` with a manifest, plus that
    /// epoch's segments. `early`: this is the wrong, before-the-CAS version.
    fn history(&mut self, o: Obj, e: u32, keys: Vec<String>, early: Option<Option<String>>) {
        let mut manifests = vec![];
        let mut segs: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for k in &keys {
            match parse_key(o, k) {
                Some((ep, "m")) if ep < e => manifests.push(ep),
                Some((ep, part)) if ep < e && part.starts_with('s') => segs.entry(ep).or_default().push(k.clone()),
                _ => {}
            }
        }
        let start = |me: &mut Core, base: Vec<String>| match &early {
            Some(etag) => {
                me.st(o).early = Some(base);
                me.send_cas(o, etag.clone(), e)
            }
            None => me.write_manifest(o, e, base),
        };
        if self.bugs.no_manifest {
            // Wrong on purpose: every segment ever written, in epoch order.
            let base = segs.into_values().flatten().collect();
            return start(self, base);
        }
        let Some(&q) = manifests.iter().max() else { return start(self, vec![]) };
        let tail = segs.remove(&q).unwrap_or_default();
        let w = match early {
            Some(etag) => Wait::EarlyManifest(o, etag, e, tail),
            None => Wait::PrevManifest(o, e, tail),
        };
        self.call(w, Call::Get(manifest_key(o, q)));
    }

    /// Manifests and segments are write-once: a second fence, after epochs.
    /// Even a process that reused an epoch couldn't overwrite its
    /// predecessor's data. (`reuse_epoch` turns this off too, or that bug
    /// could never show.)
    fn write_once(&self) -> Cond {
        if self.bugs.reuse_epoch {
            Cond::Any
        } else {
            Cond::IfAbsent
        }
    }

    fn write_manifest(&mut self, o: Obj, e: u32, base: Vec<String>) {
        self.pending_base.insert((o, e), base.clone());
        let cond = self.write_once();
        self.call(Wait::Manifest(o, e), Call::Put(manifest_key(o, e), Val::Manifest(base), cond));
    }

    fn proved(&mut self, o: Obj, e: u32) {
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        let done = s.flight.take().unwrap_or_default();
        for r in done {
            self.answer(r, Answer::Done { epoch: e });
        }
        self.flush(o);
    }
}
