//! Fuzzing the line from the node's side: the cells' process runs anyone's
//! code, so the node reads what a hostile one could write. `combd fuzz-ipc
//! N [seed]` runs a node whose cells are fake: each loop's line goes here
//! instead, and N messages go back up it, made from what the node sent
//! (its tokens, names, sockets, calls, activations) or made up, right and
//! wrong, while clients ask the node for things. It checks:
//! - the node never panics,
//! - it never stalls: its own check (`/_combd/ping`) answers within `SLOW`
//!   all along,
//! - each loop keeps reading its line,
//! - and last, bytes that don't decode stop only the loop they came on.
//!
//! What a cordon may name (its own tenants' objects, sockets, and
//! handles) is checked once there's more than one (plan.md, step 5).

use crate::app::App;
use crate::cells::Counts;
use crate::engine::Kind;
use crate::host::WsOut;
use crate::http::{self, Origin};
use crate::line::{Asked, CallTo, DbAsk, Down, Up};
use crate::sim::Rng;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The node's own check answers within this, however hostile its cells.
pub const SLOW: Duration = Duration::from_millis(500);

/// What the node has said to its cells, to make messages it will take
/// for real ones.
#[derive(Default)]
struct Seen {
    tokens: Vec<u64>,
    names: Vec<String>,
    socks: Vec<u64>,
    calls: Vec<CallTo>,
    acts: Vec<u64>,
    /// The fleet core's requests (`Origin::Fleet`), which `Ran` answers.
    reqs: Vec<u64>,
    procs: Vec<usize>,
}

pub struct Stats {
    pub sent: u64,
    pub answered: u64,
    pub slowest: Duration,
    pub problems: Vec<String>,
}

static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Read one framed message (its length, then it), as the node sends them.
fn read_frame(s: &mut UnixStream) -> Option<Vec<u8>> {
    let mut n = [0u8; 4];
    s.read_exact(&mut n).ok()?;
    let mut b = vec![0; u32::from_le_bytes(n) as usize];
    s.read_exact(&mut b).ok()?;
    Some(b)
}

fn learn(seen: &Mutex<Seen>, m: &Down) {
    let mut g = seen.lock().unwrap();
    let s = &mut *g;
    let mut name = |n: &String| {
        if s.names.len() < 1000 && !s.names.contains(n) {
            s.names.push(n.clone());
        }
    };
    match m {
        Down::Run { origin, name: n, .. } => {
            name(n);
            match origin {
                Origin::Token(t) => s.tokens.push(*t),
                Origin::Fleet(r) => s.reqs.push(*r),
                _ => {}
            }
        }
        Down::Worker { origin, proc, .. } => {
            s.procs.push(*proc);
            if let Origin::Token(t) = origin {
                s.tokens.push(*t);
            }
        }
        Down::Session { name: n, sock, proc } => {
            name(n);
            s.socks.push(*sock);
            s.procs.push(*proc);
        }
        Down::SockOpen(id) | Down::SockGone(id) => s.socks.push(*id),
        Down::SockLost { name: n, sock } | Down::SockClosed { name: n, sock } => {
            name(n);
            s.socks.push(*sock);
        }
        Down::Back { to, .. } => s.calls.push(*to),
        Down::Seal { name: n, act, .. } | Down::Restore { name: n, act, .. } => {
            name(n);
            s.acts.push(*act);
        }
        Down::Drop { name: n } => name(n),
        _ => {}
    }
    for v in [&mut s.tokens, &mut s.socks, &mut s.acts, &mut s.reqs] {
        if v.len() > 1000 {
            v.drain(..500);
        }
    }
}

fn pick<T: Clone>(r: &mut Rng, known: &[T], made: impl FnOnce(&mut Rng) -> T) -> T {
    if !known.is_empty() && r.chance(0.7) {
        known[r.below(known.len() as u64) as usize].clone()
    } else {
        made(r)
    }
}

fn num(r: &mut Rng) -> u64 {
    match r.below(5) {
        0 => 0,
        1 => u64::MAX,
        2 => r.below(10),
        3 => 1 << r.below(64),
        _ => r.next(),
    }
}

fn bytes(r: &mut Rng) -> Vec<u8> {
    let n = match r.below(10) {
        0 => 0,
        1 => 1 << 20,
        _ => r.below(200) as usize,
    };
    (0..n).map(|_| r.next() as u8).collect()
}

fn text(r: &mut Rng) -> String {
    const SOME: &[&str] = &["", "o", "acme/shop/room/r1", "../../etc/passwd", "room1", "a/b", "\u{0}", "do GET / x", "{\"to\":\"main\",\"path\":\"/\"}", "{\"db\":\"main\",\"sql\":\"select 1\"}", "{\"to\":\"main\",\"ws\":\"x\",\"ids\":[1,2,3]}"];
    match r.below(4) {
        0 => "x".repeat(r.below(5000) as usize),
        1 => String::from_utf8_lossy(&bytes(r)).into_owned(),
        _ => SOME[r.below(SOME.len() as u64) as usize].to_string(),
    }
}

