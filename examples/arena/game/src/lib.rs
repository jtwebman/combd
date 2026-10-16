//! Arena, a combd example: real-time 1 vs 1 on Durable Objects. One module,
//! four processes (it asks `COMBD_PROCESS` which one it is):
//! - `lobby`, a worker: the page, and the join and poll API;
//! - `matchmaker`, durable (one, `global`): pairs players who are close to
//!   each other, picks the node that's best for both, starts the match there;
//! - `match`, durable (one per match): the game, 20 ticks a second
//!   (`tick_every`), over WebSockets. Either player may pause it, and a
//!   player who drops pauses it too; back within `PAUSE_MS` (30 s) and it
//!   goes on, or the other player wins;
//! - `player`, durable (one per player): wins and losses, from the match's
//!   `send` when it ends.

pub mod game;
pub mod bot;

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

/// The page: the game client.
const INDEX: &str = include_str!("../../client/index.html");
/// The git commit this was built from (`ARENA_VERSION`, set by CI).
const VERSION: &str = match option_env!("ARENA_VERSION") {
    Some(v) => v,
    None => "dev",
};

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "combd")]
    extern "C" {
        pub fn now_ms() -> i64;
        pub fn ws_send(id: u64, ptr: *const u8, len: u32) -> u32;
        pub fn ws_close(id: u64, code: u32) -> u32;
        pub fn tick_every(ms: u64) -> u32;
        pub fn call(ptr: *const u8, len: u32) -> u32;
        pub fn send(ptr: *const u8, len: u32) -> u32;
        pub fn pg_read(ptr: *mut u8, cap: u32) -> u32;
        pub fn sql(ptr: *const u8, len: u32) -> u32;
        pub fn sql_read(ptr: *mut u8);
    }
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        pub fn environ_sizes_get(count: *mut u32, size: *mut u32) -> u32;
        pub fn environ_get(ptrs: *mut *mut u8, buf: *mut u8) -> u32;
        pub fn random_get(buf: *mut u8, len: u32) -> u32;
    }
}

#[cfg(target_arch = "wasm32")]
fn now() -> u64 {
    unsafe { host::now_ms() as u64 }
}

#[cfg(target_arch = "wasm32")]
fn env(name: &str) -> Option<String> {
    let (mut n, mut size) = (0u32, 0u32);
    unsafe { host::environ_sizes_get(&mut n, &mut size) };
    let mut ptrs = vec![core::ptr::null_mut::<u8>(); n as usize];
    let mut buf = vec![0u8; size as usize];
    unsafe { host::environ_get(ptrs.as_mut_ptr(), buf.as_mut_ptr()) };
    let want = format!("{name}=");
    buf.split(|&b| b == 0).filter_map(|e| core::str::from_utf8(e).ok()).find_map(|e| e.strip_prefix(want.as_str()).map(String::from))
}

