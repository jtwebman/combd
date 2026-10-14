//! A deterministic simulator for D2 ownership. It runs the real decision
//! core (`own.rs`) in several processes against a fake bucket, with clients
//! sending writes and reads, and a world that misbehaves:
//! - bucket calls take 1 to 50 ms, sometimes 500 ms,
//! - requests and replies get lost (the caller times out and can't tell),
//! - clocks run fast or slow, by up to 10%,
//! - processes crash, and their supervisor starts a new one,
//! - processes freeze; the supervisor may start a new one, and the frozen
//!   one later wakes up still thinking it owns things.
//!
//! One seed gives one exact run, so any failure replays.
//!
//! An oracle checks every run: one writer per epoch, no lost acknowledged
//! write, no history rewrite (the invariants in `spec/Ownership.tla`), and
//! that a read sees every write acknowledged before the read was sent.

use crate::own::*;
use std::collections::{BTreeMap, HashMap};

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
    pub bugs: Bugs,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { nodes: 3, objects: 2, clients: 6, duration: 20_000, lost: 0.03, crash: 0.005, freeze: 0.005, bugs: Bugs::default() }
    }
}

#[derive(Default, Debug)]
pub struct Stats {
    pub writes_acked: u64,
    pub reads_done: u64,
    pub failed: u64,
    pub redirects: u64,
    pub epochs: u64,
    pub crashes: u64,
    pub freezes: u64,
}

/// A call's give-up time: after this, the caller hears `Lost`.
const TIMEOUT: u64 = 400;
const CLIENT_TIMEOUT: u64 = 5_000;

enum Msg {
    /// To a process: an event from the outside world.
    Proc(usize, Event),
    /// A bucket call arriving at the bucket.
    Bucket(usize, OpId, Call, bool),
    /// An answer reaching a client.
    Client(ReqId, Answer, usize),
    ClientNext(u32),
    ClientTimeout(ReqId),
    Tick(usize),
    Restart(Node),
    Thaw(usize),
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
}

struct Pending {
    client: u32,
    obj: Obj,
    op: Op,
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
    procs: Vec<Proc>,
    /// The process a node's traffic goes to (its supervisor's latest).
    current: HashMap<Node, usize>,
    next_req: ReqId,
    next_w: W,
    pending: HashMap<ReqId, Pending>,
    pub stats: Stats,
    // The oracle's notes.
    /// Write -> (object, epoch, when the client heard).
    acked: HashMap<W, (Obj, u32, u64)>,
    /// (object, epoch) -> processes that answered `Done` in it.
    answered_by: HashMap<(Obj, u32), Vec<usize>>,
    /// (object, epoch) -> the longest history any answer showed.
    proven: HashMap<(Obj, u32), Vec<W>>,
}

fn is_prefix(a: &[W], b: &[W]) -> bool {
    a.len() <= b.len() && b[..a.len()] == *a
}

