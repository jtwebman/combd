//! An object's keys in a fleet's alarm index: which to put, which to
//! delete, and when a seal may go up. No I/O: the loop makes each call
//! (`Op`) and says how it went. `spec/Alarms.tla` is the model.
//!
//! The index (`alarm/<obj>/<due>-<id>`, holding the object's name) lets a
//! node find a due alarm of an object nobody is running. The rule it keeps:
//! every state the object could be restored to has its alarm there. A state
//! sealed but not yet proven can come back after a crash, so:
//! - A seal goes up only once the committed alarm has a key we put and saw
//!   land. A key we only listed may be an older owner's, which it may be
//!   deleting.
//! - Each put is a new key (a random id): a call that failed may still land
//!   later, so a key deleted once is never put again.
//! - A key goes only once a state is proven (an answer went out), and only
//!   if no state from that one on has its due time, or we have our own key
//!   for that time.
//!
//! A stale key costs a fire that finds nothing due. Its answer proves a
//! state, and the key goes then.

use crate::own::Obj;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Put(String),
    Delete(String),
}

/// Known-bad versions (the rules before), for proving the test can catch
/// them. Never set outside tests.
#[derive(Clone, Copy, Default, Debug)]
pub struct Bugs {
    /// Delete a key as soon as the committed alarm moves off it.
    pub delete_on_commit: bool,
    /// A seal counts a key it listed, not only one it put.
    pub trust_listed: bool,
    /// A key is its due time alone.
    pub reuse_keys: bool,
    /// Before anything is proven, delete all but the latest key.
    pub keep_newest: bool,
}

pub struct AlarmKeys {
    obj: Obj,
    /// Keys that may be in the bucket (listed at the restore, or put here):
    /// due time, and whether we put it and saw it land.
    keys: BTreeMap<String, (i64, bool)>,
    /// The call out, if any: one at a time.
    out: Option<Op>,
    /// The alarm of the last proven state, once one is.
    proven: Option<Option<i64>>,
    /// The states since, as (commit, alarm). The restored one is commit 0.
    pend: VecDeque<(u64, Option<i64>)>,
    /// The committed alarm, and the last commit.
    alarm: Option<i64>,
    seq: u64,
    bugs: Bugs,
}

pub fn prefix(obj: Obj) -> String {
    format!("alarm/{obj:032x}/")
}

pub fn key(obj: Obj, at: i64, id: u64) -> String {
    format!("alarm/{obj:032x}/{at:013}-{id:016x}")
}

/// A key's due time. (Keys from before had no id.)
pub fn due(key: &str) -> Option<i64> {
    key.rsplit('/').next()?.split('-').next()?.parse().ok()
}

impl AlarmKeys {
    /// Restored with this alarm; `listed`: the object's keys in the bucket.
    pub fn restored(obj: Obj, alarm: Option<i64>, listed: Vec<String>, bugs: Bugs) -> AlarmKeys {
        let keys = listed.into_iter().filter_map(|k| Some((due(&k)?, k))).map(|(at, k)| (k, (at, false))).collect();
        AlarmKeys { obj, keys, out: None, proven: None, pend: VecDeque::from([(0, alarm)]), alarm, seq: 0, bugs }
    }

    /// A commit, with the alarm it leaves; its number.
    pub fn committed(&mut self, alarm: Option<i64>) -> u64 {
        self.seq += 1;
        self.alarm = alarm;
        self.pend.push_back((self.seq, alarm));
        self.seq
    }

    /// The last commit's number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// An answer went out for a request that ran after commit `seq`: that
    /// state is proven, and no state before it can come back.
    pub fn proven(&mut self, seq: u64) {
        while self.pend.front().is_some_and(|&(s, _)| s <= seq) {
            self.proven = self.pend.pop_front().map(|(_, a)| a);
        }
    }

    /// The index has the committed alarm: what's committed may be sealed.
    pub fn sealable(&self) -> bool {
        self.alarm.is_none_or(|at| self.keys.values().any(|&(t, mine)| t == at && (mine || self.bugs.trust_listed)))
    }

    pub fn busy(&self) -> bool {
        self.out.is_some()
    }

