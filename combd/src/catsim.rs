//! A deterministic simulator for the catalog (`catalog.rs`). Several nodes
//! run the real decision core against a fake bucket, with clients writing
//! changes (an app claims a domain) at random nodes, and a world that
//! misbehaves:
//! - bucket calls take 1 to 50 ms, sometimes 500 ms; requests and replies
//!   get lost (the caller can't tell);
//! - a deleted key is just gone: a trimmed slot is empty again, as in S3;
//! - pushes between nodes are lost, repeated, or reordered;
//! - nodes crash, and come back with their copy (on disk) or without it (in
//!   memory);
//! - nodes snapshot their copy at random, which trims the log.
//!
//! An oracle checks every step against the spec's invariants: no lost
//! acknowledged change, every node's copy is the start of the log, no two
//! apps on one domain. And once the faults stop: every node catches up to
//! the whole log (the spec doesn't check that; this does).

use crate::catalog::*;
use crate::sim::Rng;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub writes_acked: u64,
    pub writes_refused: u64,
    pub dead_slot_writes: u64,
    pub snapshots: u64,
    pub crashes: u64,
    pub copies_lost: u64,
}

enum Msg {
    Reply { node: usize, life: u64, op: OpId, res: Res },
    Push { node: usize, seq: Seq },
    Submit { node: usize },
    Tick { node: usize },
    Snapshot { node: usize },
    Crash { node: usize },
}

struct Node {
    core: Catalog,
    /// Bumped at each crash: replies to a dead process go nowhere.
    life: u64,
}

pub struct Sim {
    rng: Rng,
    now: u64,
    queue: BTreeMap<(u64, u64), Msg>,
    seq: u64,
    nodes: Vec<Node>,
    bugs: Bugs,
    /// The fake bucket: key -> (value, etag).
    bucket: BTreeMap<String, (Vec<u8>, String)>,
    etags: u64,
    /// The oracle: the log as readers will see it (the snapshot's state,
    /// then the slots after it), and every acknowledged change.
    truth: Vec<Entry>,
    acked: BTreeMap<u64, Entry>,
    /// Writes submitted and not yet answered.
    pending: BTreeMap<u64, Entry>,
    next_id: u64,
    duration: u64,
    pub stats: Stats,
}

/// The rule the simulator's changes follow: an app claims a domain (its
/// value), and no two apps may hold one.
fn check() -> Check {
    Box::new(|s: &State, e: &Entry| {
        if e.value.is_empty() {
            return Ok(());
        }
        match s.apps.iter().find(|(a, v)| **v == e.value && **a != e.app) {
            Some((a, _)) => Err(format!("{} is {a}'s", String::from_utf8_lossy(&e.value))),
            None => Ok(()),
        }
    })
}

fn state_of(log: &[Entry]) -> State {
    let mut s = State::default();
    for e in log {
        s.apply(e);
    }
    s
}

impl Sim {
    pub fn new(seed: u64, bugs: Bugs) -> Sim {
        let nodes = (0..3).map(|_| Node { core: Catalog::new(State::default(), check(), bugs), life: 0 }).collect();
        let mut s = Sim { rng: Rng::new(seed), now: 0, queue: BTreeMap::new(), seq: 0, nodes, bugs, bucket: BTreeMap::new(), etags: 0, truth: vec![], acked: BTreeMap::new(), pending: BTreeMap::new(), next_id: 0, duration: 20_000, stats: Stats::default() };
        for n in 0..3 {
            let d = 1 + s.rng.below(50);
            s.at(d, Msg::Submit { node: n });
            s.at(1000, Msg::Tick { node: n });
        }
        s
    }

    fn at(&mut self, d: u64, m: Msg) {
        self.seq += 1;
        self.queue.insert((self.now + d, self.seq), m);
    }

    fn latency(&mut self) -> u64 {
        if self.rng.chance(0.02) { 500 } else { 1 + self.rng.below(50) }
    }

    fn faults(&self) -> bool {
        self.now <= self.duration
    }

