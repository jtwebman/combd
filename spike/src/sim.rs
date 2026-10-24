//! A deterministic simulator for D2 ownership and D3 peer writes. It runs the
//! real decision core (`own.rs`) in several processes against a fake bucket
//! and real peer stores, with clients sending writes and reads, and a world
//! that misbehaves:
//! - bucket calls take 1 to 50 ms, sometimes 500 ms; peer calls 1 to 5 ms,
//!   sometimes 500 ms,
//! - requests and replies get lost (the caller times out and can't tell),
//! - a peer store doesn't answer while its node is down or frozen (it keeps
//!   its entries: it's on disk),
//! - with `splits`, the network splits for 0.5 to 5 s at a time: a node cut
//!   off from the bucket (both ways, or one way), from the other nodes, or
//!   from both while clients still reach it; the nodes in two groups; or one
//!   node's messages to another dropped one way,
//! - clocks run fast or slow, by up to 10%,
//! - processes crash, and their supervisor starts a new one,
//! - processes freeze; the supervisor may start a new one, and the frozen
//!   one later wakes up still thinking it owns things,
//! - heartbeats between nodes (how they tell who's alive) go the same way
//!   as everything else: lost, late, or cut by a split,
//! - with `loss`, one node dies for good partway through: what it owned must
//!   be taken over.
//!
//! Each process also plays the executor: it keeps an object's data (a list
//! of writes), seals segments, and restores from them, through the bucket,
//! the way a real node does with SQLite.
//!
//! One seed gives one exact run, so any failure replays.
//!
//! An oracle checks every run: one writer per epoch, no lost acknowledged
//! write, no history rewrite (the invariants in `spec/Ownership.tla` and
//! `spec/Replication.tla`), and
//! that a read sees every write acknowledged before the read was sent. And
//! liveness: once the faults stop, every object takes a write (a probe per
//! object), so a node that never takes over a dead one's objects is caught.

use crate::own::*;
use std::collections::{BTreeMap, HashMap};

/// Fuzzing: `good`, damaged 1 to 4 ways at random (a bit flipped, cut
/// short, a byte added, a huge length pasted in).
pub fn mangle(rng: &mut Rng, good: &[u8]) -> Vec<u8> {
    let mut b = good.to_vec();
    for _ in 0..1 + rng.below(4) {
        if b.is_empty() {
            b.push(0);
        }
        let at = rng.below(b.len() as u64) as usize;
        match rng.below(4) {
            0 => b[at] ^= 1 << rng.below(8),
            1 => b.truncate(at),
            2 => b.insert(at, rng.next() as u8),
            _ => b.splice(at..at, [0xff, 0xff, 0xff, 0x7f]).for_each(drop),
        }
    }
    b
}

/// A write, as the simulator's clients make them.
pub type W = u64;

/// splitmix64: small, fast, and the same everywhere.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}

pub struct Settings {
    pub nodes: u32,
    pub objects: u32,
    pub clients: u32,
    /// Simulated ms of traffic (faults stop after it, then things settle).
    pub duration: u64,
    /// Chance a bucket request, or its reply, is lost.
    pub lost: f64,
    /// Chances per process per tick (every 200 ms).
    pub crash: f64,
    pub freeze: f64,
    /// Peers per object: 0 is D2, 2 is D3.
    pub replicas: usize,
    /// Lasting network splits, about one every 3 s.
    pub splits: bool,
    /// One node's disk is lost (once per run, at a crash): its peer store
    /// comes back wiped.
    pub wipes: bool,
    /// One node dies for good, partway through.
    pub loss: bool,
    /// Read leases (`Core::read_lease`, `PeerStore::promise_ms`).
    pub lease: bool,
    /// How long D3 uploads may wait, and how many bytes may build up first
    /// (`None`: picked by the seed, from no wait to 5 s).
    pub uploads: Option<(u64, usize)>,
    /// Each object beats every 50 ms where it's served (`tick_every`): a
    /// request no one waits on, so it needs no proof.
    pub beats: bool,
    /// Peer round trips in this range, ms (`None`: 1 to 5, a data center).
    pub peer_ms: Option<(u64, u64)>,
    /// Now and then a process hands an object it serves to another node
    /// (`Event::Move`), as placement does.
    pub moves: bool,
    pub bugs: Bugs,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { nodes: 3, objects: 2, clients: 6, duration: 20_000, lost: 0.03, crash: 0.005, freeze: 0.005, replicas: 0, splits: false, wipes: false, loss: false, lease: true, uploads: None, beats: false, peer_ms: None, moves: false, bugs: Bugs::default() }
    }
}

#[derive(Default, Debug)]
pub struct Stats {
    pub writes_acked: u64,
    pub reads_done: u64,
    pub failed: u64,
    /// Of `failed`: answered "never ran", and checked that it didn't.
    pub not_run: u64,
    pub redirects: u64,
    pub epochs: u64,
    pub crashes: u64,
    pub freezes: u64,
    pub peer_acks: u64,
    pub bucket_acks: u64,
    pub fences: u64,
    pub tail_entries: u64,
    pub uploads: u64,
    pub lease_reads: u64,
    /// Keys compaction deleted.
    pub gc_deleted: u64,
    /// Objects moved to another node, done.
    pub moves: u64,
    /// The longest a probe took to get a write in, from when they start.
    pub probe_ms: u64,
    pub splits: u64,
    pub wipes: u64,
    /// The longest time any object went without an acknowledged write, in ms
    /// (while traffic was running).
    pub max_gap: u64,
}

/// A call's give-up time: after this, the caller hears `Lost`.
const TIMEOUT: u64 = 400;
const CLIENT_TIMEOUT: u64 = 5_000;

enum Msg {
    /// To a process: an event from the outside world.
    Proc(usize, Event),
    /// A bucket call arriving at the bucket.
    Bucket(usize, OpId, Call, bool),
    /// A peer call arriving at a node's peer store.
    Peer(usize, OpId, Node, PeerCall, bool),
    /// An answer reaching a client.
    Client(ReqId, Answer, usize),
    ClientNext(u32),
    ClientTimeout(ReqId),
    Tick(usize),
    /// A process's objects beat.
    Beat(usize),
    Restart(Node),
    Thaw(usize),
    /// Start the next network split.
    Split,
}

