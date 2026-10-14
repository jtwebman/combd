//! The ownership decision core for D2: who serves a Durable Object when the
//! bucket is the only coordinator. It implements the protocol checked in
//! `spec/Ownership.tla`.
//!
//! Pure: `Core::on(now, event) -> effects`. No I/O, no clock of its own, no
//! randomness. The real node and the simulator (`sim.rs`) both drive it, so
//! the simulator tests the very code that will run.
//!
//! Bucket keys, per object `o`:
//! - `own/o`: the ownership record `{node, epoch}`, changed only by CAS.
//! - `obj/o/e<E>/m`: the manifest, the history the owner of epoch E started from.
//! - `obj/o/e<E>/s<K>`: segment K of epoch E, the writes of one upload.
//!
//! And per node `n`: `lease/n`, a heartbeat the node bumps. A node that hasn't
//! bumped it for `LEASE_MS`, by the *observer's* clock, can be taken over.
//! Leases are for liveness only: safety holds with any clocks.

use std::collections::{BTreeMap, HashMap, VecDeque};

pub type Obj = u32;
pub type Node = u32;
pub type W = u64;
pub type ReqId = u64;
pub type OpId = u64;

/// How long a node may go without a heartbeat before others take over.
pub const LEASE_MS: u64 = 1000;
pub const RENEW_MS: u64 = 200;

