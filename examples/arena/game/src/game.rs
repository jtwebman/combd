//! The arena: a 2D, top-down 1 vs 1 shooter, decided on the server. Plain
//! and pure (no I/O), so it's tested natively and the bots can use its map.
//!
//! Each tick the server moves both players (walls block them), fires and
//! moves bullets, and scores hits. Then each player gets their own view: the
//! other player and bullets only where a line from them isn't blocked by a
//! wall. Hidden positions never leave the server, so a client can't cheat
//! its way to them.

use serde_json::{json, Value};

pub const W: f64 = 960.0;
pub const H: f64 = 640.0;
pub const R: f64 = 14.0;
pub const SPEED: f64 = 220.0;
pub const BULLET_SPEED: f64 = 700.0;
pub const BULLET_LIFE_MS: u64 = 1200;
pub const FIRE_MS: u64 = 350;
pub const HP: u32 = 3;
pub const RESPAWN_MS: u64 = 1500;
pub const KILLS_TO_WIN: u32 = 3;

/// Walls: (x, y, w, h), plus the arena's edge.
pub const WALLS: [(f64, f64, f64, f64); 7] = [
    (300.0, 140.0, 40.0, 220.0),
    (620.0, 280.0, 40.0, 220.0),
    (430.0, 60.0, 100.0, 30.0),
    (430.0, 550.0, 100.0, 30.0),
    (140.0, 450.0, 130.0, 30.0),
    (690.0, 160.0, 130.0, 30.0),
    (455.0, 300.0, 50.0, 40.0),
];

pub const SPAWNS: [(f64, f64); 2] = [(80.0, 320.0), (880.0, 320.0)];

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Input {
    /// Movement, each -1 to 1.
    pub mx: f64,
    pub my: f64,
    /// Where they aim, in radians.
    pub aim: f64,
    pub fire: bool,
}

#[derive(Clone, Debug)]
pub struct Player {
    pub x: f64,
    pub y: f64,
    pub aim: f64,
    pub hp: u32,
    pub kills: u32,
    /// Dead until this time (ms), then back at their spawn.
    pub dead_until: Option<u64>,
    pub last_fire: u64,
    pub input: Input,
}

#[derive(Clone, Debug)]
pub struct Bullet {
    pub x: f64,
    pub y: f64,
    pub vx: f64,
    pub vy: f64,
    pub owner: usize,
    pub born: u64,
}

#[derive(Clone, Debug)]
pub struct Game {
    pub players: [Player; 2],
    pub bullets: Vec<Bullet>,
    pub winner: Option<usize>,
}

fn spawn(side: usize) -> Player {
    let (x, y) = SPAWNS[side];
    Player { x, y, aim: if side == 0 { 0.0 } else { std::f64::consts::PI }, hp: HP, kills: 0, dead_until: None, last_fire: 0, input: Input::default() }
}

/// Does a circle at (x, y) of radius r touch the rectangle?
fn circle_hits(x: f64, y: f64, r: f64, (rx, ry, rw, rh): (f64, f64, f64, f64)) -> bool {
    let cx = x.clamp(rx, rx + rw);
    let cy = y.clamp(ry, ry + rh);
    (x - cx).powi(2) + (y - cy).powi(2) < r * r
}

fn in_rect(x: f64, y: f64, (rx, ry, rw, rh): (f64, f64, f64, f64)) -> bool {
    x >= rx && x <= rx + rw && y >= ry && y <= ry + rh
}

/// Does the segment from a to b cross the rectangle? (The slab test.)
fn segment_hits(ax: f64, ay: f64, bx: f64, by: f64, (rx, ry, rw, rh): (f64, f64, f64, f64)) -> bool {
    let (dx, dy) = (bx - ax, by - ay);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, ax - rx), (dx, rx + rw - ax), (-dy, ay - ry), (dy, ry + rh - ay)] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
            if t0 > t1 {
                return false;
            }
        }
    }
    true
}

/// Can someone at a see b? No wall in between.
pub fn sees(ax: f64, ay: f64, bx: f64, by: f64) -> bool {
    !WALLS.iter().any(|&w| segment_hits(ax, ay, bx, by, w))
}

fn blocked(x: f64, y: f64) -> bool {
    !(R..=W - R).contains(&x) || !(R..=H - R).contains(&y) || WALLS.iter().any(|&w| circle_hits(x, y, R, w))
}

impl Game {
    pub fn new() -> Game {
        Game { players: [spawn(0), spawn(1)], bullets: vec![], winner: None }
    }