    /// The next call to make, if none is out. `id`: a new random number.
    pub fn next(&mut self, id: impl FnOnce() -> u64) -> Option<Op> {
        if self.out.is_some() {
            return None;
        }
        let op = match self.alarm.filter(|&at| !self.keys.values().any(|&(t, mine)| t == at && mine)) {
            Some(at) => {
                let k = key(self.obj, at, if self.bugs.reuse_keys { 0 } else { id() });
                self.keys.insert(k.clone(), (at, false));
                Op::Put(k)
            }
            None => {
                let k = self.keys.iter().find(|(k, &(at, _))| self.deletable(k, at))?.0.clone();
                // Not ours to count on from here, whatever the answer.
                self.keys.get_mut(&k).unwrap().1 = false;
                Op::Delete(k)
            }
        };
        self.out = Some(op.clone());
        Some(op)
    }

    fn deletable(&self, key: &str, at: i64) -> bool {
        if self.bugs.delete_on_commit {
            return self.alarm != Some(at);
        }
        if self.bugs.keep_newest && self.proven.is_none() {
            return self.keys.values().any(|&(t, _)| t > at);
        }
        let Some(p) = self.proven else { return false };
        let live = p == Some(at) || self.pend.iter().any(|&(_, a)| a == Some(at));
        !live || self.keys.iter().any(|(k, &(t, mine))| t == at && mine && k != key)
    }

