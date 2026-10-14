//! The ownership decision core: who serves a Durable Object when the bucket
//! is the only coordinator (D2, `spec/Ownership.tla`), and how writes get
//! fast with peers (D3, `spec/Replication.tla`).
//!
//! Pure: `Core::on(now, event) -> effects`. No I/O, no clock of its own, no
//! randomness. The real node and the simulator (`sim.rs`) both drive it, so
//! the simulator tests the very code that runs.
//!
//! The core never touches an object's data. It tells the executor what to do
//! and when (run a request, seal an entry, restore from segments, drop the
//! object), and the executor owns the data: a list of writes in the
//! simulator, a cell and SQLite in a real node.
//!
//! Each seal is one log entry. Entries are acknowledged one of two ways:
//! - D3: every peer of the object has it (`PeerStore`, one per node, on disk).
//! - D2: the bucket has it, and the record still names us (the proof).
//!
//! With no peers (`replicas` 0) only the second way exists, which is D2. With
//! peers, the first is the fast path and the second keeps writes going (at
//! bucket speed) while a peer is down. Entries are uploaded in the background
//! either way, and peers drop what the bucket has.
//!
//! Bucket keys, per object `o`:
//! - `own/o`: the ownership record `{node, epoch, sealed, peers}`, changed
//!   only by CAS.
//! - `obj/o/e<E>/m`: the manifest: the segment keys the owner of epoch E
//!   started from, in order.
//! - `obj/o/e<E>/s<N>`: an upload of epoch E: its entries up to entry N.
//! - `obj/o/e<E>/t`: what the peers had of the previous epoch past its uploads.
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
    /// The owner uploaded everything and stepped down: the next one needn't fence.
    #[serde(default)]
    pub sealed: bool,
    /// This epoch's peers. The set changes only across a sealed epoch, so
    /// fencing one peer stops every older epoch.
    #[serde(default)]
    pub peers: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Rec(Rec),
    Lease(u64),
    /// Segment keys, in order.
    Manifest(Vec<String>),
    /// Entries, framed (`frame`).
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

/// Entries in one segment: `[u32 length][bytes]...`, little-endian.
pub fn frame<'a>(ents: impl IntoIterator<Item = &'a Vec<u8>>) -> Vec<u8> {
    let mut b = vec![];
    for e in ents {
        b.extend_from_slice(&(e.len() as u32).to_le_bytes());
        b.extend_from_slice(e);
    }
    b
}