fn reply(r: &mut Rng) -> (u16, String) {
    let status = [200, 101, 0, 503, 999, u16::MAX][r.below(6) as usize];
    (status, text(r))
}

fn call_to(r: &mut Rng) -> CallTo {
    if r.chance(0.5) {
        CallTo::Park(num(r))
    } else {
        CallTo::Ev(num(r) as usize, num(r), num(r))
    }
}

fn ws_out(r: &mut Rng, seen: &Seen) -> Vec<WsOut> {
    (0..r.below(4))
        .map(|_| {
            let id = pick(r, &seen.socks, num);
            match r.below(4) {
                0 => WsOut::Send(id, text(r)),
                1 => WsOut::Binary(id, bytes(r)),
                2 => WsOut::Close(id, r.next() as u16),
                _ => WsOut::Message(text(r).into_bytes()),
            }
        })
        .collect()
}

/// A message the cells could send: each kind, its fields from what the
/// node said or made up.
fn up(r: &mut Rng, seen: &Seen) -> Up {
    let name = |r: &mut Rng| pick(r, &seen.names, text);
    let proc = |r: &mut Rng| pick(r, &seen.procs, |r| num(r) as usize);
    let req = |r: &mut Rng| pick(r, &seen.reqs, num);
    let act = |r: &mut Rng| pick(r, &seen.acts, num);
    let image = |r: &mut Rng| r.chance(0.3).then(|| (text(r), bytes(r)));
    match r.below(18) {
        0..=3 => Up::Answer { token: pick(r, &seen.tokens, num), reply: reply(r) },
        4 => Up::Call { to: r.chance(0.5).then(|| pick(r, &seen.calls, call_to)), from: proc(r), msg: text(r).into_bytes() },
        5 => Up::Ws { name: name(r), out: ws_out(r, seen) },
        6 => {
            let rest = (0..r.below(3)).map(|_| (req(r), reply(r))).collect();
            let wrote = r.chance(0.5).then(|| r.chance(0.5).then(|| num(r) as i64));
            Up::Ran { name: name(r), req: req(r), reply: reply(r), wrote, ws: ws_out(r, seen), rest }
        }
        7 => Up::Skipped { name: name(r), req: req(r) },
        8 => Up::Sealed { name: name(r), act: act(r), changes: if r.chance(0.7) { Ok(bytes(r)) } else { Err(text(r)) }, image: image(r) },
        9 => Up::Restored { name: name(r), act: act(r), alarm: r.chance(0.5).then(|| r.chance(0.5).then(|| num(r) as i64)), image: image(r) },
        10 => {
            let from = match r.below(3) {
                0 => Asked::Tick,
                1 => Asked::Alarm { at: num(r) as i64, tries: num(r) as u32 },
                _ => Asked::Lost,
            };
            Up::Submit { from, name: name(r), req: text(r) }
        }
        11 => Up::Idle((0..r.below(5)).map(|_| name(r)).collect()),
        12 => Up::Anchor { name: name(r), on: r.chance(0.5) },
        13 => Up::Behind(r.chance(0.5)),
        14 => {
            let ask = match r.below(3) {
                0 => DbAsk::Open(text(r).into_bytes()),
                1 => DbAsk::Query(text(r).into_bytes()),
                _ => DbAsk::QueryOn(num(r), text(r).into_bytes()),
            };
            Up::Db { to: pick(r, &seen.calls, call_to), proc: proc(r), ask }
        }
        15 => Up::DbClose(num(r)),
        16 => Up::Stats(Counts { objects_live: num(r), procs: (0..r.below(3)).map(|_| (text(r), num(r), num(r), Kind::Compiled)).collect(), ..Default::default() }),
        _ => Up::Call { to: None, from: proc(r), msg: format!("{{\"to\":{:?},\"path\":\"/\"}}", text(r)).into_bytes() },
    }
}

