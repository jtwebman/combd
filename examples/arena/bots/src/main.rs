//! Arena bots: players that join, get matched, and play, over the same HTTP
//! and WebSockets a browser uses. Each has a strategy:
//! - `chaser` heads for wherever the other player was last seen, firing on sight;
//! - `sniper` keeps its distance, strafes, and fires on sight;
//! - `camper` waits in cover by a wall and peeks out now and then;
//! - `random` wanders and fires at random.
//!
//! They see only what the server sends them (no peeking through walls), and
//! steer around walls with the game's own collision check.
//!
//! ```sh
//! cargo run --release -- http://localhost:8080 8 mix     # 8 bots, all four kinds
//! cargo run --release -- https://arena.combd.dev 4 mix   # over HTTPS and WSS
//! ARENA_VS_BOT=1 cargo run --release -- https://arena.combd.dev 1 chaser  # vs the match's own bot
//! cargo run --release -- http://localhost:8080 2 chaser 60   # for 60 s
//! ```
//! A bot plays one match after another, so a human who joins meets one.
//!
//! They play like people, not machines: a bot fires only once it has seen
//! the other player for a reaction time (`ARENA_BOT_REACT_MS`, 300), aims
//! where it saw them that long ago, and misses by up to `ARENA_BOT_AIM`
//! radians (0.15, about 9 degrees) either way.
//!
//! `ARENA_SPREAD=40` pretends the bots live around the world: bot i's home is
//! node i % nodes, and every node away from home adds 40 ms to what it
//! measured, so matches land on different nodes even when every bot runs on
//! one laptop.

use arena_game::bot::{Brain, Rng, Strategy};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn hostport(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, r)| r).trim_end_matches('/')
}

/// A connection to a node: plain TCP, or TLS for `https://` and `wss://`.
struct Conn {
    sock: TcpStream,
    tls: Option<rustls::ClientConnection>,
}

impl Conn {
    fn open(url: &str) -> Result<Conn, String> {
        let hp = hostport(url);
        let tls = url.starts_with("https://") || url.starts_with("wss://");
        let addr = if hp.contains(':') { hp.to_string() } else { format!("{hp}:{}", if tls { 443 } else { 80 }) };
        let sock = TcpStream::connect(&addr).map_err(|e| format!("{addr}: {e}"))?;
        sock.set_nodelay(true).ok();
        let tls = match tls {
            false => None,
            true => {
                let name = rustls::pki_types::ServerName::try_from(hp.split(':').next().unwrap_or_default().to_string()).map_err(|e| e.to_string())?;
                Some(rustls::ClientConnection::new(tls_config(), name).map_err(|e| e.to_string())?)
            }
        };
        Ok(Conn { sock, tls })
    }
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.tls {
            Some(c) => rustls::Stream::new(c, &mut self.sock).read(b),
            None => self.sock.read(b),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match &mut self.tls {
            Some(c) => rustls::Stream::new(c, &mut self.sock).write(b),
            None => self.sock.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.tls {
            Some(c) => rustls::Stream::new(c, &mut self.sock).flush(),
            None => self.sock.flush(),
        }
    }
}

/// Mozilla's root certificates, made once.
fn tls_config() -> std::sync::Arc<rustls::ClientConfig> {
    static C: std::sync::OnceLock<std::sync::Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let c = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions().expect("TLS versions").with_root_certificates(roots).with_no_client_auth();
        std::sync::Arc::new(c)
    })
    .clone()
}

/// One HTTP request: (status, body).
fn http(url: &str, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    let hp = hostport(url);
    let mut s = Conn::open(url)?;
    s.sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(s, "{method} {path} HTTP/1.1\r\nhost: {hp}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}", body.len()).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())?;
    let mut out = vec![];
    // To the end: a TLS server that closes without saying so is the end too.
    if let Err(e) = s.read_to_end(&mut out) {
        if e.kind() != std::io::ErrorKind::UnexpectedEof {
            return Err(e.to_string());
        }
    }
    let out = String::from_utf8_lossy(&out).into_owned();
    let status = out.get(9..12).and_then(|c| c.parse().ok()).ok_or("no answer")?;
    Ok((status, out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_string()))
}