impl Sim {
    pub fn new(seed: u64, s: Settings) -> Sim {
        let mut sim = Sim {
            rng: Rng::new(seed),
            s,
            now: 0,
            seq: 0,
            queue: BTreeMap::new(),
            bucket: BTreeMap::new(),
            version: 0,
            procs: vec![],
            current: HashMap::new(),
            next_req: 0,
            next_w: 0,
            pending: HashMap::new(),
            stats: Stats::default(),
            acked: HashMap::new(),
            answered_by: HashMap::new(),
            proven: HashMap::new(),
        };
        for n in 0..sim.s.nodes {
            sim.spawn(n);
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
        self.procs.push(Proc { node, core: Core::new(node, self.s.bugs), alive: true, frozen: false, held: vec![], rate, off });
        let p = self.procs.len() - 1;
        self.current.insert(node, p);
        self.at(0, Msg::Tick(p));
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
        self.check_bucket()
    }

    fn step(&mut self, m: Msg) -> Result<(), String> {
        match m {
            Msg::Proc(p, ev) => self.deliver(p, ev)?,
            Msg::Tick(p) => {
                if !self.procs[p].alive {
                    return Ok(());
                }
                if self.now < self.s.duration + 30_000 {
                    let next = (RENEW_MS as f64 / self.procs[p].rate) as u64;
                    self.at(next, Msg::Tick(p));
                }
                self.deliver(p, Event::Tick)?;
                self.maybe_fault(p);
            }
            Msg::Bucket(p, op, call, lose_reply) => {
                let res = self.apply(&call);
                if !lose_reply {
                    let d = self.latency();
                    self.at(d, Msg::Proc(p, Event::Reply { op, res }));
                }
            }
            Msg::Client(req, ans, p) => self.client_answer(req, ans, p)?,
            Msg::ClientNext(c) => self.client_send(c, None),
            Msg::ClientTimeout(req) => {
                if let Some(pd) = self.pending.remove(&req) {
                    self.stats.failed += 1;
                    self.at(1, Msg::ClientNext(pd.client));
                }
            }
            Msg::Restart(n) => self.spawn(n),
            Msg::Thaw(p) => {
                self.procs[p].frozen = false;
                for ev in std::mem::take(&mut self.procs[p].held) {
                    self.deliver(p, ev)?;
                }
            }
        }
        Ok(())
    }

    fn maybe_fault(&mut self, p: usize) {
        if !self.faulty() || self.procs[p].frozen {
            return;
        }
        let node = self.procs[p].node;
        if self.rng.chance(self.s.crash) {
            self.stats.crashes += 1;
            self.procs[p].alive = false;
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

    fn deliver(&mut self, p: usize, ev: Event) -> Result<(), String> {
        if !self.procs[p].alive {
            return Ok(());
        }
        if self.procs[p].frozen {
            self.procs[p].held.push(ev);
            return Ok(());
        }
        let now = self.clock(p);
        let effects = self.procs[p].core.on(now, ev);
        for e in effects {
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
                Effect::Answer { req, ans } => {
                    let d = self.latency();
                    self.at(d, Msg::Client(req, ans, p));
                }
            }
        }
        Ok(())
    }

    /// The bucket: linearizable, with conditional writes.
    fn apply(&mut self, c: &Call) -> Res {
        match c {
            Call::Get(k) => Res::Got(self.bucket.get(k).cloned()),
            Call::Put(k, v, cond) => {
                let ok = match (cond, self.bucket.get(k)) {
                    (Cond::Any, _) => true,
                    (Cond::IfAbsent, cur) => cur.is_none(),
                    (Cond::IfMatch(ver), Some((_, cur))) => cur == ver,
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
                Res::Put(Ok(self.version))
            }
            Call::Scan(prefix) => {
                Res::Scanned(self.bucket.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(prefix.as_str())).map(|(k, (v, _))| (k.clone(), v.clone())).collect())
            }
        }
    }

    fn client_send(&mut self, client: u32, to: Option<(Obj, Op, Node)>) {
        if self.now >= self.s.duration && to.is_none() {
            return; // traffic is over
        }
        let (obj, op, node) = to.unwrap_or_else(|| {
            let obj = self.rng.below(self.s.objects as u64) as Obj;
            let op = if self.rng.chance(0.6) {
                self.next_w += 1;
                Op::Write(self.next_w)
            } else {
                Op::Read
            };
            (obj, op, self.rng.below(self.s.nodes as u64) as Node)
        });
        self.next_req += 1;
        let req = self.next_req;
        self.pending.insert(req, Pending { client, obj, op, sent: self.now });
        self.at(CLIENT_TIMEOUT, Msg::ClientTimeout(req));
        let p = self.current[&node];
        let d = self.latency();
        self.at(d, Msg::Proc(p, Event::Request { obj, req, op }));
    }

    fn client_answer(&mut self, req: ReqId, ans: Answer, p: usize) -> Result<(), String> {
        let Some(pd) = self.pending.remove(&req) else { return Ok(()) }; // timed out already
        match ans {
            Answer::Done { epoch, history } => {
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
                match pd.op {
                    Op::Write(w) => {
                        if !history.contains(&w) {
                            return Err(format!("write {w} acknowledged but not in its own history"));
                        }
                        self.acked.insert(w, (pd.obj, epoch, self.now));
                        self.stats.writes_acked += 1;
                    }
                    Op::Read => {
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
                self.client_send(pd.client, Some((pd.obj, pd.op, n)));
            }
            Answer::Failed => {
                self.stats.failed += 1;
                let d = 10 + self.rng.below(100);
                self.at(d, Msg::ClientNext(pd.client));
            }
        }
        Ok(())
    }

    /// The bucket's invariants: every manifest holds every write acknowledged
    /// in an earlier epoch, and starts with what that epoch proved.
    fn check_bucket(&self) -> Result<(), String> {
        for (k, (v, _)) in &self.bucket {
            let Val::Manifest(m) = v else { continue };
            let Some((o, e)) = parse_manifest(k) else { continue };
            for (&w, &(wo, we, _)) in &self.acked {
                if wo == o && we < e && !m.contains(&w) {
                    return Err(format!("lost write {w}: acknowledged in object {o} epoch {we}, missing from epoch {e}"));
                }
            }
            for (&(po, pe), h) in &self.proven {
                if po == o && pe < e && !is_prefix(h, m) {
                    return Err(format!("history rewrite in object {o}: epoch {pe} proved {h:?}, epoch {e} starts from {m:?}"));
                }
            }
        }
        Ok(())
    }
}

fn parse_manifest(k: &str) -> Option<(Obj, u32)> {
    let rest = k.strip_prefix("obj/")?;
    let (o, rest) = rest.split_once("/e")?;
    let e = rest.strip_suffix("/m")?;
    Some((o.parse().ok()?, e.parse().ok()?))
}

/// Run `seeds` seeds; the first failing seed and why.
pub fn run_many(seeds: std::ops::Range<u64>, bugs: Bugs) -> (Stats, Option<(u64, String)>) {
    let mut total = Stats::default();
    for seed in seeds {
        let mut sim = Sim::new(seed, Settings { bugs, ..Settings::default() });
        let r = sim.run();
        let s = &sim.stats;
        total.writes_acked += s.writes_acked;
        total.reads_done += s.reads_done;
        total.failed += s.failed;
        total.redirects += s.redirects;
        total.epochs += s.epochs;
        total.crashes += s.crashes;
        total.freezes += s.freezes;
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
        let a = run_many(7..8, Bugs::default()).0;
        let b = run_many(7..8, Bugs::default()).0;
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn the_protocol_holds_under_faults() {
        let (s, bad) = run_many(0..200, Bugs::default());
        assert!(bad.is_none(), "{bad:?}");
        assert!(s.writes_acked > 10_000 && s.epochs > 200 && s.crashes > 10 && s.freezes > 10, "{s:?}");
    }

    /// Each known-bad version must be caught, or the oracle is blind.
    #[test]
    fn known_bugs_are_caught() {
        let cases = [
            ("skip_proof", Bugs { skip_proof: true, ..Bugs::default() }),
            ("load_before_cas", Bugs { load_before_cas: true, ..Bugs::default() }),
            ("no_manifest", Bugs { no_manifest: true, ..Bugs::default() }),
            ("reuse_epoch", Bugs { reuse_epoch: true, ..Bugs::default() }),
        ];
        for (name, bugs) in cases {
            let (_, bad) = run_many(0..2000, bugs);
            assert!(bad.is_some(), "{name} was not caught in 2,000 seeds");
        }
    }
}