/// A request on the node, its answer's status, or 0 if none came in time.
fn ask(port: u16, method: &str, path: &str, wait: Duration) -> u16 {
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else { return 0 };
    let _ = s.set_read_timeout(Some(wait));
    let _ = write!(s, "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0)
}

/// `fleet`: a node in a fleet of one, on the bucket `COMBD_S3` names, so
/// what only a fleet's cells say reaches what handles it.
pub fn run(n: u64, seed: u64, wasm: &[u8], fleet: bool) -> Stats {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| {
        PANICS.lock().unwrap().push(format!("{p}"));
        default_hook(p);
    }));
    let mut st = Stats { sent: 0, answered: 0, slowest: Duration::ZERO, problems: vec![] };
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let data = std::env::temp_dir().join(format!("combd-fuzz-ipc-{}-{seed}", std::process::id()));
    let threads = 2;
    let mut cfg = http::Config { data: data.clone(), threads, cells_child: false, lockdown: false, ..http::Config::from_env() };
    if fleet {
        let Some(base) = crate::s3::S3::from_env() else {
            st.problems.push("a fleet needs a bucket: set COMBD_S3, COMBD_S3_KEY, and COMBD_S3_SECRET".into());
            return st;
        };
        let s3 = crate::s3::S3 { bucket: format!("combd-fuzz-ipc-{}-{seed}", std::process::id()), ..base };
        if let Err(e) = s3.make_bucket() {
            st.problems.push(format!("the bucket: {e:?}"));
            return st;
        }
        crate::wire::make_certs(&data.join("certs"), 1).unwrap();
        let wire_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        cfg.fleet = Some(crate::fleet::FleetCfg {
            node: 0,
            lease_ms: 1000,
            replicas: 0,
            s3,
            peers: [(0, format!("127.0.0.1:{wire_port}"))].into(),
            wire_port,
            wire_certs: Some(data.join("certs")),
            insecure: false,
            upload_ms: 0,
            upload_bytes: 1 << 20,
            read_lease: true,
            regions: Default::default(),
            place_ms: 0,
            areas: Default::default(),
        });
    }
    let (mut node_ends, mut ours) = (vec![], vec![]);
    for _ in 0..threads {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        node_ends.push(mio::net::UnixStream::from_std(a));
        ours.push(b);
    }
    // Even seeds: no app yet, as a fleet node before its first deploy.
    let app = if seed % 2 == 0 { App::empty() } else { App::single(wasm, 4) };
    std::thread::spawn(move || http::serve_linked(&http::Live::new(app), port, cfg, node_ends));
    let seen = Arc::new(Mutex::new(Seen::default()));
    for s in &ours {
        let (mut s, seen) = (s.try_clone().unwrap(), seen.clone());
        std::thread::spawn(move || {
            while let Some(b) = read_frame(&mut s) {
                if let Some(m) = Down::decode(&b) {
                    learn(&seen, &m);
                }
            }
        });
    }
    let start = Instant::now();
    while ask(port, "GET", "/_combd/ping", SLOW) != 200 {
        if start.elapsed() > Duration::from_secs(10) {
            st.problems.push("the node never answered".into());
            return st;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Clients: requests the fake cells may or may not answer, and the
    // node's own check, timed.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let slowest = Arc::new(Mutex::new(Duration::ZERO));
    for k in 0..4 {
        let (stop, slowest) = (stop.clone(), slowest.clone());
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                i += 1;
                if k == 0 {
                    let t = Instant::now();
                    if ask(port, "GET", "/_combd/ping", SLOW * 4) != 200 {
                        *slowest.lock().unwrap() = Duration::MAX;
                    }
                    let mut s = slowest.lock().unwrap();
                    *s = (*s).max(t.elapsed());
                    drop(s);
                    std::thread::sleep(Duration::from_millis(20));
                } else {
                    ask(port, "POST", &format!("/do/o{}/incr", i % 7), Duration::from_millis(300));
                }
            }
        });
    }
    let mut r = Rng::new(seed);
    let mut b = vec![];
    for i in 0..n {
        let k = r.below(threads as u64) as usize;
        let m = up(&mut r, &seen.lock().unwrap());
        b.clear();
        m.encode(&mut b);
        let s = &mut ours[k];
        if s.write_all(&(b.len() as u32).to_le_bytes()).and_then(|()| s.write_all(&b)).is_err() {
            let mut m = format!("{m:?}");
            m.truncate(m.floor_char_boundary(300));
            st.problems.push(format!("seed {seed}, message {i}: loop {k} stopped reading its line, after {m}"));
            break;
        }
        st.sent += 1;
        st.answered += matches!(m, Up::Answer { .. }) as u64;
        if i % 64 == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        if !PANICS.lock().unwrap().is_empty() {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    // Last, what doesn't decode: loop 0 stops, and the node still answers
    // its check on the other.
    let junk: Vec<u8> = (0..64).map(|_| r.next() as u8).collect();
    let _ = ours[0].write_all(&(junk.len() as u32).to_le_bytes()).and_then(|()| ours[0].write_all(&junk));
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let up = (0..20).any(|_| ask(port, "GET", "/_combd/ping", SLOW) == 200);
    if !up {
        st.problems.push(format!("seed {seed}: the node stopped answering its check after a loop's junk"));
    }
    st.slowest = *slowest.lock().unwrap();
    if st.slowest > SLOW {
        st.problems.push(format!("seed {seed}: the node's check took {:?}", st.slowest));
    }
    st.problems.extend(PANICS.lock().unwrap().drain(..).map(|p| format!("seed {seed}: panic: {p}")));
    let _ = std::fs::remove_dir_all(&data);
    st
}