fn json_of(r: Result<(u16, String), String>) -> Value {
    r.ok().and_then(|(_, b)| serde_json::from_str(&b).ok()).unwrap_or(Value::Null)
}

/// A WebSocket client, enough for the arena: text frames, masked, as a
/// client must.
struct Ws(Conn);

impl Ws {
    fn open(url: &str, path: &str) -> Result<Ws, String> {
        let hp = hostport(url);
        let mut s = Conn::open(url)?;
        write!(s, "GET {path} HTTP/1.1\r\nhost: {hp}\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n").map_err(|e| e.to_string())?;
        s.flush().map_err(|e| e.to_string())?;
        let mut head = vec![];
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut b).map_err(|e| e.to_string())? == 0 {
                return Err("closed during the handshake".into());
            }
            head.push(b[0]);
        }
        if !head.starts_with(b"HTTP/1.1 101") {
            return Err(String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_string());
        }
        Ok(Ws(s))
    }

    fn send(&mut self, text: &str) -> std::io::Result<()> {
        let p = text.as_bytes();
        let mut f = vec![0x81];
        match p.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n < 65536 => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = [0x12, 0x34, 0x56, 0x78];
        f.extend_from_slice(&mask);
        f.extend(p.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.0.write_all(&f)?;
        self.0.flush()
    }

    /// The next text message, if one comes within `ms`. `Err`: closed.
    fn recv(&mut self, ms: u64) -> Result<Option<String>, ()> {
        self.0.sock.set_read_timeout(Some(Duration::from_millis(ms.max(1)))).ok();
        let mut h = [0u8; 2];
        match self.0.read_exact(&mut h) {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Ok(None),
            Err(_) => return Err(()),
        }
        self.0.sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut len = (h[1] & 0x7f) as usize;
        if len == 126 {
            let mut b = [0u8; 2];
            self.0.read_exact(&mut b).map_err(|_| ())?;
            len = u16::from_be_bytes(b) as usize;
        } else if len == 127 {
            let mut b = [0u8; 8];
            self.0.read_exact(&mut b).map_err(|_| ())?;
            len = u64::from_be_bytes(b) as usize;
        }
        let mut p = vec![0u8; len];
        self.0.read_exact(&mut p).map_err(|_| ())?;
        match h[0] & 0x0f {
            8 => Err(()),
            1 => Ok(Some(String::from_utf8_lossy(&p).into_owned())),
            _ => Ok(None),
        }
    }
}

