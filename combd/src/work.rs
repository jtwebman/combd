//! The work a pool's connections on one node share: statements in line,
//! pushed by the event loops, pulled by whichever connection is free. It
//! knows nothing of any database (Postgres's pool keeps `(statement,
//! reply)` here; another driver keeps its own items), and nothing of
//! threads: the pool's agent decides who pulls, and when there should be
//! more of them.
//!
//! The one rule that matters: an item is never left with nobody to pull it.
//! A puller that finds the line empty waits on it a while (`pull_wait`: a
//! push wakes it, with no word to the agent), then stops pulling inside the
//! same lock a push checks for pullers, so either it takes the item or the
//! push sees nobody pulling and says to tell the agent.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Work<T> {
    inner: Mutex<Inner<T>>,
    /// Pullers waiting for an item.
    ready: Condvar,
}

struct Inner<T> {
    line: VecDeque<(T, Instant)>,
    /// Connections taking from the line now.
    pulling: usize,
    /// Connections the agent has asked for, to pull, not here yet.
    asked: usize,
    /// The agent has been told, and hasn't looked since.
    told: bool,
    /// Pullers to let go at their next pull, for others waiting on a whole
    /// connection (a session): a puller with work never stops otherwise.
    yielding: usize,
    /// Pullers waiting on `ready` now.
    idle: usize,
}

impl<T> Default for Work<T> {
    fn default() -> Work<T> {
        Work { inner: Mutex::new(Inner { line: VecDeque::new(), pulling: 0, asked: 0, told: false, yielding: 0, idle: 0 }), ready: Condvar::new() }
    }
}

impl<T> Work<T> {
    /// In line until `deadline`. True: nobody pulls or is on the way, so
    /// tell the agent (once, until it looks).
    pub fn push(&self, item: T, deadline: Instant) -> bool {
        let mut w = self.inner.lock().unwrap();
        w.line.push_back((item, deadline));
        let tell = w.pulling == 0 && w.asked == 0 && !w.told;
        w.told |= tell;
        if w.idle > 0 {
            self.ready.notify_one();
        }
        tell
    }

    /// The next item for a puller, now. None: the line is empty (or the
    /// puller is let go, `set_yield`), and the caller is no longer pulling.
    pub fn pull(&self) -> Option<T> {
        self.pull_wait(Duration::ZERO, &AtomicBool::new(false))
    }

    /// The next item for a puller, waiting up to `idle` for one. None: none
    /// came, the puller is let go (`set_yield`), or `let_go` is set (`wake`
    /// tells waiting pullers to look); either way it's no longer pulling.
    pub fn pull_wait(&self, idle: Duration, let_go: &AtomicBool) -> Option<T> {
        let until = Instant::now() + idle;
        let mut w = self.inner.lock().unwrap();
        loop {
            if let_go.load(Relaxed) {
                w.pulling -= 1;
                return None;
            }
            if w.yielding > 0 {
                w.yielding -= 1;
                w.pulling -= 1;
                return None;
            }
            if let Some((item, _)) = w.line.pop_front() {
                return Some(item);
            }
            let now = Instant::now();
            if now >= until {
                w.pulling -= 1;
                return None;
            }
            w.idle += 1;
            w = self.ready.wait_timeout(w, until - now).unwrap().0;
            w.idle -= 1;
        }
    }

    /// Waiting pullers look again (one of them was let go).
    pub fn wake(&self) {
        let _w = self.inner.lock().unwrap();
        self.ready.notify_all();
    }

    /// Let `n` pullers go at their next pull (not more than are pulling),
    /// waiting ones at once.
    pub fn set_yield(&self, n: usize) {
        let mut w = self.inner.lock().unwrap();
        w.yielding = n.min(w.pulling);
        if w.yielding > 0 && w.idle > 0 {
            self.ready.notify_all();
        }
    }

    pub fn add_puller(&self) {
        self.inner.lock().unwrap().pulling += 1;
    }

    /// A puller stops with the line maybe not empty (it was called back):
    /// the agent, which hears it's free, finds another.
    pub fn stop_pulling(&self) {
        self.inner.lock().unwrap().pulling -= 1;
    }

    /// The agent asked for `n` more pullers; one of them arrived (or won't).
    pub fn asked(&self, n: usize) {
        self.inner.lock().unwrap().asked += n;
    }

    pub fn asked_done(&self) {
        let mut w = self.inner.lock().unwrap();
        w.asked = w.asked.saturating_sub(1);
    }

    /// One item, not as a puller (a connection on another node runs it).
    pub fn take(&self) -> Option<T> {
        self.inner.lock().unwrap().line.pop_front().map(|(item, _)| item)
    }

    /// Items past their deadline, oldest first (they're in line in the
    /// order they came, with one wait each, so they're at the front).
    pub fn expired(&self, now: Instant) -> Vec<T> {
        let mut w = self.inner.lock().unwrap();
        let mut out = vec![];
        while w.line.front().is_some_and(|(_, d)| now >= *d) {
            out.push(w.line.pop_front().unwrap().0);
        }
        out
    }

