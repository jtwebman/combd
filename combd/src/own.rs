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
//! either way, and peers drop what the bucket has. With every peer up,
//! uploads wait (`uploads`: for a while, or until enough has built up), so a
//! busy object makes one bucket write every few seconds, not one per write;
//! until then its newest writes live on its peers only. With no peers, or
//! one down, the bucket is how writes get acknowledged, so they go at once.
//!
//! Bucket keys, per object `o`:
//! - `own/o`: the ownership record `{node, epoch, sealed, peers}`, changed
//!   only by CAS.
//! - `obj/o/e<E>/m`: the manifest: the segment keys the owner of epoch E
//!   started from, in order.
//! - `obj/o/e<E>/s<N>`: an upload of epoch E: its entries up to entry N.
//! - `obj/o/e<E>/t`: what the peers had of the previous epoch past its uploads.
//! - `obj/o/e<E>/i`: an image: the object as epoch E's owner restored it from
//!   its manifest, as one entry. The next owner starts from it instead of
//!   the manifest's keys (compaction: a manifest only ever grows).
//! - `obj/o/e<E>/i<N>`: an image after E's first N entries, for an object that
//!   stays up a long time. Written only once those entries are uploaded.
//!
//! Liveness is over the wire, not the bucket: every node sends every other a
//! small heartbeat a few times a lease (`Event::Heard`), and a node not heard
//! from for `LEASE_MS`, by the *observer's* clock, can be taken over (one
//! never heard from is timed from this core's own start, so a node that just
//! booted doesn't take everything). It was a heartbeat in the bucket, which
//! cost a bucket write per node every third of a lease. Leases are for
//! liveness only: safety holds with any clocks, and any process may claim
//! at any time (`spec/Ownership.tla`), so this changes no proof.

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
/// the simulator. Real nodes set it (`COMBD_LEASE_MS`): several times the
/// worst round trip between nodes, and heartbeats go every third of it.
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
    /// The sealed epoch this one's peers were picked after (0: none). Kept
    /// across takeovers of unsealed epochs, so a takeover that loads from
    /// it knows the bucket has all of it, and needs no peer's tail (its
    /// peers may hold none of it). 0 in records from before 0.1: they wait
    /// for a tail, as then.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sealed_from: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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
    /// Delete a key (gone already is fine).
    Delete(String),
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
    /// Only in a peer store's own log, never on the wire: a store that lost
    /// its disk relearned `obj` from the bucket, whose record said `epoch`
    /// (it refuses every epoch below), and `born`: the newest epoch that
    /// began before it came back, by the bucket's times (`epoch` if it can't
    /// tell). It saw all of every epoch after that.
    Learn { obj: Obj, epoch: u32, born: u32 },
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
    /// Every tail, and `born`: 0, or for a store that lost its disk, the
    /// epoch it relearned. It speaks only for epochs after that.
    Tails { born: u32, tails: Vec<Tail> },
    /// Not yet: this store promised the object's owner it wouldn't be fenced
    /// for this many more ms (a read lease, see `PeerStore::handle_at`).
    Wait(u64),
    /// As `Have`, but with no promise: a newer owner's fence is waiting here,
    /// so this store stops renewing the older epoch's lease.
    Kept(u64),
    /// As `Kept`, from a store that lost its disk during this epoch: it took
    /// the entries, but can't speak for the epoch in a takeover (see
    /// `born`). Its owner reseats: seals, and claims a new epoch, which this
    /// store sees whole.
    Reborn(u64),
}

#[derive(Clone, Debug)]
pub enum Res {
    /// Value and etag.
    Got(Option<(Val, String)>),
    /// The new etag, or `Err` if the condition failed.
    Put(Result<String, ()>),
    Listed(Vec<String>),
    Deleted,
    Peer(PeerRes),
    /// No reply in time: the call may or may not have happened.
    Lost,
}

pub enum Event {
    Request { obj: Obj, req: ReqId },
    /// The executor ran a request; `changed`: it wrote something. `proof:
    /// false`: its answer goes nowhere that needs it proven (an object's own
    /// beat), so if it changed nothing and every earlier write is already
    /// acknowledged, it's done now, with no round to the peers. (A read would
    /// need one: a node that just lost the object mustn't answer for it.)
    Ran { obj: Obj, act: Act, req: ReqId, changed: bool, proof: bool },
    /// The changes since the last seal: the next entry.
    Sealed { obj: Obj, act: Act, changes: Vec<u8> },
    /// The executor rebuilt the object from its segments (or couldn't).
    Restored { obj: Obj, act: Act, ok: bool },
    /// The image `Restore` asked for is written: once ownership is proven
    /// again, older epochs' keys can go.
    Imaged { obj: Obj, act: Act, key: String },
    Reply { op: OpId, res: Res },
    /// Time passes: retry peers that went quiet.
    Tick,
    /// A heartbeat from `node` (over the wire).
    Heard { node: Node },
    /// Let an idle object go (eviction). Ignored unless nothing is running,
    /// waiting, or not yet uploaded. The next request claims it again.
    Release { obj: Obj },
    /// Hand the object to `to` (placement): take no new requests, finish
    /// what's in hand, upload it all, then step down with the record sealed,
    /// so `to` may claim it at once, and send what waited there (`Moved`).
    /// Ignored unless it's served here; given up if it hasn't settled
    /// within a lease.
    Move { obj: Obj, to: Node },
    /// A request another node sent on, believing we own the object (or
    /// should, after a move). As `Request`, but read the record, never a
    /// routing hint: ours may point straight back.
    Forwarded { obj: Obj, req: ReqId },
    /// Start serving the object here now, with no request: it was just
    /// handed to us (`Effect::Handed`). Ignored unless it's idle here.
    Claim { obj: Obj },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// Done and proven: send the answer the executor made.
    Done { epoch: u32 },
    /// Another node owns it; ask there.
    Redirect(Node),
    /// Not done, or not known to be done: try again.
    Failed,
    /// Never ran (it was still waiting when the object went): safe to try
    /// again as is, even for a write.
    NotRun,
    /// Never ran: the object moved to this node (`Event::Move`) while it
    /// waited, or this node handed it there lately. Send it there.
    Moved(Node),
}

