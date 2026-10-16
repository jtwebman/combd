//! A bot's brain, for the bots program and for a match that plays a bot
//! itself: what to do next, from only what that side is shown (the other
//! player behind a wall is never sent). It plays like a person, not a
//! machine: it fires only once it has seen the other player for a reaction
//! time, aims where it saw them that long ago, and misses by a little.

use crate::game::{blocked, sees, R, SPAWNS, WALLS};
use serde_json::{json, Value};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Strategy {
    Chaser,
    Sniper,
    Camper,
    Random,
}

/// splitmix64, for wandering.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl Strategy {
    pub const ALL: [Strategy; 4] = [Strategy::Chaser, Strategy::Sniper, Strategy::Camper, Strategy::Random];

    pub fn name(self) -> &'static str {
        match self {
            Strategy::Chaser => "chaser",
            Strategy::Sniper => "sniper",
            Strategy::Camper => "camper",
            Strategy::Random => "random",
        }
    }

    pub fn from_name(n: &str) -> Option<Strategy> {
        Strategy::ALL.into_iter().find(|s| s.name() == n)
    }
}

/// One bot in one match: its strategy, and what it has seen.
pub struct Brain {
    pub s: Strategy,
    rng: Rng,
    /// Reaction time (ms) and aim error (radians, either way).
    react: u64,
    aim_err: f64,
    last_seen: Option<(f64, f64)>,
    /// Where the other player was while in sight: (ms, position).
    seen: VecDeque<(u64, (f64, f64))>,
    seen_since: Option<u64>,
}

impl Brain {
    pub fn new(s: Strategy, seed: u64, react: u64, aim_err: f64) -> Brain {
        Brain { s, rng: Rng(seed | 1), react, aim_err, last_seen: None, seen: VecDeque::new(), seen_since: None }
    }

    /// The next input for `side`, from that side's view (`Game::view`), at
    /// `now` ms (any clock that counts up).
    pub fn input(&mut self, view: &Value, side: usize, now: u64) -> Value {
        match (view["them"]["x"].as_f64(), view["them"]["y"].as_f64()) {
            (Some(x), Some(y)) => {
                self.last_seen = Some((x, y));
                self.seen_since.get_or_insert(now);
                self.seen.push_back((now, (x, y)));
            }
            _ => {
                self.seen_since = None;
                self.seen.clear();
            }
        }
        // A reaction time: aim where they were that long ago.
        while self.seen.len() > 1 && self.seen[1].0 + self.react <= now {
            self.seen.pop_front();
        }
        let lagged = self.seen.front().map(|p| p.1);
        let ready = self.seen_since.is_some_and(|t| now >= t + self.react);
        think(self.s, &mut self.rng, &view["you"], lagged, self.last_seen, side, now, ready, self.aim_err)
    }
}

/// Where to go next, toward (tx, ty): straight if that's free, else the
/// nearest angle off it that is.
fn steer(x: f64, y: f64, tx: f64, ty: f64) -> (f64, f64) {
    let base = (ty - y).atan2(tx - x);
    for off in [0.0, 0.5, -0.5, 1.0, -1.0, 1.6, -1.6, 2.4, -2.4] {
        let a = base + off;
        let (mx, my) = (a.cos(), a.sin());
        if !blocked(x + mx * (R + 6.0), y + my * (R + 6.0)) {
            return (mx, my);
        }
    }
    (0.0, 0.0)
}

/// A spot behind the nearest wall, out of sight of (ex, ey), if there's one.
fn cover(x: f64, y: f64, ex: f64, ey: f64) -> (f64, f64) {
    let mut best = (x, y, f64::MAX);
    for &(wx, wy, ww, wh) in WALLS.iter() {
        for (cx, cy) in [(wx - R - 8.0, wy + wh / 2.0), (wx + ww + R + 8.0, wy + wh / 2.0), (wx + ww / 2.0, wy - R - 8.0), (wx + ww / 2.0, wy + wh + R + 8.0)] {
            if blocked(cx, cy) || sees(cx, cy, ex, ey) {
                continue;
            }
            let d = (cx - x).hypot(cy - y);
            if d < best.2 {
                best = (cx, cy, d);
            }
        }
    }
    (best.0, best.1)
}

/// One input, from what this bot knows.
#[allow(clippy::too_many_arguments)]
fn think(s: Strategy, rng: &mut Rng, me: &Value, them: Option<(f64, f64)>, last: Option<(f64, f64)>, side: usize, t: u64, ready: bool, aim_err: f64) -> Value {
    let (x, y) = (me["x"].as_f64().unwrap_or(0.0), me["y"].as_f64().unwrap_or(0.0));
    // With nothing seen yet: the other spawn.
    let (ex, ey) = them.or(last).unwrap_or(SPAWNS[1 - side]);
    let aim = (ey - y).atan2(ex - x);
    let seen = them.is_some();
    let d = (ex - x).hypot(ey - y);
    let ((mx, my), fire) = match s {
        Strategy::Chaser => (if d > 60.0 { steer(x, y, ex, ey) } else { (0.0, 0.0) }, seen),
        Strategy::Sniper => {
            let (cx, cy) = if d < 350.0 { (x - (ex - x), y - (ey - y)) } else if d > 450.0 { (ex, ey) } else {
                // Strafe: across the line to them, switching every 2 s.
                let side_step = if (t / 2000) % 2 == 0 { 1.0 } else { -1.0 };
                (x - (ey - y) * side_step, y + (ex - x) * side_step)
            };
            (steer(x, y, cx, cy), seen && d < 600.0)
        }
        Strategy::Camper => {
            // In cover most of the time; every 4 s, a step toward them to peek.
            let peek = (t / 1000) % 4 == 3;
            let (cx, cy) = if peek { (ex, ey) } else { cover(x, y, ex, ey) };
            let m = if (cx - x).hypot(cy - y) < 6.0 { (0.0, 0.0) } else { steer(x, y, cx, cy) };
            (m, seen)
        }
        Strategy::Random => {
            let a = rng.f() * std::f64::consts::TAU;
            ((a.cos(), a.sin()), rng.f() < 0.3)
        }
    };
    let aim = if s == Strategy::Random && !seen { rng.f() * std::f64::consts::TAU } else { aim + (rng.f() * 2.0 - 1.0) * aim_err };
    json!({ "t": "in", "mx": mx, "my": my, "aim": aim, "fire": fire && (ready || s == Strategy::Random) })
}