    /// One step: `dt` seconds, at time `now` (ms).
    pub fn step(&mut self, now: u64, dt: f64) {
        if self.winner.is_some() {
            return;
        }
        for side in 0..2 {
            let p = &mut self.players[side];
            if let Some(t) = p.dead_until {
                if now < t {
                    continue;
                }
                let kills = p.kills;
                *p = Player { kills, ..spawn(side) };
            }
            let i = p.input;
            let (mut mx, mut my) = (i.mx.clamp(-1.0, 1.0), i.my.clamp(-1.0, 1.0));
            let len = (mx * mx + my * my).sqrt();
            if len > 1.0 {
                mx /= len;
                my /= len;
            }
            // Each axis on its own, so a player slides along a wall.
            let nx = p.x + mx * SPEED * dt;
            if !blocked(nx, p.y) {
                p.x = nx;
            }
            let ny = p.y + my * SPEED * dt;
            if !blocked(p.x, ny) {
                p.y = ny;
            }
            if i.aim.is_finite() {
                p.aim = i.aim;
            }
            if i.fire && now.saturating_sub(p.last_fire) >= FIRE_MS {
                p.last_fire = now;
                let (c, s) = (p.aim.cos(), p.aim.sin());
                self.bullets.push(Bullet { x: p.x + c * (R + 4.0), y: p.y + s * (R + 4.0), vx: c * BULLET_SPEED, vy: s * BULLET_SPEED, owner: side, born: now });
            }
        }
        // Bullets, in small steps so a fast one can't skip through a wall.
        let steps = ((BULLET_SPEED * dt) / 8.0).ceil().max(1.0) as usize;
        let mut alive = vec![];
        'bullets: for mut b in std::mem::take(&mut self.bullets) {
            if now.saturating_sub(b.born) > BULLET_LIFE_MS {
                continue;
            }
            for _ in 0..steps {
                b.x += b.vx * dt / steps as f64;
                b.y += b.vy * dt / steps as f64;
                if !(0.0..=W).contains(&b.x) || !(0.0..=H).contains(&b.y) || WALLS.iter().any(|&w| in_rect(b.x, b.y, w)) {
                    continue 'bullets;
                }
                let target = 1 - b.owner;
                let t = &mut self.players[target];
                if t.dead_until.is_none() && (t.x - b.x).powi(2) + (t.y - b.y).powi(2) < (R + 3.0).powi(2) {
                    t.hp = t.hp.saturating_sub(1);
                    if t.hp == 0 {
                        t.dead_until = Some(now + RESPAWN_MS);
                        self.players[b.owner].kills += 1;
                        if self.players[b.owner].kills >= KILLS_TO_WIN {
                            self.winner = Some(b.owner);
                        }
                    }
                    continue 'bullets;
                }
            }
            alive.push(b);
        }
        self.bullets = alive;
    }

    /// What `side` may see: themselves, the other player only in sight, and
    /// the bullets in sight.
    pub fn view(&self, side: usize) -> Value {
        let me = &self.players[side];
        let them = &self.players[1 - side];
        let visible = them.dead_until.is_none() && me.dead_until.is_none() && sees(me.x, me.y, them.x, them.y);
        let pos = |p: &Player| json!({ "x": p.x.round(), "y": p.y.round(), "aim": (p.aim * 100.0).round() / 100.0, "hp": p.hp, "dead": p.dead_until.is_some() });
        let bullets: Vec<Value> = self.bullets.iter().filter(|b| b.owner == side || sees(me.x, me.y, b.x, b.y)).map(|b| json!([b.x.round(), b.y.round()])).collect();
        json!({
            "you": pos(me),
            "them": if visible { pos(them) } else { Value::Null },
            "b": bullets,
            "score": [self.players[side].kills, self.players[1 - side].kills],
        })
    }
}

impl Default for Game {
    fn default() -> Game {
        Game::new()
    }
}

pub fn walls() -> Value {
    json!({ "w": W, "h": H, "r": R, "walls": WALLS.iter().map(|w| json!([w.0, w.1, w.2, w.3])).collect::<Vec<_>>() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walls_block_sight_and_movement() {
        // Spawns can't see each other through the middle wall.
        assert!(!sees(80.0, 320.0, 880.0, 320.0) || WALLS.iter().all(|&w| !segment_hits(80.0, 320.0, 880.0, 320.0, w)));
        assert!(!sees(250.0, 250.0, 400.0, 250.0), "the wall at x 300..340 is in between");
        assert!(sees(250.0, 100.0, 400.0, 100.0), "above that wall, nothing is in between");
        let mut g = Game::new();
        g.players[0].x = 280.0;
        g.players[0].y = 250.0;
        g.players[0].input = Input { mx: 1.0, ..Input::default() };
        for t in 0..20 {
            g.step(t * 50, 0.05);
        }
        assert!(g.players[0].x <= 300.0 - R + 0.5, "stopped at the wall: {}", g.players[0].x);
    }

    #[test]
    fn hidden_players_stay_hidden() {
        let mut g = Game::new();
        g.players[0].x = 250.0;
        g.players[0].y = 250.0;
        g.players[1].x = 400.0;
        g.players[1].y = 250.0;
        assert!(g.view(0)["them"].is_null(), "behind the wall: not sent at all");
        g.players[1].y = 100.0;
        g.players[0].y = 100.0;
        assert!(!g.view(0)["them"].is_null(), "around it: seen");
    }

    #[test]
    fn three_kills_win() {
        let mut g = Game::new();
        // Face to face, in the open, and side 0 keeps firing.
        g.players[0].x = 200.0;
        g.players[0].y = 600.0;
        g.players[1].x = 400.0;
        g.players[1].y = 600.0;
        g.players[0].input = Input { aim: 0.0, fire: true, ..Input::default() };
        let mut now = 0;
        while g.winner.is_none() && now < 60_000 {
            now += 50;
            g.step(now, 0.05);
            // Side 1 respawns far away: bring it back into the line of fire.
            if g.players[1].dead_until.is_none() {
                g.players[1].x = 400.0;
                g.players[1].y = 600.0;
            }
        }
        assert_eq!(g.winner, Some(0), "after {now} ms: {:?}", g.players[0].kills);
    }
}