/// Our round trip to each node: the best of three pings.
fn measure(url: &str) -> (Value, Value) {
    let nodes = json_of(http(url, "GET", "/_combd/nodes", ""));
    let mut list: Vec<(String, String)> = nodes["nodes"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default();
    if list.is_empty() {
        list.push(("0".into(), url.to_string()));
    }
    let mut rtt = serde_json::Map::new();
    let mut urls = serde_json::Map::new();
    for (id, u) in list {
        let best = (0..3).filter_map(|_| {
            let t = Instant::now();
            http(&u, "GET", "/_combd/ping", "").ok().map(|_| t.elapsed().as_millis() as u64)
        }).min();
        if let Some(ms) = best {
            rtt.insert(id.clone(), json!(ms.max(1)));
        }
        urls.insert(id, json!(u));
    }
    (Value::Object(rtt), Value::Object(urls))
}

/// Pretend distance (see `ARENA_SPREAD`): `home`'s own node adds nothing,
/// each node further along adds `ms`.
fn spread(rtt: &mut Value, home: usize, ms: u64) {
    let Some(o) = rtt.as_object_mut() else { return };
    let n = o.len().max(1);
    for (k, v) in o.iter_mut() {
        let id: usize = k.parse().unwrap_or(0);
        let away = (id + n - home % n) % n;
        *v = json!(v.as_u64().unwrap_or(0) + away as u64 * ms);
    }
}

/// How a match kept time, from the server's own count.
#[derive(Default, Clone, Copy)]
struct Timing {
    ticks: u64,
    max_gap: u64,
    late: u64,
    stale: u64,
    /// Round trips to the match's node, measured in play: total and count.
    ping_ms: u64,
    pings: u64,
}

/// One match: join, wait, play. Ok(Some(true)): won; Ok(None): a draw.
fn play(url: &str, name: &str, s: Strategy, rng: &mut Rng, home: usize, timing: &mut Timing) -> Result<Option<bool>, String> {
    let (mut rtt, urls) = measure(url);
    if let Some(ms) = std::env::var("ARENA_SPREAD").ok().and_then(|v| v.parse().ok()) {
        spread(&mut rtt, home, ms);
    }
    // `ARENA_VS_BOT=1`: play the match's own bot (the page's "Play a bot").
    let vs_bot = std::env::var("ARENA_VS_BOT").is_ok_and(|v| v == "1");
    // Names starting "bot-" are our bots' own: against one, play as a probe.
    let name = if vs_bot { name.replacen("bot-", "probe-", 1) } else { name.to_string() };
    let j = json_of(http(url, "POST", if vs_bot { "/api/bot" } else { "/api/join" }, &json!({ "player": name, "rtt": rtt }).to_string()));
    let ticket = j["ticket"].as_str().unwrap_or_default().to_string();
    if !vs_bot && ticket.is_empty() {
        return Err(format!("join: {j}"));
    }
    let m = if vs_bot { j.get("match").map(|_| j.clone()).ok_or(format!("bot: {j}"))? } else { loop {
        std::thread::sleep(Duration::from_millis(300));
        let p = json_of(http(url, "POST", "/api/poll", &json!({ "ticket": ticket }).to_string()));
        if p["match"].is_string() {
            break p;
        }
        if p["error"].is_string() {
            // Nobody to play for a minute: the queue let us go. Join again.
            return Ok(Some(false)).and(Err("lonely".into()));
        }
    } };
    let node_url = urls[m["node"].to_string().trim_matches('"')].as_str().unwrap_or(url).to_string();
    let path = format!("/match/{}/ws", m["match"].as_str().unwrap());
    let mut ws = Ws::open(&node_url, &path)?;
    ws.send(&json!({ "t": "hello", "token": m["token"] }).to_string()).map_err(|e| e.to_string())?;
    let side = m["side"].as_u64().unwrap_or(0) as usize;
    let start = Instant::now();
    let mut last_ping: Option<Instant> = None;
    let env_num = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let mut brain = Brain::new(s, rng.next(), env_num("ARENA_BOT_REACT_MS", 300.0) as u64, env_num("ARENA_BOT_AIM", 0.15));
    loop {
        // Once a second: a round trip to the match's node, and then tell it
        // (the other player and spectators see it).
        if last_ping.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            last_ping = Some(Instant::now());
            ws.send(&json!({ "t": "ping", "c": start.elapsed().as_micros() as u64 }).to_string()).map_err(|e| e.to_string())?;
        }
        match ws.recv(10) {
            Ok(Some(text)) => {
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if v["t"] == "pong" {
                    if let Some(c) = v["c"].as_u64() {
                        let ms = (start.elapsed().as_micros() as u64).saturating_sub(c) / 1000;
                        timing.ping_ms += ms;
                        timing.pings += 1;
                        ws.send(&json!({ "t": "rtt", "ms": ms }).to_string()).map_err(|e| e.to_string())?;
                    }
                }
                if v["t"] == "st" {
                    if v["phase"] == "over" {
                        let t = &v["timing"];
                        let n = |k: &str| t[k].as_u64().unwrap_or(0);
                        timing.ticks += n("ticks");
                        timing.max_gap = timing.max_gap.max(n("max_gap_ms"));
                        timing.late += n("late");
                        timing.stale += n("your_stale");
                        return Ok(v["winner"].as_bool());
                    }
                    // An answer to every state, as a player at the keys would
                    // (states come 20 times a second).
                    let input = brain.input(&v, side, start.elapsed().as_millis() as u64);
                    if v["phase"] == "play" {
                        ws.send(&input.to_string()).map_err(|e| e.to_string())?;
                    }
                }
            }
            Ok(None) => {}
            Err(()) => return Err("the match closed early".into()),
        }
        if start.elapsed() > Duration::from_secs(600) {
            return Err("a match over 10 minutes: giving up".into());
        }
    }
}