    /// The call came back. A failed one may still land later: a failed put
    /// stays here (maybe there) for a delete later, and a failed delete
    /// is tried again.
    pub fn done(&mut self, op: &Op, ok: bool) {
        if self.out.as_ref() != Some(op) {
            return;
        }
        self.out = None;
        match op {
            Op::Put(k) if ok => {
                if let Some(e) = self.keys.get_mut(k) {
                    e.1 = true;
                }
            }
            Op::Delete(k) if ok => {
                self.keys.remove(k);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::Rng;
    use std::collections::BTreeSet;

    const OBJ: Obj = 0xa1;

    struct Owner {
        k: AlarmKeys,
        on: bool,
        /// Where its restored state is in the history.
        base: usize,
        /// Its seals: (place in the history, the last commit in it).
        seals: Vec<(usize, u64)>,
        /// Commits since its last seal.
        unsealed: bool,
    }

    struct Call {
        owner: usize,
        op: Op,
        landed: bool,
        answered: bool,
    }

    /// One run of `Alarms.tla`'s world, at random: owners one after another
    /// (each restoring any state that could come back), old ones still
    /// acting until they crash, and bucket calls that land and answer in any
    /// order, or fail and land later, or never. After every step, every
    /// state that could be restored has its alarm in the bucket. Returns
    /// the step that broke it.
    fn run(seed: u64, bugs: Bugs) -> Result<(), String> {
        let mut r = Rng::new(seed);
        let mut bucket: BTreeSet<String> = BTreeSet::new();
        let mut calls: Vec<Call> = vec![];
        let mut hist: Vec<Option<i64>> = vec![None];
        let mut acked = 0;
        let mut owners: Vec<Owner> = vec![];
        let mut log = vec![];
        for step in 0..120 {
            let cur = owners.len().checked_sub(1);
            match r.below(16) {
                // A new owner: restores a state that could come back, and lists.
                0 if owners.len() < 4 && (cur.is_none() || r.chance(0.3)) => {
                    let i = acked + r.below((hist.len() - acked) as u64) as usize;
                    hist.truncate(i + 1);
                    let listed = bucket.iter().cloned().collect();
                    owners.push(Owner { k: AlarmKeys::restored(OBJ, hist[i], listed, bugs), on: true, base: i, seals: vec![], unsealed: false });
                    log.push(format!("{step}: owner {} restores {i} ({:?})", owners.len() - 1, hist[i]));
                }
                1 if r.chance(0.5) => {
                    if let Some(o) = owners.iter_mut().filter(|o| o.on).nth(r.below(3) as usize) {
                        o.on = false;
                        log.push(format!("{step}: a crash"));
                    }
                }
                // A request commits; it may set or clear the alarm.
                2 | 3 => {
                    let n = owners.len();
                    if n > 0 {
                        let e = if r.chance(0.8) { n - 1 } else { r.below(n as u64) as usize };
                        let a = [None, Some(1), Some(2)][r.below(3) as usize];
                        if owners[e].on {
                            owners[e].k.committed(a);
                            owners[e].unsealed = true;
                            log.push(format!("{step}: owner {e} commits {a:?}"));
                        }
                    }
                }
                // The current owner seals: only its seals can come back.
                4 | 5 => {
                    if let Some(o) = cur.map(|c| &mut owners[c]).filter(|o| o.on && o.unsealed && o.k.sealable()) {
                        hist.push(o.k.alarm);
                        o.seals.push((hist.len() - 1, o.k.seq()));
                        o.unsealed = false;
                        log.push(format!("{step}: seal {} ({:?})", hist.len() - 1, o.k.alarm));
                    }
                }
                // The current owner proves a state of its own, or its restored one.
                6 => {
                    if let Some(o) = cur.map(|c| &mut owners[c]).filter(|o| o.on) {
                        let mut can: Vec<(usize, u64)> = o.seals.iter().copied().filter(|&(p, _)| p >= acked).collect();
                        if o.base >= acked {
                            can.push((o.base, 0));
                        }
                        if !can.is_empty() {
                            let (p, seq) = can[r.below(can.len() as u64) as usize];
                            acked = p;
                            o.k.proven(seq);
                            log.push(format!("{step}: proven {p}"));
                        }
                    }
                }
                7..=9 => {
                    for (e, o) in owners.iter_mut().enumerate().filter(|(_, o)| o.on) {
                        if let Some(op) = o.k.next(|| r.next()) {
                            log.push(format!("{step}: owner {e} sends {op:?}"));
                            calls.push(Call { owner: e, op, landed: false, answered: false });
                        }
                    }
                }
                10..=12 if !calls.is_empty() => {
                    let n = calls.len() as u64;
                    let c = &mut calls[r.below(n) as usize];
                    if !c.landed {
                        c.landed = true;
                        match &c.op {
                            Op::Put(k) => bucket.insert(k.clone()),
                            Op::Delete(k) => bucket.remove(k),
                        };
                        log.push(format!("{step}: lands {:?}", c.op));
                    }
                }
                // An answer: ok only once it landed. One that failed may
                // still land later, or never.
                13..=15 if !calls.is_empty() => {
                    let n = calls.len() as u64;
                    let c = &mut calls[r.below(n) as usize];
                    if !c.answered {
                        c.answered = true;
                        let ok = c.landed && r.chance(0.7);
                        if let Some(o) = owners.get_mut(c.owner).filter(|o| o.on) {
                            o.k.done(&c.op, ok);
                            log.push(format!("{step}: owner {} hears {:?} {}", c.owner, c.op, if ok { "ok" } else { "failed" }));
                        }
                    }
                }
                _ => {}
            }
            calls.retain(|c| !(c.landed && c.answered));
            for (p, a) in hist.iter().enumerate().skip(acked) {
                if let Some(at) = a {
                    if !bucket.iter().any(|k| due(k) == Some(*at)) {
                        return Err(format!("state {p} (alarm {at}) could come back, and the index lacks it:\n{}", log.join("\n")));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn every_state_that_could_come_back_has_its_alarm_indexed() {
        for seed in 0..100_000 {
            if let Err(e) = run(seed, Bugs::default()) {
                panic!("seed {seed}: {e}");
            }
        }
    }

    /// Each rule from before loses an alarm, and the runs above see it.
    #[test]
    fn the_runs_catch_the_old_rules() {
        let bad = [
            ("delete on commit", Bugs { delete_on_commit: true, ..Default::default() }),
            ("trust listed keys", Bugs { trust_listed: true, ..Default::default() }),
            ("reuse keys", Bugs { reuse_keys: true, ..Default::default() }),
            ("keep the newest", Bugs { keep_newest: true, ..Default::default() }),
        ];
        for (name, bugs) in bad {
            assert!((0..20_000).any(|seed| run(seed, bugs).is_err()), "{name}: no run lost an alarm");
        }
    }

    #[test]
    fn keys_read_old_and_new() {
        assert_eq!(due(&key(OBJ, 1_700_000_000_000, 0xdead)), Some(1_700_000_000_000));
        assert_eq!(due("alarm/000000000000000000000000000000a1/1700000000000"), Some(1_700_000_000_000));
        assert!(key(OBJ, 5, 1) < key(OBJ, 6, 0), "keys sort by due time");
    }
}