/// One end of a network link: a node, or the bucket (`None`).
type End = Option<Node>;

/// A peer store answers a call; a wiped one first relearns the object from
/// the bucket, as a real one does.
fn store_call(stores: &mut HashMap<Node, PeerStore>, bucket: &BTreeMap<String, (Val, u64)>, node: Node, call: PeerCall, now: u64) -> PeerRes {
    let st = stores.get_mut(&node).expect("every node's store is made at its spawn");
    if let Some(obj) = st.must_learn(&call) {
        let epoch = match bucket.get(&own_key(obj)) {
            Some((Val::Rec(r), _)) => r.epoch,
            _ => 0,
        };
        st.handle(PeerCall::Learn { obj, epoch });
    }
    st.handle_at(call, now)
}

/// Read leases in the simulator: peers promise half a lease.
const PROMISE_MS: u64 = LEASE_MS / 2;

/// What a process holds for one object it serves: its data.
#[derive(Default)]
struct Data {
    history: Vec<W>,
    unsealed: Vec<W>,
    /// An image taken at a seal, until it's put.
    image: Option<(String, Vec<u8>)>,
}

struct Proc {
    node: Node,
    core: Core,
    alive: bool,
    frozen: bool,
    /// Events held while frozen.
    held: Vec<Event>,
    rate: f64,
    off: u64,
    data: HashMap<Obj, Data>,
    /// The history each request saw, for its answer.
    saw: HashMap<ReqId, Vec<W>>,
}

struct Pending {
    client: u32,
    obj: Obj,
    write: Option<W>,
    sent: u64,
}

pub struct Sim {
    rng: Rng,
    s: Settings,
    now: u64,
    seq: u64,
    queue: BTreeMap<(u64, u64), Msg>,
    bucket: BTreeMap<String, (Val, u64)>,
    version: u64,
    /// Each node's peer store: on disk, so it outlives the node's processes.
    stores: HashMap<Node, PeerStore>,
    /// Links that are down: (from, to, until).
    cuts: Vec<(End, End, u64)>,
    /// When each object last had a write acknowledged.
    last_ack: HashMap<Obj, u64>,
    procs: Vec<Proc>,
    /// The process a node's traffic goes to (its supervisor's latest).
    current: HashMap<Node, usize>,
    next_req: ReqId,
    next_w: W,
    pending: HashMap<ReqId, Pending>,
    /// Every request ever sent: what it does (a process may run it after
    /// its client gave up).
    ops: HashMap<ReqId, (Obj, Option<W>)>,
    pub stats: Stats,
    // The oracle's notes.
    /// Write -> (object, epoch, when the client heard).
    acked: HashMap<W, (Obj, u32, u64)>,
    /// (object, epoch) -> processes that answered `Done` in it.
    answered_by: HashMap<(Obj, u32), Vec<usize>>,
    /// (object, epoch) -> the longest history any answer showed.
    proven: HashMap<(Obj, u32), Vec<W>>,
    /// The node lost for good, if one was.
    gone: Option<Node>,
    /// After the faults: has each object taken a write yet?
    probes: BTreeMap<Obj, bool>,
    /// Beats in flight (no client, no proof).
    beats: std::collections::HashSet<ReqId>,
}

/// Probe clients (one per object, after the faults) have ids from here.
const PROBE: u32 = 1_000_000;

/// An object's beat, as in the arena's matches.
const BEAT_MS: u64 = 50;

fn is_prefix(a: &[W], b: &[W]) -> bool {
    a.len() <= b.len() && b[..a.len()] == *a
}