#[cfg(target_arch = "wasm32")]
fn token() -> String {
    let mut b = [0u8; 12];
    unsafe { host::random_get(b.as_mut_ptr(), b.len() as u32) };
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Another process: `{"to", "id"?, "node"?, "method"?, "path"?, "body"?}`.
/// Its answer: `{"status", "body"}` or `{"error"}`.
#[cfg(target_arch = "wasm32")]
fn call(msg: &Value) -> Value {
    let m = msg.to_string();
    let n = unsafe { host::call(m.as_ptr(), m.len() as u32) } as usize;
    let mut out = vec![0u8; n];
    let got = unsafe { host::pg_read(out.as_mut_ptr(), n as u32) } as usize;
    out.truncate(got);
    serde_json::from_slice(&out).unwrap_or(Value::Null)
}

#[cfg(target_arch = "wasm32")]
fn send(msg: &Value) {
    let m = msg.to_string();
    unsafe { host::send(m.as_ptr(), m.len() as u32) };
}

#[cfg(target_arch = "wasm32")]
fn ws_send(id: u64, msg: &str) {
    unsafe { host::ws_send(id, msg.as_ptr(), msg.len() as u32) };
}

#[cfg(target_arch = "wasm32")]
fn ws_close(id: u64) {
    unsafe { host::ws_close(id, 1000) };
}

#[cfg(target_arch = "wasm32")]
fn tick_every(ms: u64) {
    unsafe { host::tick_every(ms) };
}

#[cfg(target_arch = "wasm32")]
fn sql(q: &str, p: &[Value]) -> Vec<Value> {
    let req = json!({ "q": q, "p": p }).to_string();
    let n = unsafe { host::sql(req.as_ptr(), req.len() as u32) } as usize;
    let mut out = vec![0u8; n];
    unsafe { host::sql_read(out.as_mut_ptr()) };
    let v: Value = serde_json::from_slice(&out).unwrap_or(Value::Null);
    v["rows"].as_array().cloned().unwrap_or_default()
}

// Native builds (tests, the bots) never run the processes.
#[cfg(not(target_arch = "wasm32"))]
fn now() -> u64 {
    0
}
#[cfg(not(target_arch = "wasm32"))]
fn env(_: &str) -> Option<String> {
    None
}
#[cfg(not(target_arch = "wasm32"))]
fn token() -> String {
    String::new()
}
#[cfg(not(target_arch = "wasm32"))]
fn call(_: &Value) -> Value {
    Value::Null
}
#[cfg(not(target_arch = "wasm32"))]
fn send(_: &Value) {}
#[cfg(not(target_arch = "wasm32"))]
fn ws_send(_: u64, _: &str) {}
#[cfg(not(target_arch = "wasm32"))]
fn ws_close(_: u64) {}
#[cfg(not(target_arch = "wasm32"))]
fn tick_every(_: u64) {}
#[cfg(not(target_arch = "wasm32"))]
fn sql(_: &str, _: &[Value]) -> Vec<Value> {
    vec![]
}

/// Set up an object's tables once per instance, not on every request: a
/// statement that may write (even `CREATE TABLE IF NOT EXISTS` with the table
/// there) makes the request a write, and a write waits for the object's
/// peers, across oceans in a world-wide fleet.
fn once(f: impl FnOnce()) {
    thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if !DONE.with(|d| d.replace(true)) {
        f();
    }
}

fn ok(v: Value) -> String {
    format!("200 {v}")
}

fn err(code: u16, msg: &str) -> String {
    format!("{code} {}", json!({ "error": msg }))
}

/// One request: `api METHOD PATH BODY` (a worker) or `do METHOD PATH BODY`
/// (an object), sent to the process this cell is.
pub fn run(req: &str) -> String {
    let mut parts = req.splitn(4, ' ');
    let (_, method, path, body) = (parts.next(), parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = path.split('?').next().unwrap_or(path);
    match env("COMBD_PROCESS").as_deref() {
        Some("lobby") => lobby(method, path, body),
        Some("matchmaker") => matchmaker(method, path, body),
        Some("match") => matchbox(method, path, body),
        Some("player") => player(method, path, body),
        other => err(500, &format!("arena: no process {other:?}")),
    }
}

// ---------------------------------------------------------------- lobby

fn lobby(method: &str, path: &str, body: &str) -> String {
    let mm = |p: &str, b: &str| -> String {
        let r = call(&json!({ "to": "matchmaker", "id": "global", "path": p, "body": b }));
        match r["status"].as_u64() {
            Some(s) => format!("{s} {}", r["body"].as_str().unwrap_or("{}")),
            None => err(503, r["error"].as_str().unwrap_or("the matchmaker didn't answer")),
        }
    };
    match (method, path) {
        ("GET", "/") => format!("200\ncontent-type: text/html; charset=utf-8\ncache-control: no-store\n\n{}", INDEX.replace("{{VERSION}}", VERSION)),
        // A new version is coming (CI, with the drain key): no new matches.
        ("POST", "/api/drain") => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            match env("DRAIN_KEY") {
                Some(k) if !k.is_empty() && v["key"].as_str() == Some(k.as_str()) => mm("/drain", "{}"),
                _ => err(403, "no"),
            }
        }
        ("POST", "/api/join") => mm("/join", body),
        ("POST", "/api/poll") => mm("/poll", body),
        // No one around: play one of our bots, which the match plays itself.
        ("POST", "/api/bot") => mm("/bot", body),
        ("GET", "/api/stats") => {
            let r = call(&json!({ "to": "matchmaker", "id": "global", "method": "GET", "path": "/stats" }));
            format!("{} {}", r["status"].as_u64().unwrap_or(503), r["body"].as_str().unwrap_or("{}"))
        }
        ("GET", p) if p.starts_with("/api/player/") => {
            let id = &p["/api/player/".len()..];
            let r = call(&json!({ "to": "player", "id": id, "method": "GET", "path": "/" }));
            format!("{} {}", r["status"].as_u64().unwrap_or(503), r["body"].as_str().unwrap_or("{}"))
        }
        _ => err(404, "not found"),
    }
}

// ----------------------------------------------------------- matchmaker

struct Waiting {
    ticket: String,
    player: String,
    /// Round trips (ms) from this player to each node.
    rtt: BTreeMap<u64, u64>,
    since: u64,
}

struct Assigned {
    m: String,
    node: u64,
    token: String,
    side: usize,
    rtt: Option<u64>,
    at: u64,
}

#[derive(Default)]
struct Mm {
    queue: Vec<Waiting>,
    ready: HashMap<String, Assigned>,
    /// Counts matches in memory, only to name them (the count that's kept
    /// is in storage).
    matches: u64,
    /// A new version is coming: no new matches.
    draining: bool,
}

thread_local! {
    static MM: RefCell<Mm> = RefCell::new(Mm::default());
}

/// The node best for both, and how bad it is for the worse off of the two
/// (the larger round trip). Nodes neither measured don't count.
fn best_node(a: &BTreeMap<u64, u64>, b: &BTreeMap<u64, u64>) -> Option<(u64, u64)> {
    a.iter().filter_map(|(n, ra)| b.get(n).map(|rb| (*ra.max(rb), *n))).min().map(|(score, n)| (n, score))
}

/// How far apart two players may be and still be matched: 80 ms, plus 25 ms
/// for each second the one who's waited longer has waited.
fn fair_enough(score: u64, waited_ms: u64) -> bool {
    score <= 80 + 25 * waited_ms / 1000
}

/// Pair whoever can be paired: the closest pair first, each pair close
/// enough for how long its older player has waited. Each pair's match
/// starts on the node best for both.
fn pair_all(mm: &mut Mm, now: u64) {
    if mm.draining {
        return;
    }
    loop {
        let mut best: Option<(u64, usize, usize, u64)> = None;
        for i in 0..mm.queue.len() {
            for j in i + 1..mm.queue.len() {
                let (a, b) = (&mm.queue[i], &mm.queue[j]);
                if a.player == b.player {
                    continue;
                }
                let Some((node, score)) = best_node(&a.rtt, &b.rtt) else { continue };
                let waited = now.saturating_sub(a.since.min(b.since));
                if fair_enough(score, waited) && best.is_none_or(|x| score < x.0) {
                    best = Some((score, i, j, node));
                }
            }
        }
        let Some((score, i, j, node)) = best else { return };
        let (b, a) = (mm.queue.remove(j), mm.queue.remove(i));
        mm.matches += 1;
        let id = format!("m{}{}", now, mm.matches);
        let tokens = [token(), token()];
        // Start the match on the node best for both.
        let r = call(&json!({ "to": "match", "id": id, "node": node, "path": "/create", "body": json!({ "id": id, "players": [a.player, b.player], "tokens": tokens }).to_string() }));
        if r["status"].as_u64() != Some(200) {
            // Couldn't start it: they wait on, and the next tick tries again.
            mm.queue.push(a);
            mm.queue.push(b);
            return;
        }
        let row = json!({ "match": id, "node": node, "players": [a.player, b.player], "worst_rtt": score, "at": now });
        sql("INSERT INTO recent (row) VALUES (?)", &[json!(row.to_string())]);
        sql("DELETE FROM recent WHERE id <= (SELECT MAX(id) - 20 FROM recent)", &[]);
        sql("INSERT INTO kv (k, v) VALUES ('matches', 1) ON CONFLICT (k) DO UPDATE SET v = v + 1", &[]);
        mm.ready.insert(a.ticket.clone(), Assigned { m: id.clone(), node, token: tokens[0].clone(), side: 0, rtt: a.rtt.get(&node).copied(), at: now });
        mm.ready.insert(b.ticket.clone(), Assigned { m: id, node, token: tokens[1].clone(), side: 1, rtt: b.rtt.get(&node).copied(), at: now });
    }
}

const DRAINING: &str = "a new version is on its way: new matches start again in a minute or two";

fn matchmaker(method: &str, path: &str, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    once(|| {
        sql("CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v INTEGER)", &[]);
        sql("CREATE TABLE IF NOT EXISTS recent (id INTEGER PRIMARY KEY, row TEXT)", &[]);
    });
    MM.with(|mm| {
        let mut mm = mm.borrow_mut();
        let now = now();
        mm.queue.retain(|w| now.saturating_sub(w.since) < 60_000);
        mm.ready.retain(|_, a| now.saturating_sub(a.at) < 60_000);
        let answer = match (method, path) {
            // A new version is on its way: no new matches until it's here.
            ("POST", "/join" | "/bot") if mm.draining => err(503, DRAINING),
            ("POST", "/join") => {
                let Some(player) = v["player"].as_str().filter(|p| !p.is_empty() && p.len() <= 32) else { return err(400, "want {\"player\": name, \"rtt\": {node: ms}}") };
                let rtt: BTreeMap<u64, u64> = v["rtt"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_u64()?))).collect()).unwrap_or_default();
                if rtt.is_empty() {
                    return err(400, "measure the nodes first: rtt");
                }
                let ticket = token();
                // One ticket per player: a join again (a client that gave up
                // waiting for an answer) replaces the old one, so stale
                // tickets can't pile up and get matched with no one there.
                mm.queue.retain(|w| w.player != player);
                mm.queue.push(Waiting { ticket: ticket.clone(), player: player.into(), rtt, since: now });
                pair_all(&mut mm, now);
                ok(json!({ "ticket": ticket }))
            }
            ("POST", "/poll") => {
                // Pair on every poll (players poll twice a second while they
                // wait): the bar for "close enough" drops with time, so a
                // lone far player is paired soon enough, with no beat.
                pair_all(&mut mm, now);
                let ticket = v["ticket"].as_str().unwrap_or("");
                match mm.ready.remove(ticket) {
                    Some(a) => ok(json!({ "match": a.m, "node": a.node, "token": a.token, "side": a.side, "rtt": a.rtt })),
                    None if mm.queue.iter().any(|w| w.ticket == ticket) => ok(json!({ "waiting": true, "queue": mm.queue.len() })),
                    None => err(404, "no such ticket (it waited too long?): join again"),
                }
            }
            // From an earlier version's beat: stop it.
            ("TICK", _) => {
                tick_every(0);
                ok(json!({}))
            }
            // Play a bot now: the match plays it, on the node nearest you.
            // A new version is coming: start no new matches (the deploy reloads
            // this, and that ends it).
            ("POST", "/drain") => {
                mm.draining = true;
                ok(json!({ "draining": true }))
            }
            ("POST", "/bot") => {
                let Some(player) = v["player"].as_str().filter(|p| !p.is_empty() && p.len() <= 32 && !p.starts_with("bot-")) else { return err(400, "want {\"player\": name, \"rtt\": {node: ms}}") };
                let rtt: BTreeMap<u64, u64> = v["rtt"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_u64()?))).collect()).unwrap_or_default();
                let Some((&node, &ms)) = rtt.iter().min_by_key(|(_, &ms)| ms) else { return err(400, "measure the nodes first: rtt") };
                let strategy = v["strategy"].as_str().and_then(bot::Strategy::from_name).unwrap_or(bot::Strategy::ALL[(now % 4) as usize]);
                let bot_name = format!("bot-{}", strategy.name());
                mm.queue.retain(|w| w.player != player);
                mm.matches += 1;
                let id = format!("m{}{}", now, mm.matches);
                let tok = token();
                let r = call(&json!({ "to": "match", "id": id, "node": node, "path": "/create", "body": json!({ "id": id, "players": [player, bot_name], "tokens": [tok, ""], "bot": strategy.name() }).to_string() }));
                if r["status"].as_u64() != Some(200) {
                    return err(503, "couldn't start the match: try again");
                }
                let row = json!({ "match": id, "node": node, "players": [player, bot_name], "worst_rtt": ms, "at": now, "bot": true });
                sql("INSERT INTO recent (row) VALUES (?)", &[json!(row.to_string())]);
                sql("DELETE FROM recent WHERE id <= (SELECT MAX(id) - 20 FROM recent)", &[]);
                sql("INSERT INTO kv (k, v) VALUES ('matches', 1) ON CONFLICT (k) DO UPDATE SET v = v + 1", &[]);
                ok(json!({ "match": id, "node": node, "token": tok, "side": 0, "rtt": ms }))
            }
            // A match is over: no more watching it (there's no replay).
            ("POST", "/ended") => {
                sql("UPDATE recent SET row = json_set(row, '$.over', json('true')) WHERE json_extract(row, '$.match') = ?", &[v["match"].clone()]);
                ok(json!({}))
            }
            ("GET", "/stats") => {
                let matches = sql("SELECT v FROM kv WHERE k = 'matches'", &[]).first().and_then(|r| r[0].as_i64()).unwrap_or(0);
                let recent: Vec<Value> = sql("SELECT row FROM recent ORDER BY id DESC LIMIT 20", &[]).iter().filter_map(|r| serde_json::from_str(r[0].as_str()?).ok()).collect();
                // Live: not over, and not older than a match can last.
                let live = recent.iter().filter(|m| !m["over"].as_bool().unwrap_or(false) && m["at"].as_u64().unwrap_or(0) + match_ms() * 2 > now).count();
                ok(json!({ "queue": mm.queue.len(), "matches": matches, "recent": recent, "live": live, "draining": mm.draining, "version": VERSION }))
            }
            _ => err(404, "not found"),
        };
        answer
    })
}