pub fn unframe(mut b: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut out = vec![];
    while !b.is_empty() {
        let n = u32::from_le_bytes(b.get(..4)?.try_into().unwrap()) as usize;
        out.push(b.get(4..4 + n)?.to_vec());
        b = &b[4 + n..];
    }
    Some(out)
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

/// A call to a node's peer store.
#[derive(Clone, Debug, PartialEq)]
pub enum PeerCall {
    /// Keep entries `from..` of `epoch` (the peer skips what it has), then
    /// drop entries up to `trunc`: the bucket has them. Refused once fenced
    /// at `epoch` or above.
    Append { obj: Obj, epoch: u32, from: u64, ents: Vec<Vec<u8>>, trunc: u64 },
    /// Refuse every epoch below `epoch` from now on, and hand back every tail.
    Fence { obj: Obj, epoch: u32 },
}

/// What a peer holds of one epoch: entries `from + 1 ..`.
#[derive(Clone, Debug, PartialEq)]
pub struct Tail {
    pub epoch: u32,
    pub from: u64,
    pub ents: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PeerRes {
    /// How many entries of the epoch the peer has (kept or dropped).
    Have(u64),
    Refused,
    Tails(Vec<Tail>),
}

#[derive(Clone, Debug)]
pub enum Res {
    /// Value and etag.
    Got(Option<(Val, String)>),
    /// The new etag, or `Err` if the condition failed.
    Put(Result<String, ()>),
    Listed(Vec<String>),
    Peer(PeerRes),
    /// No reply in time: the call may or may not have happened.
    Lost,
}

pub enum Event {
    Request { obj: Obj, req: ReqId },
    /// The executor ran a request; `changed`: it wrote something.
    Ran { obj: Obj, act: Act, req: ReqId, changed: bool },
    /// The changes since the last seal: the next entry.
    Sealed { obj: Obj, act: Act, changes: Vec<u8> },
    /// The executor rebuilt the object from its segments (or couldn't).
    Restored { obj: Obj, act: Act, ok: bool },
    Reply { op: OpId, res: Res },
    /// Time passes: renew our lease, retry peers that went quiet.
    Tick,
    /// Let an idle object go (eviction). Ignored unless nothing is running,
    /// waiting, or not yet uploaded. The next request claims it again.
    Release { obj: Obj },
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
    /// A call to `node`'s peer store (maybe our own).
    Peer { op: OpId, node: Node, call: PeerCall },
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
    /// D3: acknowledge when one peer has it, not all.
    pub ack_one: bool,
    /// D3: never fence.
    pub no_fence: bool,
    /// D3: peers drop entries before the bucket has them.
    pub trunc_early: bool,
    /// D3: seal before everything is uploaded.
    pub seal_early: bool,
}

/// A node's peer store: the entries it keeps for other nodes' objects. The
/// real node keeps it on disk; it must survive the node's restarts.
#[derive(Default)]
pub struct PeerStore {
    objs: HashMap<Obj, PeerObj>,
}

#[derive(Default)]
struct PeerObj {
    fenced: u32,
    /// Per epoch: how many entries were dropped, and the rest.
    logs: BTreeMap<u32, (u64, Vec<Vec<u8>>)>,
}

impl PeerStore {
    pub fn handle(&mut self, call: PeerCall) -> PeerRes {
        match call {
            PeerCall::Append { obj, epoch, from, ents, trunc } => {
                let po = self.objs.entry(obj).or_default();
                if epoch <= po.fenced || from == 0 {
                    return PeerRes::Refused;
                }
                // An epoch that appends has its manifest written, and that
                // manifest holds everything older: those logs aren't needed.
                po.logs.retain(|&e, _| e >= epoch);
                let (dropped, log) = po.logs.entry(epoch).or_default();
                let len = *dropped + log.len() as u64;
                if from > len + 1 {
                    if from - 1 > trunc {
                        // A gap the bucket doesn't cover: say what we have.
                        return PeerRes::Have(len);
                    }
                    // The bucket has everything before `from`.
                    *dropped = from - 1;
                    log.clear();
                }
                let len = *dropped + log.len() as u64;
                log.extend(ents.into_iter().skip((len + 1 - from) as usize));
                let len = *dropped + log.len() as u64;
                let cut = trunc.min(len).saturating_sub(*dropped);
                log.drain(..cut as usize);
                *dropped += cut;
                PeerRes::Have(len)
            }
            PeerCall::Fence { obj, epoch } => {
                let po = self.objs.entry(obj).or_default();
                po.fenced = po.fenced.max(epoch.saturating_sub(1));
                PeerRes::Tails(po.logs.iter().map(|(&e, (d, l))| Tail { epoch: e, from: *d, ents: l.clone() }).collect())
            }
        }
    }

    /// Calls that rebuild this store from nothing: each object's logs, newest
    /// epoch first (an append drops older epochs, not newer), then its fence.
    pub fn snapshot(&self) -> Vec<PeerCall> {
        let mut out = vec![];
        for (&obj, po) in &self.objs {
            for (&epoch, (d, l)) in po.logs.iter().rev() {
                out.push(PeerCall::Append { obj, epoch, from: d + 1, ents: l.clone(), trunc: 0 });
            }
            if po.fenced > 0 {
                out.push(PeerCall::Fence { obj, epoch: po.fenced + 1 });
            }
        }
        out
    }
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
fn seg_key(o: Obj, e: u32, upto: u64) -> String {
    format!("obj/{o:032x}/e{e:08}/s{upto:012}")
}
fn tail_key(o: Obj, e: u32) -> String {
    format!("obj/{o:032x}/e{e:08}/t")
}
/// The epoch and the part (`m`, `t`, or `s...`) of an object key.
pub fn parse_key(o: Obj, k: &str) -> Option<(u32, &str)> {
    let rest = k.strip_prefix(&format!("obj/{o:032x}/e"))?;
    let (e, part) = rest.split_once('/')?;
    Some((e.parse().ok()?, part))
}

/// splitmix64's finisher, for rendezvous placement.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// What a call was for.
#[derive(Clone, Debug)]
enum Wait {
    ReadRec(Obj),
    OwnerLease(Obj, Rec, String),
    Cas(Obj, u32),
    ReadBack(Obj, u32),
    /// Only with `Bugs::load_before_cas`: the history, read before the CAS.
    EarlyList(Obj, Option<String>, u32),
    EarlyManifest(Obj, Option<String>, u32, Vec<String>),
    Fence(Obj, u32),
    List(Obj, u32),
    /// Reading the manifest of an earlier epoch q: its uploads, how many
    /// entries they hold, and q.
    PrevManifest(Obj, u32, Vec<String>, u64, u32),
    TailSeg(Obj, u32, Vec<String>),
    Manifest(Obj, u32),
    /// Serving: an upload up to entry N (a retry?), a peer append (peer
    /// index, round), a proof (round, entries uploaded).
    Upload(Obj, u32, u64, bool),
    Append(Obj, u32, usize, u64),
    Prove(Obj, u32, u64, u64),
    /// Marking the record sealed; nothing waits on it.
    SealRec,
    LeaseRead,
    LeaseWrite(u64),
}

impl Wait {
    fn obj(&self) -> Option<Obj> {
        use Wait::*;
        match self {
            ReadRec(o) | OwnerLease(o, ..) | Cas(o, _) | ReadBack(o, _) | EarlyList(o, ..) | EarlyManifest(o, ..) | Fence(o, _)
            | List(o, _) | PrevManifest(o, ..) | TailSeg(o, ..) | Manifest(o, _) | Upload(o, ..) | Append(o, ..) | Prove(o, ..) => {
                Some(*o)
            }
            SealRec | LeaseRead | LeaseWrite(_) => None,
        }
    }
}

#[derive(Debug)]
enum Phase {
    Idle,
    /// Claiming, fencing, loading, writing the manifest, or restoring.
    Starting,
    Serving(Serve),
}

#[derive(Debug)]
struct PeerSt {
    node: Node,
    /// Entries it has, and the last round it confirmed.
    have: u64,
    round: u64,
    busy: bool,
    /// Its last call was lost. It stays down until one gets through; a tick
    /// allows one retry.
    down: bool,
    retry: bool,
}

/// Requests waiting to be acknowledged: they need entries up to `need`, and
/// a confirmation sent after they ran (`round` or later).
#[derive(Debug)]
struct Pend {
    need: u64,
    round: u64,
    /// When it started waiting (our clock).
    since: u64,
    reqs: Vec<ReqId>,
}

#[derive(Debug)]
struct Serve {
    epoch: u32,
    act: Act,
    /// The record's etag after our CAS, for sealing it.
    etag: String,
    peers: Vec<PeerSt>,
    /// Entries made this epoch, and how many the bucket has.
    made: u64,
    uploaded: u64,
    /// Entries `uploaded + 1 ..= made`.
    ents: VecDeque<Vec<u8>>,
    uploading: bool,
    /// D3: an upload was lost. Wait for a tick, then send the very same one
    /// again (up to this entry): it may have landed, and two segments must
    /// never overlap.
    upload_wait: bool,
    retry_upto: Option<u64>,
    /// Rounds count up; every new `Pend` gets the next one.
    round: u64,
    proving: bool,
    /// The last proof: (round, entries uploaded).
    proof: (u64, u64),
    /// Sent to the executor, not yet run.
    running: HashSet<ReqId>,
    /// Run, not yet in an entry.
    batch: Vec<ReqId>,
    dirty: bool,
    /// Asked for a seal for these; waiting for the changes.
    sealing: Option<Vec<ReqId>>,
    pends: VecDeque<Pend>,
}

impl Serve {
    fn idle(&self, seal_early: bool) -> bool {
        self.running.is_empty()
            && self.batch.is_empty()
            && self.sealing.is_none()
            && self.pends.is_empty()
            && (seal_early || (!self.uploading && self.uploaded == self.made))
    }
}

struct ObjSt {
    phase: Phase,
    /// Requests waiting for the object to start.
    waiting: VecDeque<ReqId>,
    /// Segment keys read before the CAS (only with `Bugs::load_before_cas`).
    early: Option<Vec<String>>,
    /// The record we're claiming over, and what we claim: our peers.
    prev: Option<Rec>,
    peers: Vec<Node>,
    etag: String,
    /// Fencing epoch E: calls still out. Then the tails from the first reply.
    fencing: Option<(u32, usize)>,
    tails: Vec<Tail>,
    /// The activation being started, and its epoch.
    act: Act,
    epoch: u32,
}

/// Counts, for tests and stats.
#[derive(Default, Debug, Clone, Copy)]
pub struct CoreStats {
    pub peer_acks: u64,
    pub bucket_acks: u64,
    pub fences: u64,
    pub tail_entries: u64,
}

pub struct Core {
    pub node: Node,
    /// Whether this core keeps the node's lease alive (one per node does).
    pub renews: bool,
    bugs: Bugs,
    lease_ms: u64,
    /// Every node, and how many peers each object gets (0: D2).
    fleet: Vec<Node>,
    replicas: usize,
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
    pub stats: CoreStats,
    /// The time of the event being handled.
    now: u64,
    out: Vec<Effect>,
}

impl Core {
    pub fn new(node: Node, bugs: Bugs) -> Core {
        Core::with_lease(node, bugs, LEASE_MS)
    }

    pub fn with_lease(node: Node, bugs: Bugs, lease_ms: u64) -> Core {
        Core {
            node,
            renews: true,
            bugs,
            lease_ms,
            fleet: vec![],
            replicas: 0,
            next_op: 0,
            next_act: 0,
            ops: HashMap::new(),
            objs: BTreeMap::new(),
            pending_base: HashMap::new(),
            lease: 0,
            renewing: false,
            seen: HashMap::new(),
            hints: HashMap::new(),
            stats: CoreStats::default(),
            now: 0,
            out: vec![],
        }
    }

    /// D3: give each object `replicas` peers out of `fleet`. Every node must
    /// use the same fleet list and count.
    /// ponytail: a fixed fleet. A changed one only takes effect across a
    /// sealed epoch (an evicted object); live membership comes with SWIM.
    pub fn replicate(mut self, fleet: Vec<Node>, replicas: usize) -> Core {
        self.fleet = fleet;
        self.replicas = replicas;
        self
    }

    /// Rendezvous hashing: the `replicas` nodes that score highest for `o`.
    fn peers_of(&self, o: Obj) -> Vec<Node> {
        let mut f = self.fleet.clone();
        f.sort_by_key(|&n| std::cmp::Reverse(mix(o as u64 ^ mix((o >> 64) as u64 ^ mix(n as u64)))));
        f.truncate(self.replicas);
        f
    }

    fn call(&mut self, w: Wait, c: Call) {
        self.next_op += 1;
        self.ops.insert(self.next_op, w);
        self.out.push(Effect::Call { op: self.next_op, call: c });
    }

    fn peer(&mut self, w: Wait, node: Node, c: PeerCall) {
        self.next_op += 1;
        self.ops.insert(self.next_op, w);
        self.out.push(Effect::Peer { op: self.next_op, node, call: c });
    }

    fn answer(&mut self, req: ReqId, ans: Answer) {
        self.out.push(Effect::Answer { req, ans });
    }

    fn st(&mut self, o: Obj) -> &mut ObjSt {
        self.objs.entry(o).or_insert_with(|| ObjSt {
            phase: Phase::Idle,
            waiting: VecDeque::new(),
            early: None,
            prev: None,
            peers: vec![],
            etag: String::new(),
            fencing: None,
            tails: vec![],
            act: 0,
            epoch: 0,
        })
    }

    /// The object's serving state, if this event belongs to its activation.
    fn serving(&mut self, o: Obj, act: Act) -> Option<&mut Serve> {
        match &mut self.st(o).phase {
            Phase::Serving(s) if s.act == act => Some(s),
            _ => None,
        }
    }

    /// The same, by epoch: for replies to calls made while serving.
    fn serving_in(&mut self, o: Obj, e: u32) -> Option<&mut Serve> {
        match &mut self.st(o).phase {
            Phase::Serving(s) if s.epoch == e => Some(s),
            _ => None,
        }
    }

    pub fn on(&mut self, now: u64, ev: Event) -> Vec<Effect> {
        self.now = now;
        match ev {
            Event::Tick => {
                if self.renews && !self.renewing {
                    self.renewing = true;
                    self.call(Wait::LeaseRead, Call::Get(lease_key(self.node)));
                }
                let live: Vec<Obj> = self.objs.iter().filter(|(_, st)| matches!(st.phase, Phase::Serving(_))).map(|(&o, _)| o).collect();
                let lease_ms = self.lease_ms;
                for o in live {
                    let mut late = vec![];
                    if let Phase::Serving(s) = &mut self.st(o).phase {
                        s.peers.iter_mut().for_each(|p| p.retry = p.down);
                        s.upload_wait = false;
                        // Unconfirmed for a whole lease: we're likely cut
                        // off. Don't hold the clients; "retry" sends them
                        // elsewhere. (It may still commit: that's allowed.)
                        while s.pends.front().is_some_and(|f| now.saturating_sub(f.since) > lease_ms) {
                            late.extend(s.pends.pop_front().unwrap().reqs);
                        }
                    }
                    for r in late {
                        self.answer(r, Answer::Failed);
                    }
                    self.kick(o);
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
                if let Some(s) = self.serving(obj, act) {
                    let reqs = s.sealing.take().unwrap_or_default();
                    s.made += 1;
                    s.ents.push_back(changes);
                    s.round += 1;
                    s.pends.push_back(Pend { need: s.made, round: s.round, since: now, reqs });
                    self.flush(obj);
                    self.kick(obj);
                }
            }
            Event::Restored { obj, act, ok } => self.restored(obj, act, ok),
            Event::Release { obj } => {
                let seal_early = self.bugs.seal_early;
                let st = self.st(obj);
                let idle = matches!(&st.phase, Phase::Serving(s) if s.idle(seal_early));
                if idle && st.waiting.is_empty() {
                    let Phase::Serving(s) = std::mem::replace(&mut st.phase, Phase::Idle) else { unreachable!() };
                    self.out.push(Effect::Drop { obj });
                    // Everything is uploaded: mark the epoch sealed, so the
                    // next owner can skip the fence (and needn't wait for
                    // our lease). If this fails, it fences: also fine.
                    if !s.peers.is_empty() {
                        let peers = s.peers.iter().map(|p| p.node).collect();
                        let r = Rec { node: self.node, epoch: s.epoch, sealed: true, peers };
                        self.call(Wait::SealRec, Call::Put(own_key(obj), Val::Rec(r), Cond::IfMatch(s.etag)));
                    }
                }
            }
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
        let peers = st.peers.iter().map(|&node| PeerSt { node, have: 0, round: 0, busy: false, down: false, retry: false }).collect();
        st.phase = Phase::Serving(Serve {
            epoch: st.epoch,
            act,
            etag: std::mem::take(&mut st.etag),
            peers,
            made: 0,
            uploaded: 0,
            ents: VecDeque::new(),
            uploading: false,
            upload_wait: false,
            retry_upto: None,
            round: 0,
            proving: false,
            proof: (0, 0),
            running: HashSet::new(),
            batch: vec![],
            dirty: false,
            sealing: None,
            pends: VecDeque::new(),
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

    /// Turn what has run into an entry (a seal), or, for reads only, into a
    /// wait for the next confirmation.
    fn flush(&mut self, o: Obj) {
        let now = self.now;
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        if s.sealing.is_some() || s.batch.is_empty() {
            return;
        }
        let reqs = std::mem::take(&mut s.batch);
        if std::mem::take(&mut s.dirty) {
            s.sealing = Some(reqs);
            let act = s.act;
            self.out.push(Effect::Seal { obj: o, act });
        } else {
            s.round += 1;
            s.pends.push_back(Pend { need: s.made, round: s.round, since: now, reqs });
            self.kick(o);
        }
    }

    /// Start whatever calls are due: peer appends, an upload, a proof.
    fn kick(&mut self, o: Obj) {
        let (trunc_early, skip_proof) = (self.bugs.trunc_early, self.bugs.skip_proof);
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        let e = s.epoch;
        let mut peer_calls = vec![];
        for (i, p) in s.peers.iter_mut().enumerate() {
            if p.busy || (p.down && !p.retry) || (p.have >= s.made && p.round >= s.round) {
                continue;
            }
            p.retry = false;
            // A peer behind the uploads skips ahead: the bucket has those.
            let from = p.have.max(s.uploaded) + 1;
            // At most about 4 MB per call; the rest goes on the next one.
            let mut size = 0;
            let ents = s
                .ents
                .iter()
                .skip((from - s.uploaded - 1) as usize)
                .take_while(|e| {
                    size += e.len();
                    size <= 4 << 20 || size == e.len()
                })
                .cloned()
                .collect();
            let trunc = if trunc_early { s.made } else { s.uploaded };
            p.busy = true;
            peer_calls.push((Wait::Append(o, e, i, s.round), p.node, PeerCall::Append { obj: o, epoch: e, from, ents, trunc }));
        }
        let mut calls = vec![];
        if !s.uploading && !s.upload_wait && s.uploaded < s.made {
            s.uploading = true;
            let upto = s.retry_upto.unwrap_or(s.made);
            let seg = Val::Seg(frame(s.ents.iter().take((upto - s.uploaded) as usize)));
            let retry = s.retry_upto.is_some();
            calls.push((Wait::Upload(o, e, upto, retry), Call::Put(seg_key(o, e, upto), seg, Cond::IfAbsent)));
        }
        // The bucket way: only with no peers, or one that's down.
        if let Some(f) = s.pends.front() {
            let unproven = s.proof.0 < f.round || s.proof.1 < f.need;
            if !s.proving && unproven && f.need <= s.uploaded && (s.peers.is_empty() || s.peers.iter().any(|p| p.down)) {
                if skip_proof {
                    s.proof = (s.round, s.uploaded);
                } else {
                    s.proving = true;
                    calls.push((Wait::Prove(o, e, s.round, s.uploaded), Call::Get(own_key(o))));
                }
            }
        }
        for (w, node, c) in peer_calls {
            self.peer(w, node, c);
        }
        for (w, c) in calls {
            self.call(w, c);
        }
        self.settle(o);
    }

    /// Acknowledge every waiting batch that's confirmed.
    fn settle(&mut self, o: Obj) {
        let ack_one = self.bugs.ack_one;
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        let e = s.epoch;
        let (mut done, mut by) = (vec![], (0, 0));
        while let Some(f) = s.pends.front() {
            let ok = |p: &PeerSt| p.round >= f.round && p.have >= f.need;
            let by_peers = !s.peers.is_empty() && if ack_one { s.peers.iter().any(ok) } else { s.peers.iter().all(ok) };
            let by_bucket = s.proof.0 >= f.round && s.proof.1 >= f.need;
            if !by_peers && !by_bucket {
                break;
            }
            if by_peers {
                by.0 += 1;
            } else {
                by.1 += 1;
            }
            done.extend(s.pends.pop_front().unwrap().reqs);
        }
        self.stats.peer_acks += by.0;
        self.stats.bucket_acks += by.1;
        for r in done {
            self.answer(r, Answer::Done { epoch: e });
        }
    }

    /// Give the object up: fail what's in hand, send waiters away.
    fn drop_obj(&mut self, o: Obj) {
        let st = self.st(o);
        let mut failed = vec![];
        if let Phase::Serving(s) = std::mem::replace(&mut st.phase, Phase::Idle) {
            failed.extend(s.running);
            failed.extend(s.batch);
            failed.extend(s.sealing.into_iter().flatten());
            failed.extend(s.pends.into_iter().flat_map(|p| p.reqs));
        }
        failed.extend(st.waiting.drain(..));
        st.early = None;
        st.fencing = None;
        st.tails.clear();
        self.out.push(Effect::Drop { obj: o });
        for r in failed {
            self.answer(r, Answer::Failed);
        }
    }

    fn is_mine(&self, v: &Val, e: u32) -> bool {
        matches!(v, Val::Rec(r) if r.node == self.node && r.epoch == e && !r.sealed)
    }

    /// Must we fence the peers of the epoch we take over from? Not if there
    /// was none, it had no peers (D2), or it was sealed.
    fn must_fence(&self, prev: &Option<Rec>) -> bool {
        !self.bugs.no_fence && prev.as_ref().is_some_and(|r| r.epoch > 0 && !r.sealed && !r.peers.is_empty())
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
            (Wait::SealRec, _) => {}

            // ---- 1. claim ----
            (Wait::ReadRec(o), Res::Got(None)) => {
                self.st(o).prev = None;
                self.cas(o, None, 1);
            }
            (Wait::ReadRec(o), Res::Got(Some((Val::Rec(r), etag)))) => {
                self.st(o).prev = Some(r.clone());
                if r.node == self.node {
                    // Our own node, from an earlier process: a new epoch anyway.
                    let next = if self.bugs.reuse_epoch { r.epoch } else { r.epoch + 1 };
                    self.cas(o, Some(etag), next);
                } else if r.sealed {
                    // It stepped down: no lease to wait out.
                    self.cas(o, Some(etag), r.epoch + 1);
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
            (Wait::Cas(o, e), Res::Put(Ok(etag))) => {
                self.st(o).etag = etag;
                self.load(o, e);
            }
            (Wait::Cas(o, _), Res::Put(Err(()))) => self.drop_obj(o),
            // A lost CAS reply: read back, never retry blind.
            (Wait::Cas(o, e), Res::Lost) => self.call(Wait::ReadBack(o, e), Call::Get(own_key(o))),
            (Wait::ReadBack(o, e), Res::Got(v)) => match v {
                Some((v, etag)) if self.is_mine(&v, e) => {
                    self.st(o).etag = etag;
                    self.load(o, e);
                }
                _ => self.drop_obj(o),
            },

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

            // ---- 2. fence, then load, then write the manifest, then restore ----
            (Wait::Fence(o, e), res) => {
                let st = self.st(o);
                let Some((fe, out)) = st.fencing else { return };
                if fe != e {
                    return;
                }
                match res {
                    Res::Peer(PeerRes::Tails(tails)) => {
                        st.fencing = None;
                        st.tails = tails;
                        self.stats.fences += 1;
                        self.call(Wait::List(o, e), Call::List(obj_prefix(o)));
                    }
                    // Every peer lost: give up; the next request starts over.
                    _ if out == 1 => self.drop_obj(o),
                    _ => st.fencing = Some((e, out - 1)),
                }
            }
            (Wait::List(o, e), Res::Listed(keys)) => self.history(o, e, keys, None),
            (Wait::PrevManifest(o, e, segs, uploaded, q), Res::Got(Some((Val::Manifest(m), _)))) => {
                let mut base = m;
                base.extend(segs);
                // What the fenced peer had of q past its uploads.
                let more: Vec<Vec<u8>> = std::mem::take(&mut self.st(o).tails)
                    .into_iter()
                    .find(|t| t.epoch == q)
                    .filter(|t| t.from <= uploaded)
                    .map(|t| t.ents.into_iter().skip((uploaded - t.from) as usize).collect())
                    .unwrap_or_default();
                if more.is_empty() {
                    return self.write_manifest(o, e, base);
                }
                self.stats.tail_entries += more.len() as u64;
                let cond = self.write_once();
                self.call(Wait::TailSeg(o, e, base), Call::Put(tail_key(o, e), Val::Seg(frame(&more)), cond));
            }
            (Wait::TailSeg(o, e, mut base), Res::Put(Ok(_))) => {
                base.push(tail_key(o, e));
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

            // ---- 3. serve ----
            // On a retry, "already there" means the lost one landed: only
            // this epoch's owner writes its keys.
            (Wait::Upload(o, e, upto, retry), Res::Put(r)) if r.is_ok() || retry => {
                let Some(s) = self.serving_in(o, e) else { return };
                s.uploading = false;
                s.retry_upto = None;
                s.ents.drain(..(upto - s.uploaded) as usize);
                s.uploaded = upto;
                self.kick(o);
            }
            (Wait::Append(o, e, i, round), res) => {
                let Some(s) = self.serving_in(o, e) else { return };
                let p = &mut s.peers[i];
                p.busy = false;
                match res {
                    Res::Peer(PeerRes::Have(n)) => {
                        // One call in flight per peer: its answer is the truth.
                        p.have = n;
                        p.round = p.round.max(round);
                        p.down = false;
                    }
                    // Fenced: a newer owner has it.
                    Res::Peer(PeerRes::Refused) => return self.drop_obj(o),
                    _ => p.down = true,
                }
                self.kick(o);
            }
            (Wait::Prove(o, e, round, up), Res::Got(v)) => {
                if !v.is_some_and(|(v, _)| self.is_mine(&v, e)) {
                    if self.serving_in(o, e).is_some() {
                        self.drop_obj(o);
                    }
                    return;
                }
                let Some(s) = self.serving_in(o, e) else { return };
                s.proving = false;
                s.proof = (s.proof.0.max(round), s.proof.1.max(up));
                self.kick(o);
            }
            (Wait::Prove(o, e, ..), Res::Lost) => {
                let Some(s) = self.serving_in(o, e) else { return };
                s.proving = false;
                self.kick(o);
            }
            // D3: a lost upload while serving. The peers hold the entries,
            // so keep serving and try again on the next tick.
            (Wait::Upload(o, e, upto, _), Res::Lost) if self.serving_in(o, e).is_some_and(|s| !s.peers.is_empty()) => {
                let s = self.serving_in(o, e).unwrap();
                s.uploading = false;
                s.upload_wait = true;
                s.retry_upto = Some(upto);
            }
            // D2 (nothing can be acknowledged without the bucket), or a
            // refused upload: give the object up. Its acknowledged entries
            // are on the peers or already uploaded.
            (Wait::Upload(o, e, ..), _) => {
                if self.serving_in(o, e).is_some() {
                    self.drop_obj(o);
                }
            }
            // Anything else while starting (a lost read or write, a failed
            // condition, a missing manifest): give the object up, and fail
            // its requests now. A node that can't reach the bucket mustn't
            // hold clients; they retry, maybe on another node. Safe: a
            // claim given up is just an epoch nobody serves.
            (w, _) => {
                if let Some(o) = w.obj() {
                    if matches!(self.st(o).phase, Phase::Starting) {
                        self.drop_obj(o);
                    }
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
        // Keep the old peers unless the old epoch was sealed (or had none).
        let prev = self.st(o).prev.clone();
        let peers = if self.must_fence(&prev) { prev.unwrap().peers } else { self.peers_of(o) };
        self.st(o).peers = peers.clone();
        let v = Val::Rec(Rec { node: self.node, epoch: next, sealed: false, peers });
        self.call(Wait::Cas(o, next), Call::Put(own_key(o), v, cond));
    }

    /// After winning the CAS: fence, then read what came before. Never before
    /// the CAS (a stale owner may still be adding to it until then), and
    /// never before the fence (the old owner may still be acknowledging).
    fn load(&mut self, o: Obj, e: u32) {
        if let Some(base) = self.st(o).early.take().filter(|_| self.bugs.load_before_cas) {
            self.write_manifest(o, e, base);
            return;
        }
        let prev = self.st(o).prev.clone();
        if !self.must_fence(&prev) {
            self.st(o).tails.clear();
            return self.call(Wait::List(o, e), Call::List(obj_prefix(o)));
        }
        let peers = prev.unwrap().peers;
        self.st(o).fencing = Some((e, peers.len()));
        for n in peers {
            self.peer(Wait::Fence(o, e), n, PeerCall::Fence { obj: o, epoch: e });
        }
    }

    /// From a listing: the last epoch q before `e` with a manifest, plus q's
    /// uploads. `early`: this is the wrong, before-the-CAS version.
    fn history(&mut self, o: Obj, e: u32, keys: Vec<String>, early: Option<Option<String>>) {
        let mut manifests = vec![];
        let mut segs: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for k in &keys {
            match parse_key(o, k) {
                // A newer epoch has served: we're stale, and the peers may
                // have dropped what we'd need. Stop.
                Some((ep, "m")) if ep > e && early.is_none() => return self.drop_obj(o),
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
            // Wrong on purpose: every upload ever, in epoch order.
            let base = segs.into_values().flatten().collect();
            return start(self, base);
        }
        let Some(&q) = manifests.iter().max() else { return start(self, vec![]) };
        let ups = segs.remove(&q).unwrap_or_default();
        // Uploads go one at a time, in order: the last one says how many.
        let uploaded = ups.last().and_then(|k| parse_key(o, k)?.1[1..].parse().ok()).unwrap_or(0);
        let w = match early {
            Some(etag) => Wait::EarlyManifest(o, etag, e, ups),
            None => Wait::PrevManifest(o, e, ups, uploaded, q),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_store_appends_skips_truncates_and_fences() {
        let mut p = PeerStore::default();
        let e = |b: u8| vec![b];
        let app = |p: &mut PeerStore, epoch, from, ents: Vec<Vec<u8>>, trunc| match p.handle(PeerCall::Append { obj: 1, epoch, from, ents, trunc }) {
            PeerRes::Have(n) => Some(n),
            _ => None,
        };
        assert_eq!(app(&mut p, 1, 1, vec![e(1), e(2)], 0), Some(2));
        assert_eq!(app(&mut p, 1, 2, vec![e(2), e(3)], 1), Some(3), "a resend keeps what it has");
        assert_eq!(app(&mut p, 1, 6, vec![e(6)], 4), Some(3), "no skip past what the bucket has");
        assert_eq!(app(&mut p, 1, 6, vec![e(6)], 5), Some(6), "skips ahead to what the bucket lacks");
        let PeerRes::Tails(t) = p.handle(PeerCall::Fence { obj: 1, epoch: 2 }) else { panic!() };
        assert_eq!((t[0].epoch, t[0].from, t[0].ents.clone()), (1, 5, vec![e(6)]));
        assert!(app(&mut p, 1, 7, vec![e(7)], 0).is_none(), "a fenced epoch is refused");
        assert_eq!(app(&mut p, 2, 1, vec![e(9)], 0), Some(1));
        let PeerRes::Tails(t) = p.handle(PeerCall::Fence { obj: 1, epoch: 3 }) else { panic!() };
        assert_eq!(t.len(), 1, "older epochs go once a newer one appends");
        // A snapshot rebuilds the same store.
        let mut q = PeerStore::default();
        for c in p.snapshot() {
            q.handle(c);
        }
        assert_eq!(q.handle(PeerCall::Fence { obj: 1, epoch: 3 }), p.handle(PeerCall::Fence { obj: 1, epoch: 3 }));
        assert!(app(&mut q, 2, 2, vec![e(1)], 0).is_none(), "the fence survives");
    }

    #[test]
    fn frames_round_trip() {
        let ents = vec![vec![], vec![1, 2, 3], vec![9; 300]];
        assert_eq!(unframe(&frame(&ents)), Some(ents));
        assert_eq!(unframe(&[5, 0, 0, 0, 1]), None);
    }
}
