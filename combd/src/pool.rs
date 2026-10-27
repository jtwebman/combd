//! A pool's decisions, with no I/O: who holds which member, who waits, when
//! to start a member and when to stop one. The caller does what it says
//! (`Out`) and tells it what happened. Time is passed in, so it's tested
//! like the ownership core.
//!
//! A member is something worth sharing and slow to make (a Postgres
//! connection). The pool starts one when it's asked and none is free, up to
//! `max`. At max, an ask waits in line, and fails if it waits past its
//! deadline. A member that sits free for `idle_ms` is stopped. A member
//! started on the asker's node is reused by askers on that node first, then
//! by the nearest one.
//!
//! A node may keep a member it was given for its own next asks (they never
//! reach the core): to the core it stays busy. When asks wait at max, the
//! core recalls the nearest kept member, and its node hands it back once its
//! current lease is done.

use std::collections::{HashMap, VecDeque};

pub type Node = u64;

/// How long a member's news may take to reach the manager (`report`).
const GRACE: u64 = 2000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cfg {
    pub max: usize,
    pub idle_ms: u64,
    pub wait_ms: u64,
}

#[derive(Debug, PartialEq)]
pub enum Out {
    /// Lease free member `member` to ask `ask`.
    Grant { ask: u64, member: u64 },
    /// Start member `member` on `node`, leased to `ask` once it's up.
    Start { ask: u64, member: u64, node: Node },
    /// The ask waited too long.
    Fail { ask: u64 },
    Stop { member: u64 },
    /// Asks are waiting: hand member `member` back when its lease is done.
    Recall { member: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum St {
    Starting,
    Busy,
    /// Released, and cleaning up: free in a moment.
    Resetting,
    Free,
    Stopping,
}

struct Member {
    node: Node,
    st: St,
    since: u64,
    /// While it starts: the ask it's for.
    ask: Option<Ask>,
    /// Asked back from its node (`Out::Recall`), not back yet.
    recalled: bool,
}

struct Ask {
    id: u64,
    node: Node,
    /// Round trips (ms) from the asker to other nodes, nearest wins.
    near: Vec<(Node, u64)>,
    deadline: u64,
}

pub struct Core {
    cfg: Cfg,
    members: HashMap<u64, Member>,
    queue: VecDeque<Ask>,
    next: u64,
    /// The database said it's full: no new members until then.
    full_until: u64,
}

impl Core {
    pub fn new(cfg: Cfg) -> Core {
        Core { cfg, members: HashMap::new(), queue: VecDeque::new(), next: 0, full_until: 0 }
    }

    #[cfg(test)]
    pub fn live(&self) -> usize {
        self.members.len()
    }

    pub fn ask(&mut self, now: u64, id: u64, node: Node, near: Vec<(Node, u64)>) -> Vec<Out> {
        self.queue.push_back(Ask { id, node, near, deadline: now + self.cfg.wait_ms });
        self.pump(now)
    }

    /// A member started (`Ok`), or couldn't (`full`: the database has no
    /// room; its ask goes back to the front of the line).
    pub fn started(&mut self, now: u64, member: u64, ok: bool, full: bool) -> Vec<Out> {
        if ok {
            if let Some(m) = self.members.get_mut(&member) {
                m.st = St::Busy;
                m.since = now;
                m.ask = None;
            }
            return vec![];
        }
        let ask = self.members.remove(&member).and_then(|m| m.ask);
        if full {
            self.full_until = now + 1000;
            if let Some(a) = ask {
                self.queue.push_front(a);
            }
        }
        self.pump(now)
    }

    /// A member is free again. One we don't know (from before a restart) stops.
    pub fn free(&mut self, now: u64, member: u64) -> Vec<Out> {
        match self.members.get_mut(&member) {
            Some(m) if matches!(m.st, St::Busy | St::Resetting) => {
                m.st = St::Free;
                m.since = now;
                m.recalled = false;
                self.pump(now)
            }
            Some(_) => vec![],
            None => vec![Out::Stop { member }],
        }
    }

    /// A member was released and is resetting. It'll be free soon, so an
    /// ask waits for it rather than starting another (a reset is much
    /// cheaper than a connect).
    pub fn resetting(&mut self, member: u64) {
        if let Some(m) = self.members.get_mut(&member).filter(|m| m.st == St::Busy) {
            m.st = St::Resetting;
        }
    }

    /// What a node says its members are (`busy` or not), once a second: the
    /// books catch up with lost messages. A member it lists that isn't
    /// known stops; a known one it doesn't list is gone; one busy here but
    /// free there is free. Changes younger than `GRACE` are left alone: their
    /// news may still be on its way.
    pub fn report(&mut self, now: u64, node: Node, members: &[(u64, bool)]) -> Vec<Out> {
        let mut out = vec![];
        for &(id, busy) in members {
            match self.members.get_mut(&id) {
                None => out.push(Out::Stop { member: id }),
                Some(m) if !busy && matches!(m.st, St::Busy | St::Resetting) && now >= m.since + GRACE => {
                    m.st = St::Free;
                    m.since = now;
                }
                _ => {}
            }
        }
        self.members.retain(|id, m| m.node != node || now < m.since + GRACE || members.iter().any(|x| x.0 == *id));
        out.extend(self.pump(now));
        out
    }

    /// The asker was served some other way (its node had a member free).
    pub fn cancel(&mut self, ask: u64) {
        self.queue.retain(|a| a.id != ask);
    }

    pub fn gone(&mut self, now: u64, member: u64) -> Vec<Out> {
        self.members.remove(&member);
        self.pump(now)
    }

    pub fn tick(&mut self, now: u64) -> Vec<Out> {
        let mut out = vec![];
        for (&id, m) in self.members.iter_mut() {
            if m.st == St::Free && now >= m.since + self.cfg.idle_ms {
                m.st = St::Stopping;
                out.push(Out::Stop { member: id });
            }
        }
        out.extend(self.pump(now));
        out
    }

    /// Serve the line in order, as far as it can be.
    fn pump(&mut self, now: u64) -> Vec<Out> {
        let mut out = vec![];
        let mut i = 0;
        // Members about to be free: resetting, or recalled from a node.
        let mut soon = self.members.values().filter(|m| m.st == St::Resetting || (m.recalled && m.st == St::Busy)).count();
        // Counted once, not per ask: with nothing free at max, every ask in
        // a long queue gets the same answer, and scanning every member for
        // each of them was most of a node's CPU (1,000 asks, 200 members).
        let mut n_free = self.members.values().filter(|m| m.st == St::Free).count();
        // Busy members a node could be asked to hand back, by node (a few
        // nodes: a Vec, not a hash per ask).
        let (mut n_back, mut back_on) = (0, Vec::<(Node, usize)>::new());
        let on = |v: &mut Vec<(Node, usize)>, n: Node| -> usize {
            match v.iter().position(|x| x.0 == n) {
                Some(i) => i,
                None => {
                    v.push((n, 0));
                    v.len() - 1
                }
            }
        };
        for m in self.members.values().filter(|m| m.st == St::Busy && !m.recalled) {
            n_back += 1;
            let k = on(&mut back_on, m.node);
            back_on[k].1 += 1;
        }
        while i < self.queue.len() {
            let a = &self.queue[i];
            if now >= a.deadline {
                out.push(Out::Fail { ask: a.id });
                self.queue.remove(i);
                continue;
            }
            let cap = if now < self.full_until { 0 } else { self.cfg.max };
            // Nothing free, no room, nobody elsewhere to call back: it waits.
            if n_free == 0 && self.members.len() >= cap && (soon > 0 || n_back == back_on.iter().find(|x| x.0 == a.node).map_or(0, |x| x.1)) {
                soon = soon.saturating_sub(1);
                i += 1;
                continue;
            }
            let dist = |n: Node| if n == a.node { Some(0) } else { a.near.iter().find(|x| x.0 == n).map(|x| x.1 + 1) };
            let free = self.members.iter().filter(|(_, m)| m.st == St::Free).min_by_key(|(&id, m)| (dist(m.node).unwrap_or(u64::MAX), id)).map(|(&id, _)| id);
            // Below max, a member on the asker's node beats a far free one.
            let near_free = free.filter(|id| dist(self.members[id].node) == Some(0));
            if let Some(m) = near_free.or(if self.members.len() >= cap { free } else { None }) {
                let m_ = self.members.get_mut(&m).unwrap();
                m_.st = St::Busy;
                m_.since = now;
                n_free -= 1;
                n_back += 1;
                let k = on(&mut back_on, m_.node);
                back_on[k].1 += 1;
                out.push(Out::Grant { ask: a.id, member: m });
                self.queue.remove(i);
            } else if soon > 0 {
                soon -= 1;
                i += 1;
            } else if self.members.len() < cap {
                self.next += 1;
                let id = self.next;
                let a = self.queue.remove(i).unwrap();
                out.push(Out::Start { ask: a.id, member: id, node: a.node });
                self.members.insert(id, Member { node: a.node, st: St::Starting, since: now, ask: Some(a), recalled: false });
            } else {
                // At max: ask the nearest other node keeping a member to hand
                // it back (the asker's own node serves it from its own).
                let back = self.members.iter().filter(|(_, m)| m.st == St::Busy && !m.recalled && m.node != a.node).min_by_key(|(&id, m)| (dist(m.node).unwrap_or(u64::MAX), id)).map(|(&id, _)| id);
                if let Some(id) = back {
                    let m = self.members.get_mut(&id).unwrap();
                    m.recalled = true;
                    n_back -= 1;
                    let k = on(&mut back_on, m.node);
                    back_on[k].1 -= 1;
                    out.push(Out::Recall { member: id });
                }
                i += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: Cfg = Cfg { max: 2, idle_ms: 1000, wait_ms: 500 };

    #[test]
    fn grows_to_max_then_queues_then_fails_late_asks() {
        let mut p = Core::new(CFG);
        assert_eq!(p.ask(0, 1, 0, vec![]), vec![Out::Start { ask: 1, member: 1, node: 0 }]);
        // Asked while the first is still starting: a second member.
        assert_eq!(p.ask(0, 2, 0, vec![]), vec![Out::Start { ask: 2, member: 2, node: 0 }]);
        assert!(p.started(1, 1, true, false).is_empty());
        assert!(p.started(1, 2, true, false).is_empty());
        // At max: wait in line.
        assert!(p.ask(10, 3, 0, vec![]).is_empty());
        assert!(p.ask(10, 4, 0, vec![]).is_empty());
        // A member comes back: the first in line gets it.
        assert_eq!(p.free(20, 2), vec![Out::Grant { ask: 3, member: 2 }]);
        // The other waited too long.
        assert_eq!(p.tick(510), vec![Out::Fail { ask: 4 }]);
        assert_eq!(p.live(), 2);
    }

    #[test]
    fn an_ask_waits_for_a_resetting_member_instead_of_starting_one() {
        let mut p = Core::new(CFG);
        p.ask(0, 1, 0, vec![]);
        p.started(0, 1, true, false);
        p.resetting(1);
        assert!(p.ask(1, 2, 0, vec![]).is_empty());
        assert_eq!(p.free(2, 1), vec![Out::Grant { ask: 2, member: 1 }]);
        // A second ask doesn't wait on the same one.
        p.resetting(1);
        assert!(p.ask(3, 3, 0, vec![]).is_empty());
        assert_eq!(p.ask(3, 4, 0, vec![]), vec![Out::Start { ask: 4, member: 2, node: 0 }]);
    }

    #[test]
    fn idle_members_stop_and_unknown_ones_are_told_to() {
        let mut p = Core::new(CFG);
        p.ask(0, 1, 0, vec![]);
        p.started(0, 1, true, false);
        assert!(p.free(100, 1).is_empty());
        assert!(p.tick(900).is_empty());
        assert_eq!(p.tick(1100), vec![Out::Stop { member: 1 }]);
        // Stopping: not handed out.
        assert_eq!(p.ask(1100, 2, 0, vec![]), vec![Out::Start { ask: 2, member: 2, node: 0 }]);
        assert!(p.gone(1200, 1).is_empty());
        assert_eq!(p.free(1200, 77), vec![Out::Stop { member: 77 }]);
    }

    #[test]
    fn a_full_database_makes_the_ask_wait_not_fail() {
        let mut p = Core::new(Cfg { wait_ms: 5000, ..CFG });
        p.ask(0, 1, 0, vec![]);
        // "too many clients": back in line, no new member for a second.
        assert!(p.started(5, 1, false, true).is_empty());
        assert_eq!(p.live(), 0);
        assert!(p.tick(500).is_empty());
        assert_eq!(p.tick(1005), vec![Out::Start { ask: 1, member: 2, node: 0 }]);
    }

    #[test]
    fn at_max_kept_members_are_recalled_one_per_waiting_ask() {
        let mut p = Core::new(Cfg { max: 2, ..CFG });
        for (ask, node) in [(1, 10), (2, 20)] {
            p.ask(0, ask, node, vec![]);
            p.started(0, ask, true, false);
        }
        // Node 30 asks twice at max: the nearest kept member first, then the other.
        assert_eq!(p.ask(1, 3, 30, vec![(20, 1), (10, 5)]), vec![Out::Recall { member: 2 }]);
        assert_eq!(p.ask(1, 4, 30, vec![(20, 1), (10, 5)]), vec![Out::Recall { member: 1 }]);
        // A third ask waits: nothing left to recall. Nor from the asker's
        // own node: it serves that from its own.
        assert!(p.ask(1, 5, 30, vec![]).is_empty());
        assert!(p.ask(1, 6, 10, vec![]).is_empty());
        // Member 2 comes back: the first ask gets it, and the last one in
        // line can wait on it next.
        assert_eq!(p.free(2, 2), vec![Out::Grant { ask: 3, member: 2 }, Out::Recall { member: 2 }]);
        // Ask 4 was served elsewhere: cancelled, member 1 back goes to ask 5.
        p.cancel(4);
        p.cancel(6);
        assert_eq!(p.free(3, 1), vec![Out::Grant { ask: 5, member: 1 }]);
    }

    #[test]
    fn reports_fix_the_books() {
        let mut p = Core::new(Cfg { max: 3, ..CFG });
        p.ask(0, 1, 10, vec![]);
        p.ask(0, 2, 10, vec![]);
        p.started(0, 1, true, false);
        p.started(0, 2, true, false);
        // Fresh news is left alone.
        assert!(p.report(500, 10, &[(1, false)]).is_empty());
        assert_eq!(p.live(), 2);
        // Later: member 1's "free" was lost, and member 2 is gone.
        assert!(p.report(3000, 10, &[(1, false)]).is_empty());
        assert_eq!(p.live(), 1);
        assert_eq!(p.ask(3000, 3, 10, vec![]), vec![Out::Grant { ask: 3, member: 1 }]);
        // Another node's members aren't touched; a stranger stops.
        assert_eq!(p.report(3000, 20, &[(99, false)]), vec![Out::Stop { member: 99 }]);
        assert_eq!(p.live(), 1);
    }

    #[test]
    fn reuse_prefers_this_node_then_the_nearest() {
        let mut p = Core::new(Cfg { max: 3, ..CFG });
        for (ask, node) in [(1, 10), (2, 20), (3, 30)] {
            p.ask(0, ask, node, vec![]);
            p.started(0, ask, true, false);
        }
        p.free(1, 1);
        p.free(1, 3);
        // Node 20 at max: node 30 is nearer than node 10.
        assert_eq!(p.ask(2, 9, 20, vec![(10, 5), (30, 1)]), vec![Out::Grant { ask: 9, member: 3 }]);
        // Below max, a node with no free member of its own starts one.
        let mut p = Core::new(Cfg { max: 3, ..CFG });
        p.ask(0, 1, 10, vec![]);
        p.started(0, 1, true, false);
        p.free(1, 1);
        assert_eq!(p.ask(2, 2, 20, vec![(10, 1)]), vec![Out::Start { ask: 2, member: 2, node: 20 }]);
        assert_eq!(p.ask(2, 3, 10, vec![]), vec![Out::Grant { ask: 3, member: 1 }]);
    }
}