    /// For the agent: (waiting, pulling, asked for). It's looked, so the
    /// next push into an idle line tells it again.
    pub fn look(&self) -> (usize, usize, usize) {
        let mut w = self.inner.lock().unwrap();
        w.told = false;
        (w.line.len(), w.pulling, w.asked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn tells_once_until_it_looks_and_not_while_someone_pulls() {
        let w = Work::default();
        let later = Instant::now() + Duration::from_secs(60);
        assert!(w.push(1, later), "an idle line: tell the agent");
        assert!(!w.push(2, later), "told already");
        assert_eq!(w.look(), (2, 0, 0));
        w.add_puller();
        assert!(!w.push(3, later), "someone pulls");
        assert_eq!((w.pull(), w.pull(), w.pull()), (Some(1), Some(2), Some(3)));
        assert_eq!(w.pull(), None, "empty: it stops pulling");
        assert_eq!(w.look(), (0, 0, 0));
        w.asked(1);
        assert!(!w.push(4, later), "one's on the way");
        w.asked_done();
        assert_eq!(w.take(), Some(4));
        assert!(w.push(5, later), "idle again");
    }

    #[test]
    fn pullers_yield_when_asked() {
        let w = Work::default();
        let later = Instant::now() + Duration::from_secs(60);
        w.add_puller();
        w.add_puller();
        w.push(1, later);
        w.push(2, later);
        w.set_yield(5);
        assert_eq!(w.pull(), None, "let go");
        assert_eq!(w.pull(), None, "let go: only two were pulling");
        assert_eq!(w.look(), (2, 0, 0), "the items wait for the next puller");
        w.set_yield(1);
        w.add_puller();
        assert_eq!(w.pull(), Some(1), "nothing to yield to: none were pulling when asked");
    }

    /// A waiting puller gets what's pushed (no word to the agent), is let
    /// go by `set_yield` or its flag, and leaves when nothing comes.
    #[test]
    fn a_waiting_puller_is_woken() {
        let w = std::sync::Arc::new(Work::default());
        let later = Instant::now() + Duration::from_secs(60);
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let puller = |w: std::sync::Arc<Work<u32>>, flag: std::sync::Arc<AtomicBool>| {
            w.add_puller();
            std::thread::spawn(move || {
                let mut got = vec![];
                while let Some(i) = w.pull_wait(Duration::from_secs(5), &flag) {
                    got.push(i);
                }
                got
            })
        };
        let p = puller(w.clone(), flag.clone());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!w.push(1, later), "someone waits: no word to the agent");
        assert!(!w.push(2, later));
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        w.set_yield(1);
        assert_eq!(p.join().unwrap(), vec![1, 2]);
        assert!(t.elapsed() < Duration::from_secs(1), "a yield waited for the timeout");
        let p = puller(w.clone(), flag.clone());
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        flag.store(true, Relaxed);
        w.wake();
        assert_eq!(p.join().unwrap(), Vec::<u32>::new());
        assert!(t.elapsed() < Duration::from_secs(1), "let go waited for the timeout");
        flag.store(false, Relaxed);
        w.add_puller();
        let t = Instant::now();
        assert_eq!(w.pull_wait(Duration::from_millis(50), &flag), None);
        assert!(t.elapsed() >= Duration::from_millis(50));
        assert_eq!(w.look(), (0, 0, 0));
    }

    #[test]
    fn expired_items_leave_from_the_front() {
        let w = Work::default();
        let now = Instant::now();
        w.push(1, now - Duration::from_millis(1));
        w.push(2, now);
        w.push(3, now + Duration::from_secs(60));
        assert_eq!(w.expired(now), vec![1, 2]);
        assert_eq!(w.look(), (1, 0, 0));
    }

    /// Pushers on many threads; a stand-in agent starts a puller only when
    /// a push says to. Every item is pulled exactly once: none is left with
    /// nobody pulling.
    #[test]
    fn no_item_is_ever_stranded() {
        let w = std::sync::Arc::new(Work::default());
        let (tell_tx, tell_rx) = std::sync::mpsc::channel::<()>();
        let (got_tx, got_rx) = std::sync::mpsc::channel::<u64>();
        let later = Instant::now() + Duration::from_secs(600);
        let agent = {
            let w = w.clone();
            std::thread::spawn(move || {
                while tell_rx.recv().is_ok() {
                    w.look();
                    w.add_puller();
                    let (w, got) = (w.clone(), got_tx.clone());
                    std::thread::spawn(move || {
                        while let Some(i) = w.pull() {
                            got.send(i).unwrap();
                        }
                    });
                }
            })
        };
        let pushers: Vec<_> = (0..4u64)
            .map(|p| {
                let (w, tell) = (w.clone(), tell_tx.clone());
                std::thread::spawn(move || {
                    for i in 0..20_000 {
                        if w.push(p * 1_000_000 + i, later) {
                            tell.send(()).unwrap();
                        }
                        if i % 1000 == 0 {
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        for p in pushers {
            p.join().unwrap();
        }
        let mut seen = std::collections::HashSet::new();
        while seen.len() < 80_000 {
            let i = got_rx.recv_timeout(Duration::from_secs(10)).expect("an item was stranded");
            assert!(seen.insert(i), "{i} pulled twice");
        }
        drop(tell_tx);
        agent.join().unwrap();
    }
}