#[derive(Debug)]
pub enum Effect {
    Call { op: OpId, call: Call },
    /// A call to `node`'s peer store (maybe our own).
    Peer { op: OpId, node: Node, call: PeerCall },
    Answer { req: ReqId, ans: Answer },
    /// Build the object from these segments, in order, from nothing.
    /// `image`: once restored, write the object as one entry there (the list
    /// is long), for the next owner to start from.
    Restore { obj: Obj, act: Act, segs: Vec<String>, image: Option<String> },
    /// Run a request, in order, after the ones before it.
    Run { obj: Obj, act: Act, req: ReqId },
    /// Hand over the changes made since the last seal.
    /// `image`: take an image as of this entry, to put at that key once the
    /// entries up to it are uploaded (`PutImage`).
    Seal { obj: Obj, act: Act, image: Option<String> },
    /// The image `Seal` took: put it now (its entries are uploaded).
    PutImage { obj: Obj, act: Act },
    /// Forget the object's state: we don't own it now.
    Drop { obj: Obj },
    /// A move is done (`Event::Move`): tell `to` to claim it now
    /// (`Event::Claim`), not at its next request, or a quiet object would
    /// sit unserved, and come back to us on the next request we see.
    Handed { obj: Obj, to: Node },
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
    /// D3: use a wiped peer's tail for an epoch it may have lost.
    pub trust_wiped: bool,
    /// D3: a wiped peer doesn't relearn its fence from the bucket.
    pub wipe_forgets_fence: bool,
    /// Liveness: every node counts as heard from, always (so nothing is ever
    /// taken over). Safety holds anyway; the simulator's liveness check must
    /// catch it.
    pub assume_alive: bool,
    /// Liveness: uploads wait even with a peer down, so writes (which need
    /// the bucket then) wait for the timer.
    pub wait_when_down: bool,
    /// Liveness: an owner may be its own peer, and nothing reseats (as
    /// before 0.1): one lost disk and one lost node can strand an object.
    pub self_peer: bool,
    /// Read lease: peers let a newer owner fence them while they've promised
    /// the old one not to.
    pub lease_no_promise: bool,
    /// Read lease: the owner trusts its lease longer than peers promised.
    pub lease_overtrust: bool,
    /// Read lease: the owner counts a `Kept` (no promise) as a promise.
    pub lease_trusts_kept: bool,
    /// Compaction: delete older epochs' keys as soon as the image is
    /// written, with no proof that we still own the object.
    pub gc_unproven: bool,
    /// Compaction: delete by the newest image the listing shows, not the one
    /// the round proved (a later one may be unproven).
    pub gc_newest_image: bool,
}

/// For a store that lost its disk and is relearning `o` (record at `epoch`):
/// the newest epoch that began before it came back, which it can't speak
/// for. An epoch sends entries only once its manifest is written, so one
/// whose manifest was written after the store came back sent it all of
/// them. `written(key)`: is the key there, and written after the store came
/// back (`Some(true)`) or not; `Err` if it can't tell. A missing manifest
/// (never served, or compacted away) is passed over; past 16, it stops and
/// speaks for no more.
pub fn born_of(o: Obj, epoch: u32, mut written: impl FnMut(&str) -> Result<Option<bool>, ()>) -> Option<u32> {
    // Every epoch from `lo` up began after it came back.
    let mut lo = epoch + 1;
    for _ in 0..16 {
        if lo <= 1 {
            return Some(0);
        }
        match written(&manifest_key(o, lo - 1)) {
            Err(()) => return None,
            Ok(Some(false)) => return Some(lo - 1),
            Ok(Some(true) | None) => lo -= 1,
        }
    }
    Some(lo - 1)
}

/// A node's peer store: the entries it keeps for other nodes' objects. The
/// real node keeps it on disk; it must survive the node's restarts.
///
/// If the disk is lost, the store comes back `wiped`, and it forgot two
/// things: what it held, and whom it fenced. So before it answers anything
/// about an object, it reads the object's ownership record (epoch R) and
/// refuses every epoch below R: the fence it lost. Epochs after R began
/// after it came back, so it saw all of them; of R and older it may hold a
/// partial copy, so its answers carry `born` = R, and owners don't use its
/// tail of those.
#[derive(Default)]
pub struct PeerStore {
    objs: HashMap<Obj, PeerObj>,
    pub wiped: bool,
    /// The bug switch `wipe_forgets_fence`, for tests only.
    pub forgets_fence: bool,
    /// Read leases: after acknowledging an owner's append, a store promises
    /// that owner's epoch this long (its own clock) before a newer one may
    /// fence it, and as long after it starts (it can't remember what it
    /// promised before). 0: no promises.
    pub promise_ms: u64,
    /// Promises made, per object: (epoch, until). In memory only: a
    /// restart is covered by `started`.
    held: HashMap<Obj, (u32, u64)>,
    /// A newer owner told to wait, per object: (its epoch, until). Older
    /// epochs get no more promises meanwhile, so the wait ends. It lapses
    /// (an owner that gave up), and a fence that's still waiting renews it.
    wants: HashMap<Obj, (u32, u64)>,
    started: Option<u64>,
    /// The bug switch `lease_no_promise`, for tests only.
    pub ignores_promise: bool,
}

#[derive(Default)]
struct PeerObj {
    fenced: u32,
    born: u32,
    /// Per epoch: how many entries were dropped, and the rest.
    logs: BTreeMap<u32, (u64, Vec<Vec<u8>>)>,
}

impl PeerStore {
    /// A store that lost its disk.
    pub fn wiped() -> PeerStore {
        PeerStore { wiped: true, ..PeerStore::default() }
    }

    /// The store's process started (again) at `now`: what it promised before
    /// is forgotten, so it holds off every fence for a lease.
    pub fn started(&mut self, now: u64) {
        self.held.clear();
        self.wants.clear();
        self.started = Some(now);
    }

    /// A call at `now` (this store's clock), keeping its promises: a fence
    /// for a newer epoch than one it promised is told to wait.
    pub fn handle_at(&mut self, call: PeerCall, now: u64) -> PeerRes {
        if self.promise_ms > 0 && !self.ignores_promise {
            let started = *self.started.get_or_insert(now);
            if let PeerCall::Fence { obj, epoch } = &call {
                let held = self.held.get(obj).filter(|h| *epoch > h.0).map_or(0, |h| h.1);
                let until = held.max(started + self.promise_ms);
                if now < until {
                    let w = self.wants.entry(*obj).or_insert((0, 0));
                    *w = (w.0.max(*epoch), now + 2 * self.promise_ms);
                    return PeerRes::Wait(until - now);
                }
            }
        }
        let append = match &call {
            PeerCall::Append { obj, epoch, .. } => Some((*obj, *epoch)),
            _ => None,
        };
        let res = self.handle(call);
        if let (Some((obj, epoch)), PeerRes::Have(n), true) = (append, &res, self.promise_ms > 0) {
            if self.wants.get(&obj).is_some_and(|&(e, until)| e > epoch && now < until) {
                return PeerRes::Kept(*n);
            }
            self.held.insert(obj, (epoch, now + self.promise_ms));
        }
        res
    }

    /// Can this store speak for `epoch` of `obj` in a takeover? Not if it
    /// lost its disk and hasn't seen that epoch whole (see `born`).
    pub fn speaks_for(&self, obj: Obj, epoch: u32) -> bool {
        !self.wiped || self.objs.get(&obj).is_some_and(|po| po.born < epoch)
    }

    /// Does a wiped store need to relearn this object first (`Learn`, with
    /// the epoch in the bucket's record)?
    pub fn must_learn(&self, call: &PeerCall) -> Option<Obj> {
        match call {
            PeerCall::Append { obj, .. } | PeerCall::Fence { obj, .. } if self.wiped && !self.objs.contains_key(obj) => Some(*obj),
            _ => None,
        }
    }