// ----------------------------------------------------------------- match

#[derive(PartialEq, Clone, Copy, Debug)]
enum Phase {
    /// Waiting for both players to say hello.
    Waiting,
    /// 3, 2, 1: before the first round, each new one, and after a pause.
    Countdown { until: u64 },
    Play,
    /// A player dropped: back by the deadline, or the other wins.
    Paused { who: usize, until: u64 },
    Over,
}

struct Match {
    id: String,
    /// Side 1 is our bot, which the match plays itself.
    bot: Option<bot::Brain>,
    players: [String; 2],
    tokens: [String; 2],
    socks: [Option<u64>; 2],
    game: game::Game,
    phase: Phase,
    created: u64,
    /// Time played, not counting pauses.
    played: u64,
    last_step: u64,
    ticks: u64,
    over_at: u64,
    timing: Timing,
    /// Each player's round trip to this node, as their client measured it.
    ping: [Option<u64>; 2],
    /// Spectators' sockets, and the full frames they'll see once they're
    /// `spectate_delay_ms` old: (when, frame).
    spectators: Vec<u64>,
    frames: std::collections::VecDeque<(u64, String)>,
}

/// The most spectators a match takes.
const MAX_SPECTATORS: usize = 50;

/// How the beat and the players kept up, for the record: the gaps between
/// ticks, and ticks a player sent no new input for (the game then reuses
/// their last one).
#[derive(Default)]
struct Timing {
    played_ticks: u64,
    last_tick: u64,
    max_gap: u64,
    late: u64,
    fresh: [bool; 2],
    stale: [u64; 2],
    inputs: [u64; 2],
}