fn encode(ws: &[W]) -> Vec<u8> {
    ws.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn decode(b: &[u8]) -> Vec<W> {
    b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

/// The writes in a segment: its entries, in order.
fn decode_seg(b: &[u8]) -> Option<Vec<W>> {
    Some(unframe(b)?.iter().flat_map(|e| decode(e)).collect())
}

impl Sim {
    pub fn new(seed: u64, mut s: Settings) -> Sim {
        s.uploads.get_or_insert([(0, 1 << 20), (300, 1 << 20), (2000, 1 << 20), (5000, 40)][(seed % 4) as usize]);
        let mut sim = Sim {
            rng: Rng::new(seed),
            s,
            now: 0,
            seq: 0,
            queue: BTreeMap::new(),
            bucket: BTreeMap::new(),
            version: 0,
            stores: HashMap::new(),
            cuts: vec![],
            last_ack: HashMap::new(),
            procs: vec![],
            current: HashMap::new(),
            next_req: 0,
            next_w: 0,
            pending: HashMap::new(),
            ops: HashMap::new(),
            stats: Stats::default(),
            acked: HashMap::new(),
            answered_by: HashMap::new(),
            proven: HashMap::new(),
            gone: None,
            probes: BTreeMap::new(),
            beats: Default::default(),
        };
        for n in 0..sim.s.nodes {
            sim.spawn(n);
        }
        // After the faults: one probe per object, until it takes a write.
        let t = sim.s.duration + 2000;
        for o in 0..sim.s.objects {
            sim.at(t, Msg::ClientNext(PROBE + o));
        }
        if sim.s.splits {
            let t = 500 + sim.rng.below(3000);
            sim.at(t, Msg::Split);
        }
        for c in 0..sim.s.clients {
            let t = sim.rng.below(100);
            sim.at(t, Msg::ClientNext(c));
        }
        sim
    }

    fn at(&mut self, after: u64, m: Msg) {
        self.seq += 1;
        self.queue.insert((self.now + after, self.seq), m);
    }

    fn spawn(&mut self, node: Node) {
        let rate = 0.9 + self.rng.below(200) as f64 / 1000.0;
        let off = self.rng.below(1_000_000);
        self.procs.push(Proc {
            node,
            core: {
                let (ms, bytes) = self.s.uploads.unwrap();
                // Images early, so they happen a lot.
                let c = Core::new(node, self.s.bugs).replicate((0..self.s.nodes).collect(), self.s.replicas).uploads(ms, bytes).compact_at(3);
                if self.s.lease { c.read_lease(PROMISE_MS) } else { c }
            },
            alive: true,
            frozen: false,
            held: vec![],
            rate,
            off,
            data: HashMap::new(),
            saw: HashMap::new(),
        });
        let p = self.procs.len() - 1;
        self.current.insert(node, p);
        // Its peer store (kept across restarts, like a disk), told it started
        // again: it forgets its promises, so it holds off fences a lease.
        let (lease, ignores, now) = (self.s.lease, self.s.bugs.lease_no_promise, self.clock(p));
        let st = self.stores.entry(node).or_default();
        if lease {
            st.promise_ms = PROMISE_MS;
            st.ignores_promise = ignores;
        }
        st.started(now);
        self.at(0, Msg::Tick(p));
        if self.s.beats {
            self.at(BEAT_MS, Msg::Beat(p));
        }
    }

    fn clock(&self, p: usize) -> u64 {
        (self.now as f64 * self.procs[p].rate) as u64 + self.procs[p].off
    }

    fn latency(&mut self) -> u64 {
        if self.rng.chance(0.01) {
            500
        } else {
            1 + self.rng.below(50)
        }
    }

    fn peer_latency(&mut self) -> u64 {
        let (lo, hi) = self.s.peer_ms.unwrap_or((1, 5));
        if self.rng.chance(0.01) {
            500
        } else {
            lo + self.rng.below(hi - lo + 1)
        }
    }

    fn faulty(&self) -> bool {
        self.now < self.s.duration
    }

    /// Run until the queue is empty. Returns the first broken invariant.
    pub fn run(&mut self) -> Result<(), String> {
        while let Some(((t, _), m)) = self.queue.pop_first() {
            self.now = t;
            if self.now > self.s.duration + 60_000 {
                break; // settled long ago; only ticks remain
            }
            self.step(m)?;
        }
        self.check_bucket()?;
        // Liveness: every object took a write once the faults stopped.
        if let Some((o, _)) = self.probes.iter().find(|(_, done)| !**done) {
            return Err(format!("object {o} never took a write after the faults stopped (lost node: {:?})", self.gone));
        }
        // And soon: the worst seen is about 4 s (a takeover, then a write).
        if self.stats.probe_ms > 10 * crate::own::LEASE_MS {
            return Err(format!("a write took {} ms to get in after the faults stopped (lost node: {:?})", self.stats.probe_ms, self.gone));
        }
        Ok(())
    }

    /// A probe's next try: a new write, to some node that's still there.
    fn probe(&mut self, client: u32) {
        let obj = (client - PROBE) as Obj;
        if *self.probes.entry(obj).or_insert(false) {
            return;
        }
        let nodes: Vec<Node> = (0..self.s.nodes).filter(|&n| Some(n) != self.gone).collect();
        let node = nodes[self.rng.below(nodes.len() as u64) as usize];
        self.next_w += 1;
        let w = self.next_w;
        self.client_send(client, Some((obj, Some(w), node)), false);
    }

    fn step(&mut self, m: Msg) -> Result<(), String> {
        match m {
            Msg::Proc(p, ev) => self.deliver(p, ev),
            Msg::Beat(p) => {
                if !self.procs[p].alive {
                    return Ok(());
                }
                if self.now < self.s.duration + 30_000 {
                    self.at(BEAT_MS, Msg::Beat(p));
                }
                let mut objs: Vec<Obj> = self.procs[p].data.keys().copied().collect();
                objs.sort();
                for obj in objs {
                    self.next_req += 1;
                    let req = self.next_req;
                    self.beats.insert(req);
                    self.deliver(p, Event::Request { obj, req });
                }
            }
            Msg::Tick(p) => {
                if !self.procs[p].alive {
                    return Ok(());
                }
                if self.now < self.s.duration + 30_000 {
                    let next = (RENEW_MS as f64 / self.procs[p].rate) as u64;
                    self.at(next, Msg::Tick(p));
                }
                self.deliver(p, Event::Tick);
                // Heartbeats to every other node, over the network: lost,
                // late, or cut like any other message. A frozen process
                // sends nothing.
                if !self.procs[p].frozen {
                    let node = self.procs[p].node;
                    for m in (0..self.s.nodes).filter(|&m| m != node) {
                        let Some(&q) = self.current.get(&m) else { continue };
                        if self.cut(Some(node), Some(m)) || (self.faulty() && self.rng.chance(self.s.lost)) {
                            continue;
                        }
                        let d = self.peer_latency();
                        self.at(d, Msg::Proc(q, Event::Heard { node }));
                    }
                }
                // One node lost for good, partway through.
                if self.s.loss && self.gone.is_none() && self.faulty() && self.now > self.s.duration / 3 && self.rng.chance(0.02) {
                    let n = self.procs[p].node;
                    self.gone = Some(n);
                    for q in self.procs.iter_mut().filter(|q| q.node == n) {
                        q.alive = false;
                    }
                    return Ok(());
                }
                self.maybe_fault(p);
                // Eviction: now and then, let an object go.
                if self.rng.chance(0.02) {
                    let obj = self.rng.below(self.s.objects as u64) as Obj;
                    self.deliver(p, Event::Release { obj });
                }
                // Placement: now and then, hand an object to another node.
                if self.s.moves && self.faulty() && self.rng.chance(0.05) {
                    let obj = self.rng.below(self.s.objects as u64) as Obj;
                    let to = self.rng.below(self.s.nodes as u64) as Node;
                    self.deliver(p, Event::Move { obj, to });
                }
            }
            Msg::Bucket(p, op, call, lose_reply) => {
                let n = Some(self.procs[p].node);
                if self.cut(n, None) {
                    self.at(TIMEOUT, Msg::Proc(p, Event::Reply { op, res: Res::Lost }));
                    return Ok(());
                }
                let res = self.apply(&call);
                if self.cut(None, n) {
                    // It happened, but the node never hears.
                    self.at(TIMEOUT, Msg::Proc(p, Event::Reply { op, res: Res::Lost }));
                } else if !lose_reply {
                    let d = self.latency();
                    self.at(d, Msg::Proc(p, Event::Reply { op, res }));
                }
            }
            Msg::Peer(p, op, node, call, lose_reply) => {
                let up = self.current.get(&node).is_some_and(|&q| self.procs[q].alive && !self.procs[q].frozen);
                let from = self.procs[p].node;
                if !up || self.cut(Some(from), Some(node)) {
                    self.at(TIMEOUT, Msg::Proc(p, Event::Reply { op, res: Res::Lost }));
                } else if self.cut(Some(node), Some(from)) {
                    let now = self.clock(self.current[&node]);
                    store_call(&mut self.stores, &self.bucket, node, call, now);
                    self.at(TIMEOUT, Msg::Proc(p, Event::Reply { op, res: Res::Lost }));
                } else if !lose_reply {
                    let now = self.clock(self.current[&node]);
                    let res = Res::Peer(store_call(&mut self.stores, &self.bucket, node, call, now));
                    let d = self.peer_latency();
                    self.at(d, Msg::Proc(p, Event::Reply { op, res }));
                }
            }
            Msg::Client(req, ans, p) => self.client_answer(req, ans, p)?,
            Msg::ClientNext(c) if c >= PROBE => self.probe(c),
            Msg::ClientNext(c) => self.client_send(c, None, false),
            Msg::ClientTimeout(req) => {
                if let Some(pd) = self.pending.remove(&req) {
                    self.stats.failed += 1;
                    self.at(1, Msg::ClientNext(pd.client));
                }
            }
            Msg::Restart(n) if Some(n) == self.gone => {}
            Msg::Restart(n) => self.spawn(n),
            Msg::Split => {
                if self.faulty() {
                    self.split();
                    let t = 1000 + self.rng.below(4000);
                    self.at(t, Msg::Split);
                }
            }
            Msg::Thaw(p) => {
                self.procs[p].frozen = false;
                for ev in std::mem::take(&mut self.procs[p].held) {
                    self.deliver(p, ev);
                }
            }
        }
        Ok(())
    }

    fn cut(&self, from: End, to: End) -> bool {
        // A node's own peer store is its own disk: no network in between.
        from != to && self.cuts.iter().any(|&(a, b, until)| a == from && b == to && self.now < until)
    }

    /// Cut some links for 0.5 to 5 s.
    fn split(&mut self) {
        self.stats.splits += 1;
        let until = self.now + 500 + self.rng.below(4500);
        let nodes = self.s.nodes as u64;
        let n = self.rng.below(nodes) as Node;
        let others: Vec<Node> = (0..self.s.nodes).filter(|&m| m != n).collect();
        let mut links: Vec<(End, End)> = vec![];
        match self.rng.below(7) {
            // Cut off from the bucket: both ways, requests only, replies only.
            0 => links.extend([(Some(n), None), (None, Some(n))]),
            1 => links.push((Some(n), None)),
            2 => links.push((None, Some(n))),
            // Cut off from the other nodes.
            3 => links.extend(others.iter().flat_map(|&m| [(Some(n), Some(m)), (Some(m), Some(n))])),
            // Cut off from everything but its clients.
            4 => {
                links.extend([(Some(n), None), (None, Some(n))]);
                links.extend(others.iter().flat_map(|&m| [(Some(n), Some(m)), (Some(m), Some(n))]));
            }
            // Two groups of nodes; both still reach the bucket.
            5 => {
                let side: Vec<bool> = (0..self.s.nodes).map(|m| m == n || self.rng.chance(0.4)).collect();
                for a in 0..self.s.nodes {
                    for b in 0..self.s.nodes {
                        if side[a as usize] != side[b as usize] {
                            links.push((Some(a), Some(b)));
                        }
                    }
                }
            }
            // One way: n's messages to m are dropped, m's to n get through.
            _ => {
                let m = others[self.rng.below(others.len() as u64) as usize];
                links.push((Some(n), Some(m)));
            }
        }
        self.cuts.retain(|c| c.2 > self.now);
        self.cuts.extend(links.into_iter().map(|(a, b)| (a, b, until)));
    }

    fn maybe_fault(&mut self, p: usize) {
        if !self.faulty() || self.procs[p].frozen {
            return;
        }
        let node = self.procs[p].node;
        if self.rng.chance(self.s.crash) {
            self.stats.crashes += 1;
            self.procs[p].alive = false;
            // Now and then the disk goes with it (once per run: two lost
            // disks at once really can lose writes, and a node lost for good
            // is one: with both, an object whose two peers were those two
            // rightly stops rather than lose acknowledged writes).
            if self.s.wipes && !self.s.loss && self.stats.wipes == 0 && self.rng.chance(0.3) {
                self.stats.wipes += 1;
                let mut st = PeerStore::wiped();
                st.forgets_fence = self.s.bugs.wipe_forgets_fence;
                self.stores.insert(node, st);
            }
            let d = 50 + self.rng.below(500);
            self.at(d, Msg::Restart(node));
        } else if self.rng.chance(self.s.freeze) {
            // Frozen: a long GC pause, a stopped VM. The supervisor often
            // thinks it's dead and starts another; the frozen one wakes later.
            self.stats.freezes += 1;
            self.procs[p].frozen = true;
            let d = 500 + self.rng.below(5_000);
            self.at(d, Msg::Thaw(p));
            if self.rng.chance(0.7) {
                self.at(10, Msg::Restart(node));
            }
        }
    }

    fn deliver(&mut self, p: usize, ev: Event) {
        if !self.procs[p].alive {
            return;
        }
        if self.procs[p].frozen {
            self.procs[p].held.push(ev);
            return;
        }
        let now = self.clock(p);
        let effects = self.procs[p].core.on(now, ev);
        for e in effects {
            self.effect(p, e);
        }
    }

    /// One effect: a bucket call goes out; the rest is this process's
    /// executor, which answers by queuing an event back to its core.
    fn effect(&mut self, p: usize, e: Effect) {
        match e {
            Effect::Call { op, call } => {
                let lose_req = self.faulty() && self.rng.chance(self.s.lost);
                let lose_reply = self.faulty() && self.rng.chance(self.s.lost);
                if lose_req || lose_reply {
                    self.at(TIMEOUT, Msg::Proc(p, Event::Reply { op, res: Res::Lost }));
                }
                if !lose_req {
                    let d = self.latency();
                    self.at(d, Msg::Bucket(p, op, call, lose_reply));
                }
            }
            Effect::Peer { op, node, call } => {
                let lose_req = self.faulty() && self.rng.chance(self.s.lost);
                let lose_reply = self.faulty() && self.rng.chance(self.s.lost);
                if lose_req || lose_reply {
                    self.at(TIMEOUT, Msg::Proc(p, Event::Reply { op, res: Res::Lost }));
                }
                if !lose_req {
                    let d = self.peer_latency();
                    self.at(d, Msg::Peer(p, op, node, call, lose_reply));
                }
            }
            Effect::Answer { req, ans } => {
                if self.beats.remove(&req) {
                    return;
                }
                let d = self.latency();
                self.at(d, Msg::Client(req, ans, p));
            }
            Effect::Run { obj, act, req } => {
                let write = self.ops.get(&req).and_then(|(_, w)| *w);
                let d = self.procs[p].data.entry(obj).or_default();
                if let Some(w) = write {
                    d.history.push(w);
                    d.unsealed.push(w);
                }
                let h = d.history.clone();
                self.procs[p].saw.insert(req, h);
                let proof = !self.beats.contains(&req);
                self.at(0, Msg::Proc(p, Event::Ran { obj, act, req, changed: write.is_some(), proof }));
            }
            Effect::Seal { obj, act, image } => {
                let d = self.procs[p].data.entry(obj).or_default();
                let changes = encode(&std::mem::take(&mut d.unsealed));
                // As of this entry: everything run so far.
                d.image = image.map(|k| (k, encode(&d.history)));
                self.at(0, Msg::Proc(p, Event::Sealed { obj, act, changes }));
            }
            Effect::Restore { obj, act, segs, image } => {
                // Segments are write-once, so reading them all now is the same
                // as reading them one by one later.
                let mut history = vec![];
                let mut ok = true;
                for k in &segs {
                    match self.bucket.get(k).and_then(|(v, _)| if let Val::Seg(b) = v { decode_seg(b) } else { None }) {
                        Some(ws) => history.extend(ws),
                        None => ok = false,
                    }
                }
                // The image, as restored (and sometimes not written: a crash,
                // a lost PUT), so loads from it and without it both happen.
                if let Some(k) = image.filter(|_| ok && self.rng.chance(0.7)) {
                    if let Res::Put(Ok(_)) = self.apply(&Call::Put(k.clone(), Val::Seg(crate::own::frame(&[encode(&history)])), Cond::IfAbsent)) {
                        let d = self.latency();
                        self.at(d, Msg::Proc(p, Event::Imaged { obj, act, key: k }));
                    }
                }
                self.procs[p].data.insert(obj, Data { history, unsealed: vec![], image: None });
                let d = self.latency();
                self.at(d, Msg::Proc(p, Event::Restored { obj, act, ok }));
            }
            Effect::PutImage { obj, act } => {
                // Sometimes never written (a crash, a lost PUT).
                let img = self.procs[p].data.get_mut(&obj).and_then(|d| d.image.take()).filter(|_| self.rng.chance(0.7));
                if let Some((k, w)) = img {
                    if let Res::Put(Ok(_)) = self.apply(&Call::Put(k.clone(), Val::Seg(crate::own::frame(&[w])), Cond::IfAbsent)) {
                        let d = self.latency();
                        self.at(d, Msg::Proc(p, Event::Imaged { obj, act, key: k }));
                    }
                }
            }
            Effect::Drop { obj } => {
                self.procs[p].data.remove(&obj);
            }
        }
    }

    /// The bucket: linearizable, with conditional writes.
    fn apply(&mut self, c: &Call) -> Res {
        match c {
            Call::Get(k) => Res::Got(self.bucket.get(k).map(|(v, ver)| (v.clone(), ver.to_string()))),
            Call::Put(k, v, cond) => {
                let ok = match (cond, self.bucket.get(k)) {
                    (Cond::Any, _) => true,
                    (Cond::IfAbsent, cur) => cur.is_none(),
                    (Cond::IfMatch(etag), Some((_, cur))) => cur.to_string() == *etag,
                    (Cond::IfMatch(_), None) => false,
                };
                if !ok {
                    return Res::Put(Err(()));
                }
                self.version += 1;
                if let Val::Rec(_) = v {
                    self.stats.epochs += 1;
                }
                self.bucket.insert(k.clone(), (v.clone(), self.version));
                Res::Put(Ok(self.version.to_string()))
            }
            Call::List(prefix) => {
                Res::Listed(self.bucket.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(prefix.as_str())).map(|(k, _)| k.clone()).collect())
            }
            Call::Delete(k) => {
                self.bucket.remove(k);
                Res::Deleted
            }
        }
    }

    /// `forwarded`: sent on by a node (a redirect, a move), as a real node
    /// forwards it over the wire.
    fn client_send(&mut self, client: u32, to: Option<(Obj, Option<W>, Node)>, forwarded: bool) {
        if self.now >= self.s.duration && to.is_none() {
            return; // traffic is over
        }
        let (obj, write, node) = to.unwrap_or_else(|| {
            let obj = self.rng.below(self.s.objects as u64) as Obj;
            let write = if self.rng.chance(0.6) {
                self.next_w += 1;
                Some(self.next_w)
            } else {
                None
            };
            (obj, write, self.rng.below(self.s.nodes as u64) as Node)
        });
        self.next_req += 1;
        let req = self.next_req;
        self.pending.insert(req, Pending { client, obj, write, sent: self.now });
        self.ops.insert(req, (obj, write));
        self.at(CLIENT_TIMEOUT, Msg::ClientTimeout(req));
        let p = self.current[&node];
        let d = self.latency();
        let ev = if forwarded { Event::Forwarded { obj, req } } else { Event::Request { obj, req } };
        self.at(d, Msg::Proc(p, ev));
    }

    fn client_answer(&mut self, req: ReqId, ans: Answer, p: usize) -> Result<(), String> {
        let saw = self.procs[p].saw.remove(&req);
        let Some(pd) = self.pending.remove(&req) else { return Ok(()) }; // timed out already
        if pd.client >= PROBE && matches!(ans, Answer::Done { .. }) && !self.probes.insert(pd.obj, true).unwrap_or(false) {
            self.stats.probe_ms = self.stats.probe_ms.max(self.now - (self.s.duration + 2000));
        }
        match ans {
            Answer::Done { epoch } => {
                let history = saw.ok_or_else(|| format!("request {req} answered without being run"))?;
                let key = (pd.obj, epoch);
                // One writer per epoch: only one process answers in it.
                let by = self.answered_by.entry(key).or_default();
                if !by.contains(&p) {
                    by.push(p);
                }
                if by.len() > 1 {
                    return Err(format!("two processes {by:?} answered in object {} epoch {epoch}", pd.obj));
                }
                // Answers within an epoch tell one story.
                let best = self.proven.entry(key).or_default();
                if is_prefix(best, &history) {
                    *best = history.clone();
                } else if !is_prefix(&history, best) {
                    return Err(format!("object {} epoch {epoch}: answers disagree: {history:?} vs {best:?}", pd.obj));
                }
                match pd.write {
                    Some(w) => {
                        if !history.contains(&w) {
                            return Err(format!("write {w} acknowledged but not in its own history"));
                        }
                        self.acked.insert(w, (pd.obj, epoch, self.now));
                        if let Some(&t) = self.last_ack.get(&pd.obj) {
                            if self.now <= self.s.duration {
                                self.stats.max_gap = self.stats.max_gap.max(self.now - t);
                            }
                        }
                        self.last_ack.insert(pd.obj, self.now);
                        self.stats.writes_acked += 1;
                    }
                    None => {
                        // A read sees every write acknowledged before it was sent.
                        for (&w, &(o, _, t)) in &self.acked {
                            if o == pd.obj && t < pd.sent && !history.contains(&w) {
                                return Err(format!("read of object {o} sent at {} missed write {w}, acknowledged at {t}", pd.sent));
                            }
                        }
                        self.stats.reads_done += 1;
                    }
                }
                let d = 1 + self.rng.below(20);
                self.at(d, Msg::ClientNext(pd.client));
            }
            Answer::Redirect(n) => {
                self.stats.redirects += 1;
                self.client_send(pd.client, Some((pd.obj, pd.write, n)), true);
            }
            Answer::Moved(n) => {
                if saw.is_some() {
                    return Err(format!("request {req} answered \"moved\", but it ran"));
                }
                self.client_send(pd.client, Some((pd.obj, pd.write, n)), true);
            }
            Answer::NotRun => {
                if saw.is_some() {
                    return Err(format!("request {req} answered \"not run\", but it ran"));
                }
                self.stats.failed += 1;
                self.stats.not_run += 1;
                let d = 10 + self.rng.below(100);
                self.at(d, Msg::ClientNext(pd.client));
            }
            Answer::Failed => {
                self.stats.failed += 1;
                let d = 10 + self.rng.below(100);
                self.at(d, Msg::ClientNext(pd.client));
            }
        }
        Ok(())
    }

    /// The writes a manifest stands for.
    fn expand(&self, segs: &[String]) -> Vec<W> {
        segs.iter().flat_map(|k| match self.bucket.get(k) {
            Some((Val::Seg(b), _)) => decode_seg(b).unwrap_or_default(),
            _ => vec![],
        }).collect()
    }

    /// The bucket's invariants: every manifest holds every write acknowledged
    /// in an earlier epoch, and starts with what that epoch proved.
    ///
    /// With compaction: an epoch's image stands for its manifest (its keys
    /// may be gone), and a manifest older than some image is superseded (its
    /// owner deleted what that one replaced). The newest manifest is always
    /// checked in full, so a delete of something it needs is still caught.
    fn check_bucket(&self) -> Result<(), String> {
        // Each epoch's newest image (`i`, or `i<N>`: after its first N entries).
        let mut images: HashMap<Obj, BTreeMap<u32, (u64, String)>> = HashMap::new();
        for k in self.bucket.keys() {
            if let Some((o, e, n)) = parse_image(k) {
                let best = images.entry(o).or_default().entry(e).or_insert((n, k.clone()));
                if n >= best.0 {
                    *best = (n, k.clone());
                }
            }
        }
        for (k, (v, _)) in &self.bucket {
            let Val::Manifest(segs) = v else { continue };
            let Some((o, e)) = parse_manifest(k) else { continue };
            let imaged = images.get(&o);
            if imaged.is_some_and(|m| m.range(e + 1..).next().is_some()) {
                continue;
            }
            // An image holds what the epoch started from, then its own first
            // entries: what was acknowledged before it is in there, in order.
            let m = match imaged.and_then(|m| m.get(&e)) {
                Some((_, k)) => self.expand(std::slice::from_ref(k)),
                None => self.expand(segs),
            };
            for (&w, &(wo, we, _)) in &self.acked {
                if wo == o && we < e && !m.contains(&w) {
                    return Err(format!("lost write {w}: acknowledged in object {o} epoch {we}, missing from epoch {e}"));
                }
            }
            for (&(po, pe), h) in &self.proven {
                if po == o && pe < e && !is_prefix(h, &m) {
                    return Err(format!("history rewrite in object {o}: epoch {pe} proved {h:?}, epoch {e} starts from {m:?}"));
                }
            }
        }
        Ok(())
    }
}

fn parse_manifest(k: &str) -> Option<(Obj, u32)> {
    parse_part(k, "m")
}

/// An image key's object, epoch, and entries (`i`: 0, `i<N>`: N).
fn parse_image(k: &str) -> Option<(Obj, u32, u64)> {
    let rest = k.strip_prefix("obj/")?;
    let (o, _) = rest.split_once('/')?;
    let o = Obj::from_str_radix(o, 16).ok()?;
    match parse_key(o, k)? {
        (e, "i") => Some((o, e, 0)),
        (e, p) if p.starts_with('i') => Some((o, e, p[1..].parse().ok()?)),
        _ => None,
    }
}

/// An object key's object and epoch, if its part is `part` (`m`, `i`).
fn parse_part(k: &str, part: &str) -> Option<(Obj, u32)> {
    let rest = k.strip_prefix("obj/")?;
    let (o, _) = rest.split_once('/')?;
    let o = Obj::from_str_radix(o, 16).ok()?;
    match parse_key(o, k)? {
        (e, p) if p == part => Some((o, e)),
        _ => None,
    }
}

/// Run `seeds` seeds; the first failing seed and why.
pub fn run_many(seeds: std::ops::Range<u64>, replicas: usize, bugs: Bugs) -> (Stats, Option<(u64, String)>) {
    run_with(seeds, &|| Settings { replicas, bugs, ..Settings::default() })
}

pub fn run_with(seeds: std::ops::Range<u64>, settings: &dyn Fn() -> Settings) -> (Stats, Option<(u64, String)>) {
    let mut total = Stats::default();
    for seed in seeds {
        let mut sim = Sim::new(seed, settings());
        let r = sim.run();
        for p in &sim.procs {
            total.peer_acks += p.core.stats.peer_acks;
            total.bucket_acks += p.core.stats.bucket_acks;
            total.fences += p.core.stats.fences;
            total.tail_entries += p.core.stats.tail_entries;
            total.uploads += p.core.stats.uploads;
            total.lease_reads += p.core.stats.lease_reads;
            total.gc_deleted += p.core.stats.gc_deleted;
            total.moves += p.core.stats.moves;
        }
        let s = &sim.stats;
        total.writes_acked += s.writes_acked;
        total.reads_done += s.reads_done;
        total.failed += s.failed;
        total.not_run += s.not_run;
        total.redirects += s.redirects;
        total.epochs += s.epochs;
        total.crashes += s.crashes;
        total.freezes += s.freezes;
        total.splits += s.splits;
        total.wipes += s.wipes;
        total.max_gap = total.max_gap.max(s.max_gap);
        total.probe_ms = total.probe_ms.max(s.probe_ms);
        if let Err(e) = r {
            return (total, Some((seed, e)));
        }
    }
    (total, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_run() {
        for r in [0, 2] {
            let a = run_many(7..8, r, Bugs::default()).0;
            let b = run_many(7..8, r, Bugs::default()).0;
            assert_eq!(format!("{a:?}"), format!("{b:?}"));
        }
    }

    #[test]
    fn the_protocol_holds_under_faults() {
        let (s, bad) = run_many(0..200, 0, Bugs::default());
        assert!(bad.is_none(), "{bad:?}");
        assert!(s.writes_acked > 10_000 && s.epochs > 200 && s.crashes > 10 && s.freezes > 10, "{s:?}");
    }

    #[test]
    fn peer_writes_hold_under_faults() {
        let (s, bad) = run_many(0..200, 2, Bugs::default());
        assert!(bad.is_none(), "{bad:?}");
        assert!(s.writes_acked > 10_000 && s.epochs > 200 && s.crashes > 10 && s.freezes > 10, "{s:?}");
        // Mostly the fast way; takeovers really fence and recover tails.
        assert!(s.peer_acks > 2 * s.bucket_acks && s.bucket_acks > 0, "{s:?}");
        assert!(s.fences > 50 && s.tail_entries > 50, "{s:?}");
    }

    #[test]
    fn splits_keep_every_invariant() {
        for replicas in [0, 2] {
            let (s, bad) = run_with(0..300, &|| Settings { replicas, splits: true, ..Settings::default() });
            assert!(bad.is_none(), "replicas {replicas}: {bad:?}");
            assert!(s.splits > 1000 && s.writes_acked > 10_000, "{s:?}");
        }
    }

    #[test]
    fn a_lost_disk_loses_nothing() {
        let (s, bad) = run_with(0..500, &|| Settings { replicas: 2, wipes: true, ..Settings::default() });
        assert!(bad.is_none(), "{bad:?}");
        assert!(s.wipes > 150 && s.fences > 500, "{s:?}");
        for (name, bugs) in [("trust_wiped", Bugs { trust_wiped: true, ..Bugs::default() }), ("wipe_forgets_fence", Bugs { wipe_forgets_fence: true, ..Bugs::default() })] {
            // Heartbeats made false deaths (and so takeovers) rarer, and these
            // need a takeover while a peer's disk is gone: up to 20,000 seeds
            // (the first failure stops it).
            let (_, bad) = run_with(0..20_000, &|| Settings { replicas: 2, wipes: true, bugs, ..Settings::default() });
            assert!(bad.is_some(), "{name} was not caught in 20,000 seeds");
        }
    }

    /// Liveness, with heartbeats over the network: a node lost for good has
    /// its objects taken over, so every object takes a write once the
    /// faults stop. With the known-bad version that hears everyone forever,
    /// safety still holds (it never rested on liveness), and the probe
    /// catches it, as it does uploads that wait while a peer is down.
    #[test]
    fn a_lost_node_is_taken_over() {
        for replicas in [0, 2] {
            let (_, bad) = run_with(0..300, &|| Settings { replicas, loss: true, splits: true, ..Settings::default() });
            assert!(bad.is_none(), "replicas {replicas}: {bad:?}");
            let alive = Bugs { assume_alive: true, ..Bugs::default() };
            let (_, bad) = run_with(0..300, &|| Settings { replicas, loss: true, bugs: alive, ..Settings::default() });
            assert!(bad.is_some_and(|(_, why)| why.contains("never took a write")), "assume_alive wasn't caught");
            let (_, bad) = run_with(0..100, &|| Settings { replicas, bugs: alive, ..Settings::default() });
            assert!(bad.is_none(), "with nobody lost, it's only liveness: safety must hold: {bad:?}");
        }
        // Uploads that wait with a peer down: writes need the bucket then,
        // so they crawl (one per upload timer). Caught by the probe's clock.
        let wait = Bugs { wait_when_down: true, ..Bugs::default() };
        let (_, bad) = run_with(0..300, &|| Settings { replicas: 2, loss: true, bugs: wait, ..Settings::default() });
        assert!(bad.is_some_and(|(_, why)| why.contains("ms to get in")), "wait_when_down wasn't caught");
    }

    /// Read leases: reads answered with no round are still never stale,
    /// through splits and lost disks, and the lease is really used. The two
    /// ways to get it wrong (peers that break their promise, an owner that
    /// trusts past it) are caught as stale reads.
    #[test]
    fn read_leases_hold_and_their_bugs_are_caught() {
        let (s, bad) = run_with(0..300, &|| Settings { replicas: 2, splits: true, wipes: true, ..Settings::default() });
        assert!(bad.is_none(), "{bad:?}");
        assert!(s.lease_reads > s.reads_done / 4, "the lease should answer many reads: {s:?}");
        for (name, bugs) in [
            ("lease_no_promise", Bugs { lease_no_promise: true, ..Bugs::default() }),
            ("lease_overtrust", Bugs { lease_overtrust: true, ..Bugs::default() }),
            ("lease_trusts_kept", Bugs { lease_trusts_kept: true, ..Bugs::default() }),
        ] {
            let (_, bad) = run_with(0..2000, &|| Settings { replicas: 2, splits: true, bugs, ..Settings::default() });
            assert!(bad.as_ref().is_some_and(|(_, why)| why.contains("missed write")), "{name} wasn't caught: {bad:?}");
        }
    }

    /// Objects that beat every 50 ms, with peers far away (a round of 200 to
    /// 500 ms): reads must still be answered under the lease. Once one read
    /// waited for a round, every beat queued behind it and the lease was
    /// never used again (found on the arena's fleet: pings of 400 to 1,000 ms).
    ///
    ///
    /// And a takeover isn't held off by an old owner that keeps renewing: a
    /// peer with a newer owner's fence waiting stops promising the older
    /// epoch (`Kept`), so the wait ends. Before, it lasted as long as the old
    /// owner kept going (seed 113: 15 s for a write to get in).
    #[test]
    fn beats_keep_reads_under_the_lease() {
        let far = || Settings { replicas: 2, beats: true, peer_ms: Some((100, 250)), ..Settings::default() };
        let (s, bad) = run_with(0..300, &far);
        assert!(bad.is_none(), "{bad:?}");
        assert!(s.lease_reads > s.reads_done / 2, "the lease should answer most reads: {s:?}");
        let (_, bad) = run_with(0..300, &|| Settings { splits: true, wipes: true, ..far() });
        assert!(bad.is_none(), "{bad:?}");
        // Near peers, with splits.
        let (_, bad) = run_with(0..500, &|| Settings { replicas: 2, beats: true, splits: true, ..Settings::default() });
        assert!(bad.is_none(), "{bad:?}");
    }

    /// Placement hands objects to other nodes mid-traffic (`Event::Move`):
    /// every invariant holds, with every fault on, and moves really finish.
    /// A step-down that seals before the bucket has everything is caught
    /// at once with moves (about one seed in 5,000 without).
    #[test]
    fn moves_keep_every_invariant() {
        for replicas in [0, 2] {
            let (s, bad) = run_with(0..300, &|| Settings { replicas, moves: true, ..Settings::default() });
            assert!(bad.is_none(), "replicas {replicas}: {bad:?}");
            assert!(s.moves > 400 && s.writes_acked > 10_000, "{s:?}");
            let (s, bad) = run_with(0..300, &|| Settings { replicas, moves: true, splits: true, loss: true, ..Settings::default() });
            assert!(bad.is_none(), "replicas {replicas}, splits and a lost node: {bad:?}");
            assert!(s.moves > 300, "{s:?}");
        }
        let (_, bad) = run_with(0..300, &|| Settings { replicas: 2, moves: true, beats: true, peer_ms: Some((100, 250)), splits: true, wipes: true, ..Settings::default() });
        assert!(bad.is_none(), "far peers, beats, splits, wipes: {bad:?}");
        let (_, bad) = run_with(0..100, &|| Settings { replicas: 2, moves: true, uploads: Some((5000, 1 << 20)), bugs: Bugs { seal_early: true, ..Bugs::default() }, ..Settings::default() });
        assert!(bad.is_some(), "seal_early wasn't caught through a move");
    }

    /// Each known-bad version must be caught, or the oracle is blind.
    #[test]
    fn known_bugs_are_caught() {
        let cases = [
            ("skip_proof", 0, Bugs { skip_proof: true, ..Bugs::default() }),
            ("load_before_cas", 0, Bugs { load_before_cas: true, ..Bugs::default() }),
            ("no_manifest", 0, Bugs { no_manifest: true, ..Bugs::default() }),
            ("reuse_epoch", 0, Bugs { reuse_epoch: true, ..Bugs::default() }),
            ("ack_one", 2, Bugs { ack_one: true, ..Bugs::default() }),
            ("no_fence", 2, Bugs { no_fence: true, ..Bugs::default() }),
            ("trunc_early", 2, Bugs { trunc_early: true, ..Bugs::default() }),
        ];
        // Stepping down with entries only on the peers: rare (about one seed
        // in 5,000), so many seeds, with slow uploads to leave entries there.
        let (_, bad) = run_with(0..20000, &|| Settings { replicas: 2, uploads: Some((5000, 1 << 20)), bugs: Bugs { seal_early: true, ..Bugs::default() }, ..Settings::default() });
        assert!(bad.is_some(), "seal_early wasn't caught");
        // Compaction that deletes before proving it still owns the object
        // (a race with a newer owner's load: splits make it common).
        let (_, bad) = run_with(0..3000, &|| Settings { replicas: 2, splits: true, bugs: Bugs { gc_unproven: true, ..Bugs::default() }, ..Settings::default() });
        assert!(bad.as_ref().is_some_and(|(_, why)| why.contains("lost write")), "gc_unproven wasn't caught: {bad:?}");
        // Deletes that lean on a newer, unproven image than their round proved.
        let (_, bad) = run_with(0..8000, &|| Settings { replicas: 2, splits: true, bugs: Bugs { gc_newest_image: true, ..Bugs::default() }, ..Settings::default() });
        assert!(bad.is_some(), "gc_newest_image wasn't caught");
        for (name, replicas, bugs) in cases {
            let (_, bad) = run_many(0..6000, replicas, bugs);
            assert!(bad.is_some(), "{name} was not caught in 6,000 seeds");
        }
    }
}