    /// The bucket answers a call (or loses it, either way round).
    fn bucket_call(&mut self, call: Call) -> Res {
        let lost_req = self.faults() && self.rng.chance(0.03);
        if lost_req {
            return Res::Lost;
        }
        let res = match call {
            Call::Get(k) => Res::Got(self.bucket.get(&k).cloned()),
            Call::Put(k, v) => {
                self.slot_written(&k, &v, true);
                Res::Put(Ok(self.put(k, v)))
            }
            Call::PutIfAbsent(k, v) => {
                if self.bucket.contains_key(&k) {
                    Res::Put(Err(()))
                } else {
                    self.slot_written(&k, &v, false);
                    Res::Put(Ok(self.put(k, v)))
                }
            }
            Call::PutIfMatch(k, v, etag) => {
                let now = self.bucket.get(&k).map(|(_, e)| e.clone());
                if now == etag {
                    Res::Put(Ok(self.put(k, v)))
                } else {
                    Res::Put(Err(()))
                }
            }
            Call::Delete(k) => {
                self.bucket.remove(&k);
                Res::Done
            }
        };
        // The reply may be lost too.
        if self.faults() && self.rng.chance(0.03) { Res::Lost } else { res }
    }

    fn put(&mut self, k: String, v: Vec<u8>) -> String {
        self.etags += 1;
        let e = format!("e{}", self.etags);
        self.bucket.insert(k, (v, e.clone()));
        e
    }

    /// The oracle sees a slot written: it's the next one (it joins the log),
    /// an old one trimmed away (nobody will read it), or, with the blind
    /// bug, one overwritten.
    fn slot_written(&mut self, k: &str, v: &[u8], blind: bool) {
        let Some(n) = k.strip_prefix("catalog/log/").and_then(|n| n.parse::<u64>().ok()) else { return };
        let Some(e) = decode(v) else { return };
        let n = n as usize;
        if n == self.truth.len() + 1 {
            self.truth.push(e);
        } else if n <= self.truth.len() {
            if blind && self.bucket.contains_key(k) {
                self.truth[n - 1] = e;
            } else {
                self.stats.dead_slot_writes += 1;
            }
        }
    }

    fn effects(&mut self, node: usize, fx: Vec<Effect>) {
        let life = self.nodes[node].life;
        for f in fx {
            match f {
                Effect::Call { op, call } => {
                    let res = self.bucket_call(call);
                    let d = self.latency();
                    self.at(d, Msg::Reply { node, life, op, res });
                }
                Effect::Push { seq, .. } => {
                    for to in (0..self.nodes.len()).filter(|&t| t != node) {
                        if self.faults() && self.rng.chance(0.2) {
                            continue; // lost
                        }
                        let copies = if self.rng.chance(0.1) { 2 } else { 1 };
                        for _ in 0..copies {
                            let d = 1 + self.rng.below(100);
                            self.at(d, Msg::Push { node: to, seq });
                        }
                    }
                }
                Effect::Ack { write, res } => match res {
                    Ok(_) => {
                        self.stats.writes_acked += 1;
                        let e = self.pending.remove(&write).unwrap();
                        self.acked.insert(e.id, e);
                    }
                    Err(_) => {
                        self.stats.writes_refused += 1;
                        self.pending.remove(&write);
                    }
                },
            }
        }
    }

    fn submit(&mut self, node: usize) {
        self.next_id += 1;
        let id = self.next_id;
        let app = format!("a{}", self.rng.below(3));
        let value = if self.rng.chance(0.1) { vec![] } else { format!("d{}", self.rng.below(3)).into_bytes() };
        let e = Entry { id, app, value };
        self.pending.insert(id, e.clone());
        let fx = self.nodes[node].core.on(Event::Submit { write: id, entry: e });
        self.effects(node, fx);
    }

    /// The invariants, after every step.
    fn check(&self) -> Result<(), String> {
        for (id, e) in &self.acked {
            if !self.truth.iter().any(|t| t.id == *id) {
                return Err(format!("acknowledged change {id} ({e:?}) isn't in the log"));
            }
        }
        let doms = |s: &State| -> Result<(), String> {
            let mut seen = BTreeSet::new();
            for v in s.apps.values() {
                if !seen.insert(v.clone()) {
                    return Err(format!("two apps hold {}", String::from_utf8_lossy(v)));
                }
            }
            Ok(())
        };
        doms(&state_of(&self.truth)).map_err(|e| format!("the log: {e}"))?;
        for (i, n) in self.nodes.iter().enumerate() {
            let c = &n.core.copy;
            if c.at as usize > self.truth.len() || *c != state_of(&self.truth[..c.at as usize]) {
                return Err(format!("node {i}'s copy (at {}) isn't the start of the log ({} long)", c.at, self.truth.len()));
            }
            doms(c).map_err(|e| format!("node {i}: {e}"))?;
        }
        Ok(())
    }