/// Spectate a match from here: how many frames come, and how late.
/// `arena-bots watch <node url> <match id> [seconds]`.
fn watch(node_url: &str, id: &str, secs: u64) {
    let mut ws = Ws::open(node_url, &format!("/match/{id}/ws")).unwrap_or_else(|e| panic!("{e}"));
    ws.send(&json!({ "t": "watch" }).to_string()).unwrap();
    let (t, mut first, mut frames, mut last) = (Instant::now(), None, 0u64, Value::Null);
    while t.elapsed() < Duration::from_secs(secs) {
        match ws.recv(100) {
            Ok(Some(text)) => {
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                match v["t"].as_str() {
                    Some("hello") => println!("hello: {} vs {}, {} ms behind live", v["players"][0], v["players"][1], v["delay_ms"]),
                    Some("sp") => {
                        first.get_or_insert(t.elapsed());
                        frames += 1;
                        last = v;
                    }
                    _ => {}
                }
            }
            Ok(None) => {}
            Err(()) => break,
        }
    }
    println!("{frames} frames; the first after {:?}; last: phase {}, kills {}-{}, pings {}", first.unwrap_or_default(), last["phase"], last["p"][0]["kills"], last["p"][1]["kills"], last["ping"]);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("watch") {
        return watch(&args[2], &args[3], args.get(4).and_then(|v| v.parse().ok()).unwrap_or(30));
    }
    let url = args.get(1).cloned().unwrap_or("http://localhost:8080".into());
    let n: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(4);
    let kind = args.get(3).cloned().unwrap_or("mix".into());
    let secs: u64 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(u64::MAX / 2);
    let all = [Strategy::Chaser, Strategy::Sniper, Strategy::Camper, Strategy::Random];
    let deadline = Instant::now() + Duration::from_secs(secs.min(10_000_000));
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let s = match kind.as_str() {
                "chaser" => Strategy::Chaser,
                "sniper" => Strategy::Sniper,
                "camper" => Strategy::Camper,
                "random" => Strategy::Random,
                _ => all[i % all.len()],
            };
            let url = url.clone();
            std::thread::spawn(move || {
                let name = format!("bot-{}-{i}", format!("{s:?}").to_lowercase());
                let mut rng = Rng(i as u64 * 7919 + 1);
                let (mut won, mut lost, mut drawn, mut failed) = (0, 0, 0, 0);
                let mut timing = Timing::default();
                while Instant::now() < deadline {
                    match play(&url, &name, s, &mut rng, i, &mut timing) {
                        Ok(Some(true)) => won += 1,
                        Ok(Some(false)) => lost += 1,
                        Ok(None) => drawn += 1,
                        Err(e) if e == "lonely" => {}
                        Err(e) => {
                            failed += 1;
                            eprintln!("{name}: {e}");
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
                (name, won, lost, drawn, failed, timing)
            })
        })
        .collect();
    let mut all = Timing::default();
    for h in handles {
        let (name, w, l, d, f, t) = h.join().unwrap();
        let pct = |x: u64| if t.ticks == 0 { 0.0 } else { 100.0 * x as f64 / t.ticks as f64 };
        println!("{name}: {w} won, {l} lost, {d} drawn, {f} failed | {} ticks, {:.1}% late, worst gap {} ms, {:.1}% without my input, ping {} ms", t.ticks, pct(t.late), t.max_gap, pct(t.stale), t.ping_ms.checked_div(t.pings).unwrap_or(0));
        all.ticks += t.ticks;
        all.late += t.late;
        all.stale += t.stale;
        all.max_gap = all.max_gap.max(t.max_gap);
    }
    let pct = |x: u64| if all.ticks == 0 { 0.0 } else { 100.0 * x as f64 / all.ticks as f64 };
    println!("all: {} ticks seen by players, {:.2}% late (over 75 ms apart), worst gap {} ms, {:.2}% of a player's ticks had no new input from them", all.ticks, pct(all.late), all.max_gap, pct(all.stale));
}