thread_local! {
    static MATCH: RefCell<Option<Match>> = const { RefCell::new(None) };
}

/// How long a match lasts (`MATCH_MS`, 3 minutes): then more kills wins,
/// and a tie is a draw.
fn match_ms() -> u64 {
    env("MATCH_MS").and_then(|v| v.parse().ok()).unwrap_or(180_000)
}

/// How long a pause may last before the other player wins (`PAUSE_MS`).
fn pause_ms() -> u64 {
    env("PAUSE_MS").and_then(|v| v.parse().ok()).unwrap_or(30_000)
}
const START_WAIT_MS: u64 = 60_000;
/// The 3, 2, 1 before a round.
const COUNTDOWN_MS: u64 = 3000;

/// How far behind live spectators are (`SPECTATE_DELAY_MS`, 10 s).
fn spectate_delay_ms() -> u64 {
    env("SPECTATE_DELAY_MS").and_then(|v| v.parse().ok()).unwrap_or(10_000)
}

/// A side is here: its player's socket is open, or it's our bot.
fn here(m: &Match, side: usize) -> bool {
    m.socks[side].is_some() || (side == 1 && m.bot.is_some())
}

/// What a spectator sees of a tick: everything.
fn frame(m: &Match, now: u64) -> String {
    let mut v = m.game.view_all();
    let (phase, left) = match m.phase {
        Phase::Waiting => ("waiting", 0),
        Phase::Countdown { until } => ("countdown", until.saturating_sub(now)),
        Phase::Play => ("play", 0),
        Phase::Paused { until, .. } => ("paused", until.saturating_sub(now)),
        Phase::Over => ("over", 0),
    };
    v["t"] = json!("sp");
    v["ev"] = json!(m.game.events.iter().map(|(k, w)| json!([k, w])).collect::<Vec<_>>());
    v["phase"] = json!(phase);
    v["left"] = json!(left);
    v["paused_by"] = match m.phase {
        Phase::Paused { who, .. } => json!(who),
        _ => Value::Null,
    };
    v["ping"] = json!(m.ping);
    v["played"] = json!(m.played);
    v["winner"] = match (m.phase, m.game.winner) {
        (Phase::Over, Some(w)) => json!(w),
        (Phase::Over, None) => json!("draw"),
        _ => Value::Null,
    };
    v.to_string()
}