    pub fn run(&mut self) -> Result<Stats, String> {
        while let Some(((t, _), m)) = self.queue.pop_first() {
            self.now = t;
            if t > self.duration + 30_000 {
                break;
            }
            match m {
                Msg::Reply { node, life, op, res } => {
                    if life == self.nodes[node].life {
                        let fx = self.nodes[node].core.on(Event::Reply { op, res });
                        self.effects(node, fx);
                    }
                }
                Msg::Push { node, seq } => {
                    let fx = self.nodes[node].core.on(Event::Push { seq });
                    self.effects(node, fx);
                }
                Msg::Submit { node } => {
                    if self.faults() {
                        if !self.nodes[node].core.busy() {
                            self.submit(node);
                        }
                        let d = 20 + self.rng.below(200);
                        self.at(d, Msg::Submit { node });
                    }
                }
                Msg::Tick { node } => {
                    let fx = self.nodes[node].core.on(Event::Tick);
                    self.effects(node, fx);
                    if self.faults() && self.rng.chance(0.5) {
                        let d = 1 + self.rng.below(200);
                        self.at(d, Msg::Snapshot { node });
                    }
                    if self.faults() && self.rng.chance(0.05) {
                        let d = 1 + self.rng.below(200);
                        self.at(d, Msg::Crash { node });
                    }
                    self.at(1000, Msg::Tick { node });
                }
                Msg::Snapshot { node } => {
                    self.stats.snapshots += 1;
                    let fx = self.nodes[node].core.on(Event::Snapshot);
                    self.effects(node, fx);
                }
                Msg::Crash { node } => {
                    self.stats.crashes += 1;
                    // Its writes in flight are gone: never acknowledged.
                    let keep = self.rng.chance(0.5);
                    let copy = if keep { self.nodes[node].core.copy.clone() } else { State::default() };
                    if !keep {
                        self.stats.copies_lost += 1;
                    }
                    self.nodes[node] = Node { core: Catalog::new(copy, check(), self.bugs), life: self.nodes[node].life + 1 };
                }
            }
            self.check()?;
        }
        // After the faults: every node catches up to the whole log.
        for (i, n) in self.nodes.iter().enumerate() {
            if n.core.copy.at as usize != self.truth.len() {
                return Err(format!("node {i} never caught up: at {} of {}", n.core.copy.at, self.truth.len()));
            }
        }
        Ok(self.stats)
    }
}

pub fn run_many(seeds: std::ops::Range<u64>, bugs: Bugs) -> (Stats, Option<(u64, String)>) {
    let mut total = Stats::default();
    for seed in seeds {
        let mut s = Sim::new(seed, bugs);
        match s.run() {
            Ok(st) => {
                total.writes_acked += st.writes_acked;
                total.writes_refused += st.writes_refused;
                total.dead_slot_writes += st.dead_slot_writes;
                total.snapshots += st.snapshots;
                total.crashes += st.crashes;
                total.copies_lost += st.copies_lost;
            }
            Err(e) => return (total, Some((seed, e))),
        }
    }
    (total, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_holds_in_200_worlds() {
        let (st, bad) = run_many(0..200, Bugs::default());
        assert!(bad.is_none(), "{bad:?}");
        assert!(st.writes_acked > 1000 && st.snapshots > 100 && st.crashes > 50, "{st:?}");
    }

    /// Each known-bad version must be caught, or the simulator is blind.
    #[test]
    fn every_known_bad_version_is_caught() {
        let all = [
            ("ack_before_write", Bugs { ack_before_write: true, ..Bugs::default() }),
            ("blind_slot", Bugs { blind_slot: true, ..Bugs::default() }),
            ("uncovered_check", Bugs { uncovered_check: true, ..Bugs::default() }),
            ("trim_no_gate", Bugs { trim_no_gate: true, ..Bugs::default() }),
            ("trim_early", Bugs { trim_early: true, ..Bugs::default() }),
            ("read_no_gate", Bugs { read_no_gate: true, ..Bugs::default() }),
        ];
        for (name, bugs) in all {
            let (_, bad) = run_many(0..2000, bugs);
            assert!(bad.is_some(), "{name} passed: the simulator can't see it");
        }
    }
}
