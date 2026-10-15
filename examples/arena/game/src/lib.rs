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

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// The page: the game client.
const INDEX: &str = include_str!("../../client/index.html");

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
        ("GET", "/") => format!("200\ncontent-type: text/html; charset=utf-8\ncache-control: no-store\n\n{INDEX}"),
        ("POST", "/api/join") => mm("/join", body),
        ("POST", "/api/poll") => mm("/poll", body),
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
}

#[derive(Default)]
struct Mm {
    queue: Vec<Waiting>,
    ready: HashMap<String, Assigned>,
    recent: VecDeque<Value>,
    matches: u64,
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

fn matchmaker(method: &str, path: &str, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    MM.with(|mm| {
        let mut mm = mm.borrow_mut();
        let now = now();
        mm.queue.retain(|w| now.saturating_sub(w.since) < 60_000);
        match (method, path) {
            ("POST", "/join") => {
                let Some(player) = v["player"].as_str().filter(|p| !p.is_empty() && p.len() <= 32) else { return err(400, "want {\"player\": name, \"rtt\": {node: ms}}") };
                let rtt: BTreeMap<u64, u64> = v["rtt"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_u64()?))).collect()).unwrap_or_default();
                if rtt.is_empty() {
                    return err(400, "measure the nodes first: rtt");
                }
                let ticket = token();
                // The best partner already waiting: the closest, if close enough.
                let partner = mm
                    .queue
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| w.player != player)
                    .filter_map(|(i, w)| best_node(&w.rtt, &rtt).map(|(n, score)| (score, i, n, now.saturating_sub(w.since))))
                    .filter(|(score, _, _, waited)| fair_enough(*score, *waited))
                    .min();
                let Some((score, i, node, _)) = partner else {
                    mm.queue.push(Waiting { ticket: ticket.clone(), player: player.into(), rtt, since: now });
                    return ok(json!({ "ticket": ticket }));
                };
                let other = mm.queue.remove(i);
                mm.matches += 1;
                let id = format!("m{}{}", now, mm.matches);
                let tokens = [token(), token()];
                // Start the match on the node best for both.
                let r = call(&json!({ "to": "match", "id": id, "node": node, "path": "/create", "body": json!({ "players": [other.player, player], "tokens": tokens }).to_string() }));
                if r["status"].as_u64() != Some(200) {
                    mm.queue.push(other);
                    return err(503, &format!("couldn't start a match: {r}"));
                }
                mm.ready.insert(other.ticket.clone(), Assigned { m: id.clone(), node, token: tokens[0].clone(), side: 0, rtt: other.rtt.get(&node).copied() });
                mm.ready.insert(ticket.clone(), Assigned { m: id.clone(), node, token: tokens[1].clone(), side: 1, rtt: rtt.get(&node).copied() });
                mm.recent.push_front(json!({ "match": id, "node": node, "players": [other.player, player], "worst_rtt": score, "at": now }));
                mm.recent.truncate(20);
                ok(json!({ "ticket": ticket }))
            }
            ("POST", "/poll") => {
                let ticket = v["ticket"].as_str().unwrap_or("");
                match mm.ready.remove(ticket) {
                    Some(a) => ok(json!({ "match": a.m, "node": a.node, "token": a.token, "side": a.side, "rtt": a.rtt })),
                    None if mm.queue.iter().any(|w| w.ticket == ticket) => ok(json!({ "waiting": true, "queue": mm.queue.len() })),
                    None => err(404, "no such ticket (it waited too long?): join again"),
                }
            }
            ("GET", "/stats") => ok(json!({ "queue": mm.queue.len(), "matches": mm.matches, "recent": mm.recent })),
            _ => err(404, "not found"),
        }
    })
}

// ----------------------------------------------------------------- match

#[derive(PartialEq, Clone, Copy, Debug)]
enum Phase {
    /// Waiting for both players to say hello.
    Waiting,
    Play,
    /// A player dropped: back by the deadline, or the other wins.
    Paused { who: usize, until: u64 },
    Over,
}

struct Match {
    players: [String; 2],
    tokens: [String; 2],
    socks: [Option<u64>; 2],
    game: game::Game,
    phase: Phase,
    created: u64,
    last_step: u64,
    ticks: u64,
    over_at: u64,
}

thread_local! {
    static MATCH: RefCell<Option<Match>> = const { RefCell::new(None) };
}

/// How long a pause may last before the other player wins (`PAUSE_MS`).
fn pause_ms() -> u64 {
    env("PAUSE_MS").and_then(|v| v.parse().ok()).unwrap_or(30_000)
}
const START_WAIT_MS: u64 = 60_000;