#[derive(Clone, Debug, PartialEq)]
pub struct Rec {
    pub node: Node,
    pub epoch: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Rec(Rec),
    Lease(u64),
    Manifest(Vec<W>),
    Seg(Vec<W>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cond {
    Any,
    IfAbsent,
    IfMatch(u64),
}

#[derive(Clone, Debug)]
pub enum Call {
    Get(String),
    Put(String, Val, Cond),
    /// Every key under a prefix, with its value (a list, then gets of
    /// write-once keys).
    Scan(String),
}

#[derive(Clone, Debug)]
pub enum Res {
    /// Value and version.
    Got(Option<(Val, u64)>),
    /// The new version, or `Err` if the condition failed.
    Put(Result<u64, ()>),
    Scanned(Vec<(String, Val)>),
    /// No reply in time: the call may or may not have happened.
    Lost,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Write(W),
    Read,
}

pub enum Event {
    Request { obj: Obj, req: ReqId, op: Op },
    Reply { op: OpId, res: Res },
    /// Time passes: renew our lease.
    Tick,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// Done, and proven: `history` is the object's history after it.
    Done { epoch: u32, history: Vec<W> },
    /// Another node owns it; ask there.
    Redirect(Node),
    /// Not done (or not known to be done): try again.
    Failed,
}

#[derive(Debug)]
pub enum Effect {
    Call { op: OpId, call: Call },
    Answer { req: ReqId, ans: Answer },
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

fn own_key(o: Obj) -> String {
    format!("own/{o}")
}
fn lease_key(n: Node) -> String {
    format!("lease/{n}")
}
fn manifest_key(o: Obj, e: u32) -> String {
    format!("obj/{o}/e{e}/m")
}
fn seg_key(o: Obj, e: u32, k: u32) -> String {
    format!("obj/{o}/e{e}/s{k:06}")
}

/// What a bucket call was for.
#[derive(Clone, Debug)]
enum Wait {
    ReadRec(Obj),
    /// Only with `Bugs::load_before_cas`: the history, read before the CAS.
    EarlyLoad(Obj, Option<u64>, u32),
    OwnerLease(Obj, Rec, Option<(Val, u64)>),
    Cas(Obj, u32),
    ReadBack(Obj, u32),
    Load(Obj, u32),
    Manifest(Obj, u32),
    Upload(Obj, u32),
    Prove(Obj, u32),
    LeaseRead,
    LeaseWrite(u64),
}

#[derive(Debug)]
enum Phase {
    Idle,
    /// A claim is in flight.
    Claiming,
    Serving(Serve),
}

#[derive(Debug)]
struct Serve {
    epoch: u32,
    /// Base plus every write run so far, uploaded or not.
    history: Vec<W>,
    next_seg: u32,
    /// Run, not yet sent: (request, history after it).
    batch: Vec<(ReqId, Vec<W>)>,
    /// Writes in `batch` (the next segment).
    batch_writes: Vec<W>,
    /// Sent, waiting for the upload and the proof.
    flight: Option<Vec<(ReqId, Vec<W>)>>,
}

struct ObjSt {
    phase: Phase,
    /// Requests waiting for a claim to finish.
    waiting: VecDeque<(ReqId, Op)>,
    /// History read before the CAS (only with `Bugs::load_before_cas`).
    early: Option<Vec<W>>,
}

pub struct Core {
    pub node: Node,
    bugs: Bugs,
    next_op: OpId,
    ops: HashMap<OpId, Wait>,
    objs: BTreeMap<Obj, ObjSt>,
    /// Our heartbeat: the version we last wrote, and whether a renew is out.
    lease: u64,
    renewing: bool,
    /// Other nodes' heartbeats as we last saw them: (value, when it changed, by our clock).
    seen: HashMap<Node, (u64, u64)>,
    out: Vec<Effect>,
    /// The base of a manifest being written, until it's written.
    pending_base: HashMap<(Obj, u32), Vec<W>>,
}

impl Core {
    pub fn new(node: Node, bugs: Bugs) -> Core {
        Core {
            node,
            bugs,
            next_op: 0,
            ops: HashMap::new(),
            objs: BTreeMap::new(),
            lease: 0,
            renewing: false,
            seen: HashMap::new(),
            out: vec![],
            pending_base: HashMap::new(),
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
        self.objs.entry(o).or_insert_with(|| ObjSt { phase: Phase::Idle, waiting: VecDeque::new(), early: None })
    }

    pub fn on(&mut self, now: u64, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Tick => {
                if !self.renewing {
                    self.renewing = true;
                    self.call(Wait::LeaseRead, Call::Get(lease_key(self.node)));
                }
            }
            Event::Request { obj, req, op } => self.request(obj, req, op),
            Event::Reply { op, res } => {
                if let Some(w) = self.ops.remove(&op) {
                    self.reply(now, w, res);
                }
            }
        }
        std::mem::take(&mut self.out)
    }

    fn request(&mut self, o: Obj, req: ReqId, op: Op) {
        let st = self.st(o);
        match &mut st.phase {
            Phase::Serving(_) => self.run(o, req, op),
            Phase::Claiming => st.waiting.push_back((req, op)),
            Phase::Idle => {
                st.waiting.push_back((req, op));
                st.phase = Phase::Claiming;
                self.call(Wait::ReadRec(o), Call::Get(own_key(o)));
            }
        }
    }

    /// Run one request on an object we serve, then send what's ready.
    fn run(&mut self, o: Obj, req: ReqId, op: Op) {
        let Phase::Serving(s) = &mut self.st(o).phase else { unreachable!() };
        if let Op::Write(w) = op {
            s.history.push(w);
            s.batch_writes.push(w);
        }
        let h = s.history.clone();
        s.batch.push((req, h));
        self.flush(o);
    }

    /// Start the next upload and proof, if none is in flight.
    fn flush(&mut self, o: Obj) {
        let skip_proof = self.bugs.skip_proof;
        let write_once = self.write_once();
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        if s.flight.is_some() || s.batch.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut s.batch);
        let writes = std::mem::take(&mut s.batch_writes);
        let (e, k) = (s.epoch, s.next_seg);
        s.flight = Some(batch);
        if writes.is_empty() && !skip_proof {
            // Reads only: nothing to upload, but they still need a proof.
            self.call(Wait::Prove(o, e), Call::Get(own_key(o)));
        } else if writes.is_empty() {
            self.proved(o, e);
        } else {
            s.next_seg += 1;
            self.call(Wait::Upload(o, e), Call::Put(seg_key(o, e, k), Val::Seg(writes), write_once));
        }
    }

    /// Give the object up: fail what's in hand, send waiters away.
    fn drop_obj(&mut self, o: Obj) {
        let st = self.st(o);
        let mut failed = vec![];
        if let Phase::Serving(s) = std::mem::replace(&mut st.phase, Phase::Idle) {
            failed.extend(s.batch.into_iter().map(|(r, _)| r));
            failed.extend(s.flight.into_iter().flatten().map(|(r, _)| r));
        }
        failed.extend(st.waiting.drain(..).map(|(r, _)| r));
        st.early = None;
        for r in failed {
            self.answer(r, Answer::Failed);
        }
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
            (Wait::ReadRec(o), Res::Got(Some((Val::Rec(r), ver)))) => {
                if r.node == self.node {
                    // Our own node, from an earlier process: a new epoch anyway.
                    let next = if self.bugs.reuse_epoch { r.epoch } else { r.epoch + 1 };
                    self.cas(o, Some(ver), next);
                } else {
                    let owner = r.clone();
                    self.call(Wait::OwnerLease(o, owner, Some((Val::Rec(r.clone()), ver))), Call::Get(lease_key(r.node)));
                }
            }
            (Wait::OwnerLease(o, r, Some((_, ver))), Res::Got(v)) => {
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
                if now.saturating_sub(e.1) >= LEASE_MS {
                    self.cas(o, Some(ver), r.epoch + 1);
                } else {
                    let st = self.st(o);
                    st.phase = Phase::Idle;
                    let rs: Vec<ReqId> = st.waiting.drain(..).map(|(r, _)| r).collect();
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
                if v.is_some_and(|(v, _)| v == Val::Rec(Rec { node: self.node, epoch: e })) {
                    self.load(o, e);
                } else {
                    self.drop_obj(o);
                }
            }

            (Wait::EarlyLoad(o, ver, e), Res::Scanned(kv)) => {
                let h = self.history(o, e, &kv);
                self.st(o).early = Some(h);
                self.send_cas(o, ver, e);
            }

            // ---- 2. load, then write the manifest ----
            (Wait::Load(o, e), Res::Scanned(kv)) => {
                let base = self.history(o, e, &kv);
                self.write_manifest(o, e, base);
            }
            (Wait::Manifest(o, e), Res::Put(Ok(_))) => self.serve(o, e),

            // ---- 3. serve: upload, then prove ----
            (Wait::Upload(o, e), Res::Put(Ok(_))) => {
                if self.bugs.skip_proof {
                    self.proved(o, e);
                } else {
                    self.call(Wait::Prove(o, e), Call::Get(own_key(o)));
                }
            }
            (Wait::Prove(o, e), Res::Got(v)) => {
                if v.is_some_and(|(v, _)| v == Val::Rec(Rec { node: self.node, epoch: e })) {
                    self.proved(o, e);
                } else {
                    self.drop_obj(o);
                }
            }
            // A lost proof read is safe to repeat.
            (Wait::Prove(o, e), Res::Lost) => self.call(Wait::Prove(o, e), Call::Get(own_key(o))),
            // A bare read is safe to repeat too.
            (Wait::ReadRec(o), Res::Lost) => self.call(Wait::ReadRec(o), Call::Get(own_key(o))),
            (Wait::ReadBack(o, e), Res::Lost) => self.call(Wait::ReadBack(o, e), Call::Get(own_key(o))),
            // Anything else (a lost upload, a lost manifest, a failed
            // condition): give the object up. Safe, and the client retries.
            (w, _) => {
                let o = match w {
                    Wait::ReadRec(o)
                    | Wait::EarlyLoad(o, ..)
                    | Wait::OwnerLease(o, ..)
                    | Wait::Cas(o, _)
                    | Wait::ReadBack(o, _)
                    | Wait::Load(o, _)
                    | Wait::Manifest(o, _)
                    | Wait::Upload(o, _)
                    | Wait::Prove(o, _) => o,
                    Wait::LeaseRead | Wait::LeaseWrite(_) => return,
                };
                self.drop_obj(o);
            }
        }
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

    fn cas(&mut self, o: Obj, ver: Option<u64>, next: u32) {
        if self.bugs.load_before_cas {
            // Wrong on purpose: read the history before winning the CAS.
            self.call(Wait::EarlyLoad(o, ver, next), Call::Scan(format!("obj/{o}/")));
        } else {
            self.send_cas(o, ver, next);
        }
    }

    fn send_cas(&mut self, o: Obj, ver: Option<u64>, next: u32) {
        let cond = match ver {
            Some(v) => Cond::IfMatch(v),
            None => Cond::IfAbsent,
        };
        self.call(Wait::Cas(o, next), Call::Put(own_key(o), Val::Rec(Rec { node: self.node, epoch: next }), cond));
    }

    /// After winning the CAS: read what came before. Never before the CAS:
    /// a stale owner may still be adding to it until then.
    fn load(&mut self, o: Obj, e: u32) {
        if let Some(h) = self.st(o).early.take().filter(|_| self.bugs.load_before_cas) {
            self.write_manifest(o, e, h);
            return;
        }
        self.call(Wait::Load(o, e), Call::Scan(format!("obj/{o}/")));
    }

    fn write_manifest(&mut self, o: Obj, e: u32, base: Vec<W>) {
        self.pending_base.insert((o, e), base.clone());
        let cond = self.write_once();
        self.call(Wait::Manifest(o, e), Call::Put(manifest_key(o, e), Val::Manifest(base), cond));
    }

    /// The history the owner of epoch `e` starts from: the last epoch before
    /// it with a manifest, plus that epoch's segments.
    fn history(&self, o: Obj, e: u32, kv: &[(String, Val)]) -> Vec<W> {
        let prefix = format!("obj/{o}/e");
        let mut manifests: BTreeMap<u32, Vec<W>> = BTreeMap::new();
        let mut segs: BTreeMap<(u32, String), Vec<W>> = BTreeMap::new();
        for (k, v) in kv {
            let Some(rest) = k.strip_prefix(&prefix) else { continue };
            let Some((ep, part)) = rest.split_once('/') else { continue };
            let Ok(ep) = ep.parse::<u32>() else { continue };
            match v {
                Val::Manifest(h) => {
                    manifests.insert(ep, h.clone());
                }
                Val::Seg(ws) => {
                    segs.insert((ep, part.to_string()), ws.clone());
                }
                _ => {}
            }
        }
        if self.bugs.no_manifest {
            // Wrong on purpose: every segment ever written, in epoch order.
            return segs.into_iter().filter(|((ep, _), _)| *ep < e).flat_map(|(_, w)| w).collect();
        }
        let Some((&q, base)) = manifests.range(..e).next_back() else { return vec![] };
        let mut h = base.clone();
        h.extend(segs.into_iter().filter(|((ep, _), _)| *ep == q).flat_map(|(_, w)| w));
        h
    }

    fn serve(&mut self, o: Obj, e: u32) {
        let base = self.pending_base.remove(&(o, e)).unwrap_or_default();
        let st = self.st(o);
        st.phase = Phase::Serving(Serve { epoch: e, history: base, next_seg: 1, batch: vec![], batch_writes: vec![], flight: None });
        let waiting: Vec<(ReqId, Op)> = st.waiting.drain(..).collect();
        for (r, op) in waiting {
            self.run(o, r, op);
        }
    }

    fn proved(&mut self, o: Obj, e: u32) {
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        let done = s.flight.take().unwrap_or_default();
        for (r, h) in done {
            self.answer(r, Answer::Done { epoch: e, history: h });
        }
        self.flush(o);
    }
}