fn state_msg(m: &Match, side: usize, now: u64) -> String {
    let mut v = m.game.view(side);
    let (phase, left) = match m.phase {
        Phase::Waiting => ("waiting", 0),
        Phase::Countdown { until } => ("countdown", until.saturating_sub(now)),
        Phase::Play => ("play", 0),
        Phase::Paused { until, .. } => ("paused", until.saturating_sub(now)),
        Phase::Over => ("over", 0),
    };
    v["t"] = json!("st");
    v["k"] = json!(m.ticks);
    v["phase"] = json!(phase);
    v["left"] = json!(left);
    v["paused_by_you"] = json!(matches!(m.phase, Phase::Paused { who, .. } if who == side));
    v["ping"] = json!([m.ping[side], m.ping[1 - side]]);
    // This tick's events, for sounds: who as you (0) or them (1).
    v["ev"] = json!(m.game.events.iter().map(|(k, w)| json!([k, (*w != side) as u8])).collect::<Vec<_>>());
    if m.phase == Phase::Over {
        let t = &m.timing;
        v["timing"] = json!({ "ticks": t.played_ticks, "max_gap_ms": t.max_gap, "late": t.late, "your_stale": t.stale[side], "their_stale": t.stale[1 - side], "your_inputs": t.inputs[side] });
    }
    v["winner"] = match m.game.winner {
        Some(w) => json!(w == side),
        None => Value::Null,
    };
    v.to_string()
}