fn state_msg(m: &Match, side: usize, now: u64) -> String {
    let mut v = m.game.view(side);
    let (phase, left) = match m.phase {
        Phase::Waiting => ("waiting", 0),
        Phase::Play => ("play", 0),
        Phase::Paused { until, .. } => ("paused", until.saturating_sub(now)),
        Phase::Over => ("over", 0),
    };
    v["t"] = json!("st");
    v["k"] = json!(m.ticks);
    v["phase"] = json!(phase);
    v["left"] = json!(left);
    v["paused_by_you"] = json!(matches!(m.phase, Phase::Paused { who, .. } if who == side));
    v["winner"] = match m.game.winner {
        Some(w) => json!(w == side),
        None => Value::Null,
    };
    v.to_string()
}

fn finish(m: &mut Match, winner: usize, now: u64) {
    m.game.winner = Some(winner);
    m.phase = Phase::Over;
    m.over_at = now;
    sql("INSERT OR REPLACE INTO meta (k, v) VALUES ('winner', ?)", &[json!(m.players[winner])]);
    for side in 0..2 {
        send(&json!({ "to": "player", "id": m.players[side], "path": "/result", "body": json!({ "won": side == winner }).to_string() }));
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
            *cell = Some(Match { players: [s("players", 0), s("players", 1)], tokens: [s("tokens", 0), s("tokens", 1)], socks: [None, None], game: game::Game::new(), phase: Phase::Waiting, created: now, last_step: now, ticks: 0, over_at: 0 });
            tick_every(50);
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
                            Phase::Waiting if m.socks.iter().all(Option::is_some) => {
                                m.phase = Phase::Play;
                                m.last_step = now;
                            }
                            Phase::Paused { who, .. } if who == side => {
                                m.phase = Phase::Play;
                                m.last_step = now;
                            }
                            _ => {}
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
                                ("resume", Phase::Paused { who, .. }) if who == side && m.socks.iter().all(Option::is_some) => {
                                    m.phase = Phase::Play;
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
                        }
                    }
                    _ => {}
                }
                ok(json!({}))
            }
            "WSCLOSE" => {
                let sock: u64 = path.trim_start_matches('/').parse().unwrap_or(0);
                if let Some(side) = (0..2).find(|&i| m.socks[i] == Some(sock)) {
                    m.socks[side] = None;
                    m.game.players[side].input = game::Input::default();
                    if m.phase == Phase::Play {
                        m.phase = Phase::Paused { who: side, until: now + pause_ms() };
                    }
                }
                ok(json!({}))
            }
            "TICK" => {
                m.ticks += 1;
                match m.phase {
                    Phase::Waiting if now.saturating_sub(m.created) > START_WAIT_MS => {
                        m.phase = Phase::Over;
                        m.over_at = now;
                    }
                    Phase::Play => {
                        let dt = (now.saturating_sub(m.last_step) as f64 / 1000.0).clamp(0.0, 0.2);
                        m.last_step = now;
                        m.game.step(now, dt);
                        if let Some(w) = m.game.winner {
                            finish(m, w, now);
                        }
                    }
                    Phase::Paused { who, until } if now >= until => finish(m, 1 - who, now),
                    _ => {}
                }
                for side in 0..2 {
                    if let Some(s) = m.socks[side] {
                        ws_send(s, &state_msg(m, side, now));
                    }
                }
                // A few seconds after the end, close up and stop the beat.
                if m.phase == Phase::Over && now.saturating_sub(m.over_at) > 3000 {
                    for s in m.socks.iter().flatten() {
                        ws_close(*s);
                    }
                    m.socks = [None, None];
                    tick_every(0);
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
    sql("CREATE TABLE IF NOT EXISTS stats (k TEXT PRIMARY KEY, v INTEGER)", &[]);
    let get = |k: &str| sql("SELECT v FROM stats WHERE k = ?", &[json!(k)]).first().and_then(|r| r[0].as_i64()).unwrap_or(0);
    let bump = |k: &str| sql("INSERT INTO stats (k, v) VALUES (?, 1) ON CONFLICT (k) DO UPDATE SET v = v + 1", &[json!(k)]);
    match (method, path) {
        ("POST", "/result") => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            bump(if v["won"].as_bool() == Some(true) { "wins" } else { "losses" });
            ok(json!({}))
        }
        ("GET", "/") => ok(json!({ "wins": get("wins"), "losses": get("losses") })),
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