    pub fn handle(&mut self, call: PeerCall) -> PeerRes {
        match call {
            PeerCall::Learn { obj, epoch, born } => {
                let fenced = if self.forgets_fence { 0 } else { epoch.saturating_sub(1) };
                let po = self.objs.entry(obj).or_default();
                po.fenced = po.fenced.max(fenced);
                po.born = po.born.max(born.min(epoch));
                PeerRes::Have(0)
            }
            PeerCall::Append { obj, epoch, from, ents, trunc } => {
                let po = self.objs.entry(obj).or_default();
                if epoch <= po.fenced || from == 0 {
                    return PeerRes::Refused;
                }
                // An epoch that appends has its manifest written, and that
                // manifest holds everything older: those logs aren't needed.
                po.logs.retain(|&e, _| e >= epoch);
                let len = po.logs.get(&epoch).map_or(0, |(d, l)| d + l.len() as u64);
                let have = |n| if epoch <= po.born { PeerRes::Reborn(n) } else { PeerRes::Have(n) };
                if from > len + 1 && from - 1 > trunc {
                    // A gap the bucket doesn't cover: say what we have (and
                    // don't start a log we can't fill).
                    return have(len);
                }
                let (dropped, log) = po.logs.entry(epoch).or_default();
                if from > len + 1 {
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
                have(len)
            }
            PeerCall::Fence { obj, epoch } => {
                let po = self.objs.entry(obj).or_default();
                po.fenced = po.fenced.max(epoch.saturating_sub(1));
                let tails = po.logs.iter().map(|(&e, (d, l))| Tail { epoch: e, from: *d, ents: l.clone() }).collect();
                PeerRes::Tails { born: po.born, tails }
            }
        }
    }

    /// Calls that rebuild this store from nothing: what a wiped store
    /// relearned, each object's logs, newest epoch first (an append drops
    /// older epochs, not newer), then its fence.
    pub fn snapshot(&self) -> Vec<PeerCall> {
        let mut out = vec![];
        for (&obj, po) in &self.objs {
            if po.born > 0 {
                out.push(PeerCall::Learn { obj, epoch: po.born, born: po.born });
            }
            for (&epoch, (d, l)) in po.logs.iter().rev() {
                out.push(PeerCall::Append { obj, epoch, from: d + 1, ents: l.clone(), trunc: *d });
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

/// Rendezvous hashing's score for an object on a node (splitmix64).
fn spread_score(o: Obj, n: Node) -> u64 {
    let mut z = (o as u64) ^ ((o >> 64) as u64) ^ (n as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
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
pub fn image_key(o: Obj, e: u32) -> String {
    format!("obj/{o:032x}/e{e:08}/i")
}
/// An image after epoch `e`'s first `n` entries.
fn image_key_at(o: Obj, e: u32, n: u64) -> String {
    format!("obj/{o:032x}/e{e:08}/i{n:012}")
}
/// The entries an image or segment part (`i`, `i<N>`, `s<N>`) covers.
fn part_upto(part: &str) -> Option<u64> {
    match part {
        "i" => Some(0),
        _ => part.get(1..)?.parse().ok(),
    }
}

/// A restore from this many segments or more writes an image (each is a GET).
pub const COMPACT_SEGS: usize = 16;
/// At most this many deletes per compaction (the rest go next time).
const GC_MAX: usize = 1000;
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
    Cas(Obj, u32),
    ReadBack(Obj, u32),
    /// Only with `Bugs::load_before_cas`: the history, read before the CAS.
    EarlyList(Obj, Option<String>, u32),
    EarlyManifest(Obj, Option<String>, u32, Vec<String>),
    Fence(Obj, u32, Node),
    List(Obj, u32),
    /// Reading the manifest of an earlier epoch q: its uploads, how many
    /// entries they hold, and q.
    PrevManifest(Obj, u32, Vec<String>, u64, u32),
    TailSeg(Obj, u32, Vec<String>),
    Manifest(Obj, u32),
    /// Serving: an upload up to entry N (a retry?), a peer append (peer
    /// index, round), a proof (round, entries uploaded).
    Upload(Obj, u32, u64, bool),
    /// Peer `usize`, round, and when it was sent (our clock).
    Append(Obj, u32, usize, u64, u64),
    Prove(Obj, u32, u64, u64),
    /// Marking the record sealed; nothing waits on it.
    SealRec,
    /// Sealing it as the object leaves for a node (`Event::Move`): its
    /// waiting requests go there once it's done.
    Leave(Obj, Node),
    /// Compaction: listing an object to delete what its image replaced, and
    /// the deletes. Nothing waits on them.
    GcList(Obj, u32, u64),
    GcDelete,
}

impl Wait {
    fn obj(&self) -> Option<Obj> {
        use Wait::*;
        match self {
            ReadRec(o) | Cas(o, _) | ReadBack(o, _) | EarlyList(o, ..) | EarlyManifest(o, ..) | Fence(o, ..)
            | List(o, _) | PrevManifest(o, ..) | TailSeg(o, ..) | Manifest(o, _) | Upload(o, ..) | Append(o, ..) | Prove(o, ..) => {
                Some(*o)
            }
            SealRec | Leave(..) | GcList(..) | GcDelete => None,
        }
    }
}

#[derive(Debug)]
enum Phase {
    Idle,
    /// Claiming, fencing, loading, writing the manifest, or restoring.
    Starting,
    Serving(Serve),
    /// Moving to another node (`Event::Move`): dropped here, the record
    /// being sealed. Requests wait.
    Leaving,
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
    /// When (our clock) the last call it acknowledged was sent: its promise
    /// runs from after that (see `Core::read_lease`).
    acked_at: Option<u64>,
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
    /// Compaction waits on this round: once it's proven, we still own the
    /// object after our image was written.
    gc: Option<u64>,
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
    /// The bytes in `ents`, and when the oldest not yet in an upload was
    /// made: uploads wait for one or the other to get big enough.
    bytes: usize,
    since: Option<u64>,
    /// Upload now, without waiting (the object is being let go).
    flush: bool,
    /// D3: an upload was lost. Wait for a tick, then send the very same one
    /// again (up to this entry): it may have landed, and two segments must
    /// never overlap.
    upload_wait: bool,
    retry_upto: Option<u64>,
    /// Uploads since the last image (compaction: one every `compact_segs`).
    since_image: usize,
    /// An image taken as of this entry, put once it's uploaded; and the
    /// upload that must end right there (segments never straddle an image).
    image_at: Option<u64>,
    cut: Option<u64>,
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
    /// Moving to this node (`Event::Move`), since then (our clock): new
    /// requests wait until it has settled. To our own node: a reseat.
    moving: Option<(Node, u64)>,
    /// A peer lost its disk this epoch (`PeerRes::Reborn`): reseat.
    reseat: bool,
    /// Our record's `sealed_from`, kept when we seal it.
    sealed_from: u32,
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

/// A takeover's fence: calls go to every peer, and answers come back.
struct FenceSt {
    epoch: u32,
    /// Calls still out.
    out: usize,
    /// Answers: (born, tails).
    replies: Vec<(u32, Vec<Tail>)>,
    /// How many answers came before the listing went out. Only those can be
    /// used: the listing must come after the fence of the peer we use.
    listed: usize,
    /// None of those could speak for the epoch we need: wait for the rest,
    /// then list again.
    waiting: bool,
    /// Peers that said "not yet" (a lease they promised): ask again then.
    again: Vec<(Node, u64)>,
}

struct ObjSt {
    phase: Phase,
    /// Requests waiting for the object to start.
    waiting: VecDeque<ReqId>,
    /// Segment keys read before the CAS (only with `Bugs::load_before_cas`).
    early: Option<Vec<String>>,
    /// The record we're claiming over, and what we claim: our peers, and
    /// the sealed epoch they were picked after.
    prev: Option<Rec>,
    peers: Vec<Node>,
    sealed_from: u32,
    etag: String,
    /// Fencing for epoch E, until the manifest is written.
    fence: Option<FenceSt>,
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
    /// Segments sent to the bucket (retries too).
    pub uploads: u64,
    /// Reads answered under the read lease, with no round.
    pub lease_reads: u64,
    /// Fences told to wait out a peer's promise (a takeover held off).
    pub fence_waits: u64,
    /// Keys deleted by compaction (what an image replaced).
    pub gc_deleted: u64,
    /// Objects handed to another node (`Event::Move`), sealed and gone.
    pub moves: u64,
    /// Epochs sealed and claimed again here (a peer lost its disk, or we
    /// were our own peer): see `PeerRes::Reborn`.
    pub reseats: u64,
}

pub struct Core {
    pub node: Node,
    bugs: Bugs,
    lease_ms: u64,
    /// Every node, and how many peers each object gets (0: D2).
    fleet: Vec<Node>,
    replicas: usize,
    /// D3 uploads wait up to this long, or until this many bytes (0 ms: no
    /// wait).
    upload_ms: u64,
    upload_bytes: usize,
    /// Read leases: how long after a round all peers acknowledged a read may
    /// be answered without another (0: every read waits for a round).
    lease_trust_ms: u64,
    /// A restore from this many segments writes an image (`COMPACT_SEGS`).
    compact_segs: usize,
    next_op: OpId,
    next_act: Act,
    ops: HashMap<OpId, Wait>,
    objs: BTreeMap<Obj, ObjSt>,
    /// The segment list of a manifest being written, until it's written.
    pending_base: HashMap<(Obj, u32), Vec<String>>,
    /// Loads that listed many keys: their restore writes an image, so the
    /// old keys can go even if the list to restore is short.
    crowded: HashSet<(Obj, u32)>,
    /// When we last heard from each node (our clock), and when this core
    /// started (a node never heard from counts from then).
    heard: HashMap<Node, u64>,
    born: Option<u64>,
    /// Who owned an object when we last checked, when (our clock), and
    /// whether we handed it there ourselves (a move). For routing only: a
    /// fresh hint skips the bucket and redirects at once.
    hints: HashMap<Obj, (Node, u64, bool)>,
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
            bugs,
            lease_ms,
            fleet: vec![],
            replicas: 0,
            upload_ms: 0,
            upload_bytes: 1 << 20,
            lease_trust_ms: 0,
            compact_segs: COMPACT_SEGS,
            next_op: 0,
            next_act: 0,
            ops: HashMap::new(),
            objs: BTreeMap::new(),
            pending_base: HashMap::new(),
            crowded: HashSet::new(),
            heard: HashMap::new(),
            born: None,
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

    /// D3: read leases, with peers that promise `promise_ms` (their
    /// `PeerStore::promise_ms`). The owner trusts three quarters of it,
    /// counted from when it sent the round: room for clocks that run apart
    /// by up to a fifth.
    pub fn read_lease(mut self, promise_ms: u64) -> Core {
        self.lease_trust_ms = if self.bugs.lease_overtrust { promise_ms * 3 / 2 } else { promise_ms * 3 / 4 };
        self
    }

    /// Write an image after a restore from `n` segments or more.
    pub fn compact_at(mut self, n: usize) -> Core {
        self.compact_segs = n;
        self
    }

    /// D3: let uploads wait up to `ms`, or until `bytes` have built up.
    pub fn uploads(mut self, ms: u64, bytes: usize) -> Core {
        self.upload_ms = ms;
        self.upload_bytes = bytes;
        self
    }

    /// Rendezvous hashing: the `replicas` nodes that score highest for `o`.
    /// An object's peers when we claim it fresh: never us. Our own store
    /// goes down with us, so as a peer it would leave one copy where the
    /// fleet counts two (`reseat` puts an epoch that kept us right).
    fn peers_of(&self, o: Obj) -> Vec<Node> {
        let mut f = self.fleet.clone();
        if !self.bugs.self_peer {
            f.retain(|&n| n != self.node);
        }
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
            sealed_from: 0,
            etag: String::new(),
            fence: None,
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

    /// Where an object of a spread process (`placement = "spread"`) should
    /// start: the node rendezvous hashing picks among `among` (its scope)
    /// heard from within a lease (this one counts), if that isn't this one. `None`:
    /// start it here, or it's already here or starting, or this core knows
    /// who has it (and routes there itself). A node going quiet moves only
    /// its own share.
    pub fn place(&self, now: u64, o: Obj, among: &[Node]) -> Option<Node> {
        if self.objs.get(&o).is_some_and(|s| !matches!(s.phase, Phase::Idle)) {
            return None;
        }
        if self.hints.get(&o).is_some_and(|&(_, t, _)| now.saturating_sub(t) < self.lease_ms) {
            return None;
        }
        self.home(now, o, among).filter(|&n| n != self.node)
    }

    /// A spread object's home: the node rendezvous hashing picks among
    /// `among` heard from within a lease (this one counts). Wherever the
    /// object is now: placement moves one that isn't home once it's back.
    pub fn home(&self, now: u64, o: Obj, among: &[Node]) -> Option<Node> {
        let born = self.born.unwrap_or(now);
        let alive = |n: Node| n == self.node || now.saturating_sub(self.heard.get(&n).copied().unwrap_or(born)) < self.lease_ms;
        among.iter().copied().filter(|&n| alive(n)).max_by_key(|&n| spread_score(o, n))
    }

    pub fn on(&mut self, now: u64, ev: Event) -> Vec<Effect> {
        self.now = now;
        self.born.get_or_insert(now);
        match ev {
            Event::Heard { node } => {
                self.heard.insert(node, now);
            }
            Event::Tick => {
                // Fences told to wait (a lease the peer promised): again, once due.
                let mut again = vec![];
                for (&o, st) in self.objs.iter_mut() {
                    if let Some(f) = st.fence.as_mut() {
                        let (due, wait): (Vec<_>, Vec<_>) = f.again.drain(..).partition(|x| x.1 <= now);
                        f.again = wait;
                        again.extend(due.into_iter().map(|(n, _)| (o, f.epoch, n)));
                    }
                }
                for (o, e, n) in again {
                    self.peer(Wait::Fence(o, e, n), n, PeerCall::Fence { obj: o, epoch: e });
                }
                let live: Vec<Obj> = self.objs.iter().filter(|(_, st)| matches!(st.phase, Phase::Serving(_))).map(|(&o, _)| o).collect();
                let (lease_ms, me, self_peer) = (self.lease_ms, self.node, self.bugs.self_peer);
                for o in live {
                    let mut late = vec![];
                    let mut stay = false;
                    if let Phase::Serving(s) = &mut self.st(o).phase {
                        s.peers.iter_mut().for_each(|p| p.retry = p.down);
                        s.upload_wait = false;
                        // A move that hasn't settled in a lease (the bucket
                        // out of reach, say): stay, and serve what waited.
                        if s.moving.is_some_and(|(_, t)| now.saturating_sub(t) > lease_ms) {
                            s.moving = None;
                            stay = true;
                        }
                        // Reseat: our peers are us (an epoch taken over from
                        // one that named us), or one lost its disk. Seal,
                        // and claim the next epoch, with peers that aren't
                        // us and stores that see it whole.
                        if !self_peer && s.moving.is_none() && (s.reseat || s.peers.iter().any(|p| p.node == me)) {
                            s.reseat = false;
                            s.moving = Some((me, now));
                            s.flush = true;
                        }
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
                    if stay {
                        let waiting: Vec<ReqId> = self.st(o).waiting.drain(..).collect();
                        for r in waiting {
                            self.run(o, r);
                        }
                    }
                    self.kick(o);
                }
            }
            Event::Request { obj, req } => self.request(now, obj, req, false),
            Event::Forwarded { obj, req } => self.request(now, obj, req, true),
            Event::Claim { obj } => {
                let st = self.st(obj);
                if matches!(st.phase, Phase::Idle) {
                    st.phase = Phase::Starting;
                    self.call(Wait::ReadRec(obj), Call::Get(own_key(obj)));
                }
            }
            Event::Move { obj, to } => {
                let me = self.node;
                if let Phase::Serving(s) = &mut self.st(obj).phase {
                    if to != me && s.moving.is_none() {
                        s.moving = Some((to, now));
                        s.flush = true;
                        self.kick(obj);
                    }
                }
            }
            Event::Ran { obj, act, req, changed, proof } => {
                if let Some(s) = self.serving(obj, act) {
                    if !proof && !changed && s.pends.is_empty() && s.sealing.is_none() && !s.dirty && s.running.remove(&req) {
                        let epoch = s.epoch;
                        self.answer(req, Answer::Done { epoch });
                        self.try_leave(obj);
                        return std::mem::take(&mut self.out);
                    }
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
                    s.bytes += changes.len();
                    s.since.get_or_insert(now);
                    s.ents.push_back(changes);
                    // Leaving: the bucket must have everything, now.
                    s.flush |= s.moving.is_some();
                    s.round += 1;
                    s.pends.push_back(Pend { need: s.made, round: s.round, since: now, reqs, gc: None });
                    self.flush(obj);
                    self.kick(obj);
                }
            }
            Event::Restored { obj, act, ok } => self.restored(obj, act, ok),
            Event::Imaged { obj, act, key } => {
                let unproven = self.bugs.gc_unproven;
                // Which image: what the deletes may lean on is this one, once
                // proven, never one that came after.
                let Some(n) = parse_key(obj, &key).and_then(|(_, part)| part_upto(part)) else { return std::mem::take(&mut self.out) };
                if let Some(s) = self.serving(obj, act) {
                    if unproven {
                        let e = s.epoch;
                        self.gc_list(obj, e, n);
                    } else {
                        // A round to prove we still own it, now that the image
                        // is there (see `gc_list`).
                        s.round += 1;
                        s.pends.push_back(Pend { need: s.made, round: s.round, since: now, reqs: vec![], gc: Some(n) });
                        self.kick(obj);
                    }
                }
            }
            Event::Release { obj } => {
                let seal_early = self.bugs.seal_early;
                let st = self.st(obj);
                // Only uploads left: send them now, and it goes next time.
                if let Phase::Serving(s) = &mut st.phase {
                    if s.uploaded < s.made && !s.flush {
                        s.flush = true;
                        self.kick(obj);
                        return std::mem::take(&mut self.out);
                    }
                }
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
                        let r = Rec { node: self.node, epoch: s.epoch, sealed: true, peers, sealed_from: s.sealed_from };
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
        let peers = st.peers.iter().map(|&node| PeerSt { node, have: 0, round: 0, busy: false, down: false, retry: false, acked_at: None }).collect();
        st.phase = Phase::Serving(Serve {
            epoch: st.epoch,
            act,
            etag: std::mem::take(&mut st.etag),
            peers,
            made: 0,
            uploaded: 0,
            ents: VecDeque::new(),
            uploading: false,
            bytes: 0,
            since: None,
            flush: false,
            upload_wait: false,
            retry_upto: None,
            since_image: 0,
            image_at: None,
            cut: None,
            round: 0,
            proving: false,
            proof: (0, 0),
            running: HashSet::new(),
            batch: vec![],
            dirty: false,
            sealing: None,
            pends: VecDeque::new(),
            moving: None,
            reseat: false,
            sealed_from: st.sealed_from,
        });
        let waiting: Vec<ReqId> = st.waiting.drain(..).collect();
        for r in waiting {
            self.run(o, r);
        }
    }

    fn request(&mut self, now: u64, o: Obj, req: ReqId, forwarded: bool) {
        // A hint we read may be stale, so a forwarded request reads the
        // record instead. One we made by moving the object is the truth: it
        // sends even a forwarded request on, or the sealed record would let
        // us claim the object straight back.
        if matches!(self.st(o).phase, Phase::Idle) {
            if let Some(&(n, t, moved)) = self.hints.get(&o) {
                if now.saturating_sub(t) >= self.lease_ms {
                    self.hints.remove(&o);
                } else if moved {
                    return self.answer(req, Answer::Moved(n));
                } else if !forwarded {
                    return self.answer(req, Answer::Redirect(n));
                }
            }
        }
        let st = self.st(o);
        match &st.phase {
            Phase::Serving(s) if s.moving.is_some() => st.waiting.push_back(req),
            Phase::Serving(_) => self.run(o, req),
            Phase::Starting | Phase::Leaving => st.waiting.push_back(req),
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
        let (now, trust, ack_one, compact) = (self.now, self.lease_trust_ms, self.bugs.ack_one, self.compact_segs);
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        if s.sealing.is_some() || s.batch.is_empty() {
            return;
        }
        let reqs = std::mem::take(&mut s.batch);
        // The read lease: every peer acknowledged a call sent at `from` or
        // later, so none can be fenced before `from` + their promise.
        let lease = match s.peers.iter().map(|p| if p.down { None } else { p.acked_at }).collect::<Option<Vec<u64>>>() {
            Some(at) if trust > 0 && !at.is_empty() => at.into_iter().min().unwrap() + trust,
            _ => 0,
        };
        if std::mem::take(&mut s.dirty) {
            s.sealing = Some(reqs);
            let act = s.act;
            // A long-lived object's uploads pile up too: every so often, an
            // image as of this entry.
            let image = (s.since_image >= compact && s.image_at.is_none()).then(|| {
                let n = s.made + 1;
                s.since_image = 0;
                s.image_at = Some(n);
                s.cut = Some(n);
                image_key_at(o, s.epoch, n)
            });
            self.out.push(Effect::Seal { obj: o, act, image });
        } else if now < lease && {
            // Every entry made so far is acknowledged, as a write's answer
            // needs: then nothing a read saw can be lost.
            let has = |p: &PeerSt| p.have >= s.made;
            s.uploaded >= s.made || if ack_one { s.peers.iter().any(has) } else { s.peers.iter().all(has) }
        } {
            // Reads under the lease, with every earlier write acknowledged:
            // done now, no round. Past half the lease, a round renews it.
            // Whatever still waits for a round (reads that came while the
            // lease had lapsed) is proven now too, and goes first, so
            // answers keep their order. Waiting on them instead would never
            // end where a round takes longer than the gap between requests
            // (an object's beat is 50 ms; a round to another continent, 200).
            let epoch = s.epoch;
            if lease - now < trust / 2 {
                s.round += 1;
            }
            let gc = s.pends.iter().filter_map(|p| p.gc).max();
            let waiting: Vec<ReqId> = s.pends.drain(..).flat_map(|p| p.reqs).collect();
            self.stats.lease_reads += (waiting.len() + reqs.len()) as u64;
            for r in waiting.into_iter().chain(reqs) {
                self.answer(r, Answer::Done { epoch });
            }
            if let Some(n) = gc {
                self.gc_list(o, epoch, n);
            }
            self.kick(o);
        } else {
            s.round += 1;
            s.pends.push_back(Pend { need: s.made, round: s.round, since: now, reqs, gc: None });
            self.kick(o);
        }
    }

    /// Start whatever calls are due: peer appends, an upload, a proof.
    fn kick(&mut self, o: Obj) {
        let (trunc_early, skip_proof) = (self.bugs.trunc_early, self.bugs.skip_proof);
        let (now, upload_ms, upload_bytes, wait_when_down) = (self.now, self.upload_ms, self.upload_bytes, self.bugs.wait_when_down);
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
            peer_calls.push((Wait::Append(o, e, i, s.round, now), p.node, PeerCall::Append { obj: o, epoch: e, from, ents, trunc }));
        }
        let mut calls = vec![];
        // Uploads wait only while every peer is up: otherwise the bucket is
        // how writes are acknowledged. A retry must go as it went.
        let due = s.retry_upto.is_some()
            || s.flush
            || s.peers.is_empty()
            || (!wait_when_down && s.peers.iter().any(|p| p.down))
            || s.bytes >= upload_bytes
            || s.since.is_some_and(|t| now.saturating_sub(t) >= upload_ms);
        if due && !s.uploading && !s.upload_wait && s.uploaded < s.made {
            s.uploading = true;
            s.flush = false;
            if s.retry_upto.is_none() {
                s.since = None;
            }
            let upto = match (s.retry_upto, s.cut) {
                (Some(r), _) => r,
                (None, Some(c)) if c > s.uploaded => c.min(s.made),
                _ => s.made,
            };
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
            self.stats.uploads += matches!(w, Wait::Upload(..)) as u64;
            self.call(w, c);
        }
        self.settle(o);
    }

    /// Acknowledge every waiting batch that's confirmed.
    fn settle(&mut self, o: Obj) {
        let ack_one = self.bugs.ack_one;
        let Phase::Serving(s) = &mut self.st(o).phase else { return };
        let e = s.epoch;
        let (mut done, mut by, mut gc) = (vec![], (0, 0), None);
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
            let p = s.pends.pop_front().unwrap();
            gc = gc.max(p.gc);
            done.extend(p.reqs);
        }
        self.stats.peer_acks += by.0;
        self.stats.bucket_acks += by.1;
        for r in done {
            self.answer(r, Answer::Done { epoch: e });
        }
        if let Some(n) = gc {
            self.gc_list(o, e, n);
        }
        self.try_leave(o);
    }

    /// A move (`Event::Move`) that has settled: everything that ran is
    /// acknowledged and uploaded. Drop it, and seal the record (with no
    /// peers too: sealed is what lets the new node claim at once instead of
    /// sending requests back to us).
    fn try_leave(&mut self, o: Obj) {
        let seal_early = self.bugs.seal_early;
        let st = self.st(o);
        let Phase::Serving(s) = &st.phase else { return };
        let Some((to, _)) = s.moving else { return };
        if !s.idle(seal_early) {
            return;
        }
        let Phase::Serving(s) = std::mem::replace(&mut st.phase, Phase::Leaving) else { unreachable!() };
        self.out.push(Effect::Drop { obj: o });
        let r = Rec { node: self.node, epoch: s.epoch, sealed: true, peers: s.peers.iter().map(|p| p.node).collect(), sealed_from: s.sealed_from };
        self.call(Wait::Leave(o, to), Call::Put(own_key(o), Val::Rec(r), Cond::IfMatch(s.etag)));
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
        let waiting: Vec<ReqId> = st.waiting.drain(..).collect();
        st.early = None;
        st.fence = None;
        self.out.push(Effect::Drop { obj: o });
        for r in failed {
            self.answer(r, Answer::Failed);
        }
        for r in waiting {
            self.answer(r, Answer::NotRun);
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
            (Wait::SealRec, _) => {}
            // Sealed: it's `to`'s to claim, and what waited goes there. Not
            // (someone took it over, or we can't tell): what waited is
            // routed again from scratch.
            // A reseat (to us): claim the next epoch now; what waited is
            // served there. (Not sealed: someone took it over, as below.)
            (Wait::Leave(o, to), Res::Put(Ok(_))) if to == self.node => {
                self.stats.reseats += 1;
                self.st(o).phase = Phase::Starting;
                self.call(Wait::ReadRec(o), Call::Get(own_key(o)));
            }
            (Wait::Leave(o, to), res) => {
                let st = self.st(o);
                st.phase = Phase::Idle;
                let waiting: Vec<ReqId> = st.waiting.drain(..).collect();
                let moved = matches!(res, Res::Put(Ok(_)));
                if moved {
                    self.hints.insert(o, (to, now, true));
                    self.stats.moves += 1;
                    self.out.push(Effect::Handed { obj: o, to });
                }
                for r in waiting {
                    self.answer(r, if moved { Answer::Moved(to) } else { Answer::NotRun });
                }
            }

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
                    // Heard from lately: it's alive, ask there. Silent for a
                    // whole lease of our own time: it's gone, take over.
                    let last = if self.bugs.assume_alive { now } else { self.heard.get(&r.node).copied().unwrap_or(self.born.unwrap_or(now)) };
                    if now.saturating_sub(last) < self.lease_ms {
                        self.hints.insert(o, (r.node, now, false));
                        let st = self.st(o);
                        st.phase = Phase::Idle;
                        let rs: Vec<ReqId> = st.waiting.drain(..).collect();
                        for q in rs {
                            self.answer(q, Answer::Redirect(r.node));
                        }
                        return;
                    }
                    self.cas(o, Some(etag), r.epoch + 1);
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
            (Wait::Fence(o, e, n), res) => {
                let now = self.now;
                let st = self.st(o);
                let Some(f) = st.fence.as_mut().filter(|f| f.epoch == e) else { return };
                // It promised the old owner a lease: ask again once it's out
                // (on a tick), the call still counted as out.
                if let Res::Peer(PeerRes::Wait(ms)) = res {
                    f.again.push((n, now + ms));
                    self.stats.fence_waits += 1;
                    return;
                }
                f.out -= 1;
                if let Res::Peer(PeerRes::Tails { born, tails }) = res {
                    f.replies.push((born, tails));
                }
                let first = f.listed == 0 && !f.replies.is_empty();
                let rest = f.waiting && f.out == 0;
                if first || rest {
                    f.listed = f.replies.len();
                    f.waiting = false;
                    if first {
                        self.stats.fences += 1;
                    }
                    self.call(Wait::List(o, e), Call::List(obj_prefix(o)));
                } else if f.out == 0 && f.replies.is_empty() {
                    // Every peer lost: give up; the next request starts over.
                    self.drop_obj(o);
                }
            }
            (Wait::List(o, e), Res::Listed(keys)) => self.history(o, e, keys, None),
            (Wait::PrevManifest(o, e, segs, uploaded, q), Res::Got(Some((Val::Manifest(m), _)))) => self.from_prev(o, e, m, segs, uploaded, q),
            (Wait::TailSeg(o, e, mut base), Res::Put(Ok(_))) => {
                base.push(tail_key(o, e));
                self.write_manifest(o, e, base);
            }
            // Compaction: what our image replaced goes.
            (Wait::GcList(o, e, n), Res::Listed(keys)) => {
                // The proven image, which must be there.
                if !keys.contains(&if n == 0 { image_key(o, e) } else { image_key_at(o, e, n) }) {
                    return;
                }
                let n = if self.bugs.gc_newest_image {
                    keys.iter().filter_map(|k| parse_key(o, k).filter(|&(ep, p)| ep == e && p.starts_with('i')).and_then(|(_, p)| part_upto(p))).max().unwrap_or(n)
                } else {
                    n
                };
                // Older epochs' keys, and our own segments and images it holds.
                let old: Vec<String> = keys
                    .into_iter()
                    .filter(|k| match parse_key(o, k) {
                        Some((ep, _)) if ep < e => true,
                        Some((ep, part)) if ep == e && (part.starts_with('s') || part.starts_with('i')) => part_upto(part).is_some_and(|u| u < n || (u == n && part.starts_with('s'))),
                        _ => false,
                    })
                    .take(GC_MAX)
                    .collect();
                for k in old {
                    self.call(Wait::GcDelete, Call::Delete(k));
                }
            }
            (Wait::GcDelete, Res::Deleted) => self.stats.gc_deleted += 1,
            (Wait::GcList(..) | Wait::GcDelete, _) => {}
            (Wait::Manifest(o, e), Res::Put(Ok(_))) => {
                let segs = self.pending_base.remove(&(o, e)).unwrap_or_default();
                // A long list, or a bucket full of old keys (an image lets them go).
                let crowded = self.crowded.remove(&(o, e));
                let image = (segs.len() >= self.compact_segs || crowded).then(|| image_key(o, e));
                self.next_act += 1;
                let act = self.next_act;
                let st = self.st(o);
                st.act = act;
                st.epoch = e;
                self.out.push(Effect::Restore { obj: o, act, segs, image });
            }

            // ---- 3. serve ----
            // On a retry, "already there" means the lost one landed: only
            // this epoch's owner writes its keys.
            (Wait::Upload(o, e, upto, retry), Res::Put(r)) if r.is_ok() || retry => {
                let Some(s) = self.serving_in(o, e) else { return };
                s.uploading = false;
                s.retry_upto = None;
                s.bytes -= s.ents.drain(..(upto - s.uploaded) as usize).map(|e| e.len()).sum::<usize>();
                s.uploaded = upto;
                s.since_image += 1;
                if s.cut.is_some_and(|c| c <= upto) {
                    s.cut = None;
                }
                // The image's entries are all in the bucket now: it may go up.
                if s.image_at.is_some_and(|n| n <= upto) {
                    s.image_at = None;
                    let act = s.act;
                    self.out.push(Effect::PutImage { obj: o, act });
                }
                self.kick(o);
            }
            (Wait::Append(o, e, i, round, sent), res) => {
                let trusts_kept = self.bugs.lease_trusts_kept;
                let Some(s) = self.serving_in(o, e) else { return };
                let p = &mut s.peers[i];
                p.busy = false;
                match res {
                    Res::Peer(r @ (PeerRes::Have(_) | PeerRes::Kept(_) | PeerRes::Reborn(_))) => {
                        let (PeerRes::Have(n) | PeerRes::Kept(n) | PeerRes::Reborn(n)) = r else { unreachable!() };
                        // It lost its disk this epoch: what it has counts for
                        // writes, but no takeover can use it. Reseat.
                        if matches!(r, PeerRes::Reborn(_)) {
                            s.reseat = true;
                        }
                        // One call in flight per peer: its answer is the truth.
                        p.have = n;
                        p.round = p.round.max(round);
                        p.down = false;
                        // Kept (or Reborn): it has them, but promised nothing
                        // (a newer owner waits there), so the lease can't
                        // count on it.
                        if matches!(r, PeerRes::Have(_)) || trusts_kept {
                            p.acked_at = Some(p.acked_at.map_or(sent, |a| a.max(sent)));
                        }
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
        let sealed_from = match &prev {
            Some(r) if r.sealed => r.epoch,
            Some(r) => r.sealed_from,
            None => 0,
        };
        let peers = if self.must_fence(&prev) { prev.unwrap().peers } else { self.peers_of(o) };
        self.st(o).peers = peers.clone();
        self.st(o).sealed_from = sealed_from;
        let v = Val::Rec(Rec { node: self.node, epoch: next, sealed: false, peers, sealed_from });
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
            self.st(o).fence = None;
            return self.call(Wait::List(o, e), Call::List(obj_prefix(o)));
        }
        let peers = prev.unwrap().peers;
        self.st(o).fence = Some(FenceSt { epoch: e, out: peers.len(), replies: vec![], listed: 0, waiting: false, again: vec![] });
        for n in peers {
            self.peer(Wait::Fence(o, e, n), n, PeerCall::Fence { obj: o, epoch: e });
        }
    }

    /// The base for epoch `e` from epoch q: what q started from (`m`: its
    /// manifest's keys, or its image), q's uploads, and a fenced peer's
    /// tail of q.
    fn from_prev(&mut self, o: Obj, e: u32, m: Vec<String>, segs: Vec<String>, uploaded: u64, q: u32) {
        let mut base = m;
        base.extend(segs);
        // A fenced peer's tail of q, if any (no fence: nothing to add).
        let trust_wiped = self.bugs.trust_wiped;
        let st = self.st(o);
        // q was sealed: the bucket has all of it, and the peers we fenced
        // (picked after it) may hold none of it. The fence is what counts.
        let sealed_q = q > 0 && st.prev.as_ref().is_some_and(|r| r.sealed_from == q);
        let tails = match st.fence.as_mut().filter(|f| f.epoch == e) {
            None => vec![],
            Some(_) if sealed_q => vec![],
            Some(f) => match f.replies[..f.listed].iter().position(|(born, _)| trust_wiped || q > *born) {
                Some(i) => std::mem::take(&mut f.replies[i].1),
                // None of them can speak for q: they lost their disk
                // since. Wait for the other peers, then list again.
                None if f.out > 0 => {
                    f.waiting = true;
                    return;
                }
                None if f.listed < f.replies.len() => {
                    f.listed = f.replies.len();
                    return self.call(Wait::List(o, e), Call::List(obj_prefix(o)));
                }
                None => return self.drop_obj(o),
            },
        };
        self.st(o).fence = None;
        // What that peer had of q past its uploads.
        let more: Vec<Vec<u8>> = tails
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

    /// Compaction, once our image is written and a round after it proved we
    /// still own the object: every key of an older epoch goes. Safe because
    /// a newer owner lists only after that round (it must fence our peers
    /// first, or, with no peers, the record named us then), so it sees our
    /// manifest and image and starts from them or from something newer; our
    /// image holds everything our manifest named. Only a stale claimer, still
    /// loading from before us, could miss a key, and its restore then fails
    /// (safe: it's stale).
    /// `n`: the image this round proved (`i`: 0, `i<N>`: N).
    fn gc_list(&mut self, o: Obj, e: u32, n: u64) {
        self.call(Wait::GcList(o, e, n), Call::List(obj_prefix(o)));
    }

    /// From a listing: the last epoch q before `e` with a manifest, plus q's
    /// uploads. `early`: this is the wrong, before-the-CAS version.
    fn history(&mut self, o: Obj, e: u32, keys: Vec<String>, early: Option<Option<String>>) {
        if keys.len() >= 4 * self.compact_segs {
            self.crowded.insert((o, e));
        }
        let mut manifests = vec![];
        let mut images: HashMap<u32, u64> = HashMap::new();
        let mut segs: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for k in &keys {
            match parse_key(o, k) {
                // A newer epoch has served: we're stale, and the peers may
                // have dropped what we'd need. Stop.
                Some((ep, "m")) if ep > e && early.is_none() => return self.drop_obj(o),
                Some((ep, "m")) if ep < e => manifests.push(ep),
                Some((ep, part)) if ep < e && part.starts_with('i') => {
                    if let Some(n) = part_upto(part) {
                        let best = images.entry(ep).or_insert(n);
                        *best = (*best).max(n);
                    }
                }
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
        // Uploads go one at a time, in order: the last one says how many
        // (or q's newest image, if compaction took the segments it holds).
        let uploaded = ups.last().and_then(|k| parse_key(o, k)?.1[1..].parse().ok()).unwrap_or(0);
        // q's image is what q restored from its manifest: start there instead
        // (and skip reading the manifest).
        if let (None, Some(&n)) = (&early, images.get(&q)) {
            let key = if n == 0 { image_key(o, q) } else { image_key_at(o, q, n) };
            let after: Vec<String> = ups.into_iter().filter(|k| parse_key(o, k).and_then(|(_, p)| part_upto(p)).is_some_and(|u| u > n)).collect();
            return self.from_prev(o, e, vec![key], after, uploaded.max(n), q);
        }
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
    /// Spread placement: the fleet's nodes share the objects, every core
    /// agrees on who gets which, and a node gone quiet gets none.
    #[test]
    fn spread_places_by_rendezvous_among_live_nodes() {
        let core = |me| Core::with_lease(me, Bugs::default(), 1000).replicate(vec![0, 1, 2], 2);
        let objs: Vec<Obj> = (0..64).map(|n| obj_id(&format!("acme/million/chunk/{n}"))).collect();
        let (mut a, b) = (core(0), core(1));
        a.on(0, Event::Tick);
        let at = |c: &Core, now, o| c.place(now, o, &[0, 1, 2]).unwrap_or(c.node);
        let mut count = [0; 3];
        for &o in &objs {
            assert_eq!(at(&a, 0, o), at(&b, 0, o), "every node agrees");
            count[at(&a, 0, o) as usize] += 1;
        }
        assert!(count.iter().all(|&n| n >= 10), "spread: {count:?}");
        // Node 2 silent for a lease (node 1 heard): its share goes elsewhere, the rest stay.
        a.on(1500, Event::Heard { node: 1 });
        for &o in &objs {
            let before = at(&a, 0, o);
            let after = at(&a, 1500, o);
            assert_ne!(after, 2);
            if before != 2 {
                assert_eq!(after, before, "only node 2's objects move");
            }
        }
    }

    /// Bucket values come from storage anyone may have damaged: the
    /// decoders must never panic.
    #[test]
    fn mangled_bucket_values_never_panic() {
        let o: Obj = 0xabc;
        let rec = Val::Rec(Rec { node: 2, epoch: 7, sealed: false, peers: vec![0, 1], sealed_from: 0 }).encode();
        let man = Val::Manifest(vec![seg_key(o, 1, 4), tail_key(o, 2)]).encode();
        let seg = frame(&[vec![1, 2, 3], vec![9; 100]]);
        let mut rng = crate::sim::Rng::new(23);
        for i in 0..100_000 {
            let (k, good) = [(own_key(o), &rec), (manifest_key(o, 2), &man), (seg_key(o, 1, 4), &seg)][i % 3].clone();
            let b = crate::sim::mangle(&mut rng, good);
            std::panic::catch_unwind(|| (Val::decode(&k, &b), unframe(&b))).expect("a bucket value decoder panicked");
        }
    }

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
        let PeerRes::Tails { tails: t, .. } = p.handle(PeerCall::Fence { obj: 1, epoch: 2 }) else { panic!() };
        assert_eq!((t[0].epoch, t[0].from, t[0].ents.clone()), (1, 5, vec![e(6)]));
        assert!(app(&mut p, 1, 7, vec![e(7)], 0).is_none(), "a fenced epoch is refused");
        assert_eq!(app(&mut p, 2, 1, vec![e(9)], 0), Some(1));
        let PeerRes::Tails { tails: t, .. } = p.handle(PeerCall::Fence { obj: 1, epoch: 3 }) else { panic!() };
        assert_eq!(t.len(), 1, "older epochs go once a newer one appends");
        // A snapshot rebuilds the same store.
        let mut q = PeerStore::default();
        for c in p.snapshot() {
            q.handle(c);
        }
        assert_eq!(q.handle(PeerCall::Fence { obj: 1, epoch: 3 }), p.handle(PeerCall::Fence { obj: 1, epoch: 3 }));
        assert!(app(&mut q, 2, 2, vec![e(1)], 0).is_none(), "the fence survives");
        // A log with dropped entries survives a snapshot too.
        assert_eq!(app(&mut p, 4, 3, vec![e(3)], 2), Some(3));
        let mut q = PeerStore::default();
        for c in p.snapshot() {
            q.handle(c);
        }
        assert_eq!(q.handle(PeerCall::Fence { obj: 1, epoch: 5 }), p.handle(PeerCall::Fence { obj: 1, epoch: 5 }));
    }

    #[test]
    fn a_wiped_store_relearns_its_fence_and_says_what_it_knows() {
        let mut p = PeerStore::wiped();
        let call = PeerCall::Append { obj: 1, epoch: 2, from: 1, ents: vec![vec![1]], trunc: 0 };
        assert_eq!(p.must_learn(&call), Some(1));
        p.handle(PeerCall::Learn { obj: 1, epoch: 3, born: 3 });
        assert_eq!(p.must_learn(&call), None);
        assert_eq!(p.handle(call), PeerRes::Refused, "epoch 2 is older than the record: fenced");
        p.handle(PeerCall::Append { obj: 1, epoch: 3, from: 1, ents: vec![vec![1]], trunc: 0 });
        let PeerRes::Tails { born, .. } = p.handle(PeerCall::Fence { obj: 1, epoch: 4 }) else { panic!() };
        assert_eq!(born, 3, "it can't speak for epoch 3 or older");
        let mut q = PeerStore::wiped();
        for c in p.snapshot() {
            q.handle(c);
        }
        assert_eq!(q.handle(PeerCall::Fence { obj: 1, epoch: 4 }), p.handle(PeerCall::Fence { obj: 1, epoch: 4 }));
        // Epoch 3 began after it came back: it saw all of it (and still
        // refuses the ones before the record).
        let mut p = PeerStore::wiped();
        p.handle(PeerCall::Learn { obj: 1, epoch: 3, born: 2 });
        assert_eq!(p.handle(PeerCall::Append { obj: 1, epoch: 2, from: 1, ents: vec![vec![1]], trunc: 0 }), PeerRes::Refused);
        assert_eq!(p.handle(PeerCall::Append { obj: 1, epoch: 3, from: 1, ents: vec![vec![1]], trunc: 0 }), PeerRes::Have(1), "not Reborn: it speaks for 3");
        let PeerRes::Tails { born, .. } = p.handle(PeerCall::Fence { obj: 1, epoch: 4 }) else { panic!() };
        assert_eq!(born, 2);
        assert!(p.speaks_for(1, 3) && !p.speaks_for(1, 2));
        let mut q = PeerStore::wiped();
        for c in p.snapshot() {
            q.handle(c);
        }
        assert_eq!(q.handle(PeerCall::Fence { obj: 1, epoch: 5 }), p.handle(PeerCall::Fence { obj: 1, epoch: 5 }), "a snapshot keeps it");
    }

    #[test]
    fn frames_round_trip() {
        let ents = vec![vec![], vec![1, 2, 3], vec![9; 300]];
        assert_eq!(unframe(&frame(&ents)), Some(ents));
        assert_eq!(unframe(&[5, 0, 0, 0, 1]), None);
    }
}