/// Tell the matchmaker this match is over (its list stops offering it to
/// watch).
fn ended(m: &Match) {
    send(&json!({ "to": "matchmaker", "id": "global", "path": "/ended", "body": json!({ "match": m.id }).to_string() }));
}

/// The end: a winner, or (`None`) a draw.
fn finish(m: &mut Match, winner: Option<usize>, now: u64) {
    m.game.winner = winner;
    m.phase = Phase::Over;
    m.over_at = now;
    ended(m);
    let who = winner.map_or("draw".to_string(), |w| m.players[w].clone());
    sql("INSERT OR REPLACE INTO meta (k, v) VALUES ('winner', ?)", &[json!(who)]);
    for side in 0..2 {
        let result = match winner {
            None => "draw",
            Some(w) if w == side => "won",
            Some(_) => "lost",
        };
        send(&json!({ "to": "player", "id": m.players[side], "path": "/result", "body": json!({ "result": result }).to_string() }));
    }
}

fn matchbox(method: &str, path: &str, body: &str) -> String {
    let now = now();
    MATCH.with(|cell| {
        let mut cell = cell.borrow_mut();
        if method == "POST" && path == "/create" {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let s = |k: &str, i: usize| v[k][i].as_str().unwrap_or("").to_string();
            sql("CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT)", &[]);
            sql("INSERT OR REPLACE INTO meta (k, v) VALUES ('players', ?)", &[json!(format!("{} vs {}", s("players", 0), s("players", 1)))]);
            if s("players", 0) == s("players", 1) {
                return err(400, "a player can't play themselves");
            }
            let bot = v["bot"].as_str().and_then(bot::Strategy::from_name).map(|st| bot::Brain::new(st, now ^ 0x9e37_79b9, 300, 0.15));
            *cell = Some(Match { id: v["id"].as_str().unwrap_or("").to_string(), bot, players: [s("players", 0), s("players", 1)], tokens: [s("tokens", 0), s("tokens", 1)], socks: [None, None], game: game::Game::new(), phase: Phase::Waiting, created: now, played: 0, last_step: now, ticks: 0, over_at: 0, timing: Timing::default(), ping: [None, None], spectators: vec![], frames: Default::default() });
            if let Some(m) = cell.as_mut().filter(|m| m.bot.is_some()) {
                m.ping[1] = Some(0);
            }
            // Slowly until both players are here: a match nobody joins costs little.
            tick_every(1000);
            return ok(json!({ "ok": true }));
        }
        let Some(m) = cell.as_mut() else {
            // Not in memory: this node restarted, or the match moved. Its
            // state lived in memory only, so it's over.
            return match method {
                "WSOPEN" => err(410, "this match is gone"),
                "TICK" => {
                    tick_every(0);
                    ok(json!({}))
                }
                _ => err(404, "no such match"),
            };
        };
        match method {
            "WSOPEN" => "101 {}".into(),
            "WSMSG" => {
                let sock: u64 = path.trim_start_matches('/').parse().unwrap_or(0);
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                match v["t"].as_str() {
                    Some("hello") => {
                        let Some(side) = (0..2).find(|&i| v["token"].as_str() == Some(m.tokens[i].as_str())) else {
                            ws_close(sock);
                            return ok(json!({}));
                        };
                        m.socks[side] = Some(sock);
                        let hello = json!({ "t": "hello", "side": side, "players": m.players, "map": game::walls() });
                        ws_send(sock, &hello.to_string());
                        match m.phase {
                            Phase::Waiting if (0..2).all(|i| here(m, i)) => {
                                m.phase = Phase::Countdown { until: now + COUNTDOWN_MS };
                                m.last_step = now;
                                tick_every(50);
                            }
                            Phase::Paused { who, .. } if who == side => {
                                m.phase = Phase::Countdown { until: now + COUNTDOWN_MS };
                                m.last_step = now;
                            }
                            _ => {}
                        }
                    }
                    // A spectator: everything, but late (see `frame`).
                    Some("watch") => {
                        if m.spectators.len() >= MAX_SPECTATORS {
                            ws_close(sock);
                            return ok(json!({}));
                        }
                        m.spectators.push(sock);
                        let hello = json!({ "t": "hello", "watch": true, "players": m.players, "map": game::walls(), "delay_ms": spectate_delay_ms() });
                        ws_send(sock, &hello.to_string());
                        // Already over and shown: just how it ended.
                        if m.phase == Phase::Over && m.frames.is_empty() {
                            ws_send(sock, &frame(m, now));
                        }
                    }
                    // Anyone's round trip: answered at once.
                    Some("ping") => ws_send(sock, &json!({ "t": "pong", "c": v["c"] }).to_string()),
                    // A player's round trip, as their client measured it.
                    Some("rtt") => {
                        if let (Some(side), Some(ms)) = ((0..2).find(|&i| m.socks[i] == Some(sock)), v["ms"].as_u64()) {
                            m.ping[side] = Some(ms.min(10_000));
                        }
                    }
                    // A player pauses (and the clock for the other to win
                    // starts), or resumes their own pause.
                    Some(t @ ("pause" | "resume")) => {
                        if let Some(side) = (0..2).find(|&i| m.socks[i] == Some(sock)) {
                            match (t, m.phase) {
                                ("pause", Phase::Play) => {
                                    m.phase = Phase::Paused { who: side, until: now + pause_ms() };
                                    m.game.players[side].input = game::Input::default();
                                }
                                ("resume", Phase::Paused { who, .. }) if who == side && (0..2).all(|i| here(m, i)) => {
                                    m.phase = Phase::Countdown { until: now + COUNTDOWN_MS };
                                    m.last_step = now;
                                }
                                _ => {}
                            }
                        }
                    }
                    Some("in") => {
                        if let Some(side) = (0..2).find(|&i| m.socks[i] == Some(sock)) {
                            let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
                            m.game.players[side].input = game::Input { mx: f("mx"), my: f("my"), aim: f("aim"), fire: v["fire"].as_bool().unwrap_or(false) };
                            m.timing.fresh[side] = true;
                            m.timing.inputs[side] += 1;
                        }
                    }
                    _ => {}
                }
                ok(json!({}))
            }
            "WSCLOSE" => {
                let sock: u64 = path.trim_start_matches('/').parse().unwrap_or(0);
                m.spectators.retain(|&s| s != sock);
                if let Some(side) = (0..2).find(|&i| m.socks[i] == Some(sock)) {
                    m.socks[side] = None;
                    m.game.players[side].input = game::Input::default();
                    if matches!(m.phase, Phase::Play | Phase::Countdown { .. }) {
                        m.phase = Phase::Paused { who: side, until: now + pause_ms() };
                    }
                }
                ok(json!({}))
            }
            "TICK" => {
                // Events are one tick's: none carry over (a countdown steps nothing).
                m.game.events.clear();
                if m.phase != Phase::Play {
                    m.last_step = now;
                    m.timing.last_tick = 0;
                }
                m.ticks += 1;
                match m.phase {
                    Phase::Waiting if now.saturating_sub(m.created) > START_WAIT_MS => {
                        m.phase = Phase::Over;
                        m.over_at = now;
                        ended(m);
                    }
                    Phase::Play => {
                        let t = &mut m.timing;
                        if t.last_tick > 0 {
                            let gap = now.saturating_sub(t.last_tick);
                            t.max_gap = t.max_gap.max(gap);
                            t.late += (gap > 75) as u64;
                        }
                        t.last_tick = now;
                        t.played_ticks += 1;
                        for side in 0..2 {
                            t.stale[side] += !t.fresh[side] as u64;
                            t.fresh[side] = false;
                        }
                        // Our bot: its move, from only what its side is shown.
                        if let Some(b) = m.bot.as_mut() {
                            let v = b.input(&m.game.view(1), 1, now);
                            let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
                            m.game.players[1].input = game::Input { mx: f("mx"), my: f("my"), aim: f("aim"), fire: v["fire"].as_bool().unwrap_or(false) };
                            m.timing.fresh[1] = true;
                        }
                        let dt = (now.saturating_sub(m.last_step) as f64 / 1000.0).clamp(0.0, 0.2);
                        m.played += now.saturating_sub(m.last_step);
                        m.last_step = now;
                        m.game.step(now, dt);
                        if let Some(w) = m.game.winner {
                            finish(m, Some(w), now);
                        } else if m.game.events.iter().any(|e| e.0 == "kill") {
                            // A round won: both back at their spawns, then 3, 2, 1.
                            m.game.new_round();
                            m.phase = Phase::Countdown { until: now + COUNTDOWN_MS };
                        } else if m.played >= match_ms() {
                            let (k0, k1) = (m.game.players[0].kills, m.game.players[1].kills);
                            finish(m, if k0 == k1 { None } else { Some(if k0 > k1 { 0 } else { 1 }) }, now);
                        }
                    }
                    Phase::Paused { who, until } if now >= until => finish(m, Some(1 - who), now),
                    Phase::Countdown { until } if now >= until => {
                        m.phase = Phase::Play;
                        m.last_step = now;
                    }
                    _ => {}
                }
                for side in 0..2 {
                    if let Some(s) = m.socks[side] {
                        ws_send(s, &state_msg(m, side, now));
                    }
                }
                // Spectators: this tick goes in line, and the one now old
                // enough goes out.
                let delay = spectate_delay_ms();
                if m.phase != Phase::Over || now.saturating_sub(m.over_at) <= 3000 {
                    let f = frame(m, now);
                    m.frames.push_back((now, f));
                }
                while m.frames.front().is_some_and(|(t, _)| t + delay <= now) {
                    let (_, f) = m.frames.pop_front().unwrap();
                    for &s in &m.spectators {
                        ws_send(s, &f);
                    }
                }
                // A few seconds after the end, close up the players; once the
                // spectators have seen the end too, them, and stop the beat.
                if m.phase == Phase::Over && now.saturating_sub(m.over_at) > 3000 {
                    for s in m.socks.iter().flatten() {
                        ws_close(*s);
                    }
                    m.socks = [None, None];
                    if m.frames.is_empty() {
                        for &s in &m.spectators {
                            ws_close(s);
                        }
                        m.spectators.clear();
                        tick_every(0);
                    }
                }
                ok(json!({}))
            }
            "GET" if path == "/state" => ok(json!({ "players": m.players, "phase": format!("{:?}", m.phase), "ticks": m.ticks, "score": [m.game.players[0].kills, m.game.players[1].kills] })),
            _ => err(404, "not found"),
        }
    })
}

// ---------------------------------------------------------------- player

fn player(method: &str, path: &str, body: &str) -> String {
    once(|| {
        sql("CREATE TABLE IF NOT EXISTS stats (k TEXT PRIMARY KEY, v INTEGER)", &[]);
    });
    let get = |k: &str| sql("SELECT v FROM stats WHERE k = ?", &[json!(k)]).first().and_then(|r| r[0].as_i64()).unwrap_or(0);
    let bump = |k: &str| sql("INSERT INTO stats (k, v) VALUES (?, 1) ON CONFLICT (k) DO UPDATE SET v = v + 1", &[json!(k)]);
    match (method, path) {
        ("POST", "/result") => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            match v["result"].as_str() {
                Some("won") => bump("wins"),
                Some("lost") => bump("losses"),
                Some("draw") => bump("draws"),
                _ => return err(400, "want {\"result\": won, lost, or draw}"),
            };
            ok(json!({}))
        }
        ("GET", "/") => ok(json!({ "wins": get("wins"), "losses": get("losses"), "draws": get("draws") })),
        _ => err(404, "not found"),
    }
}

// ------------------------------------------------------------ the cell ABI

#[cfg(target_arch = "wasm32")]
mod cell {
    #[no_mangle]
    pub extern "C" fn alloc(len: u32) -> *mut u8 {
        let mut v = Vec::<u8>::with_capacity(len as usize);
        let p = v.as_mut_ptr();
        core::mem::forget(v);
        p
    }

    #[no_mangle]
    pub extern "C" fn release(ptr: *mut u8, len: u32) {
        drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr, len as usize)) });
    }

    /// The answer, as (ptr << 32) | len; the host frees it with `release`.
    #[no_mangle]
    pub extern "C" fn handle(ptr: *mut u8, len: u32) -> u64 {
        let req = unsafe { Vec::from_raw_parts(ptr, len as usize, len as usize) };
        let answer = super::run(core::str::from_utf8(&req).unwrap_or(""));
        let out = Box::into_raw(answer.into_bytes().into_boxed_slice());
        let len = out.len() as u64;
        ((out as *mut u8 as u64) << 32) | len
    }
}
