//! `ws-click`: Million's pages, roughly. Each of `n` sockets watches a
//! random 1,000 boxes, renews it every 20 s as the page does, clicks 1 to
//! `max_sets` of them (each flipped from what it sees) a random 1 s to
//! `max_gap` s apart, then sits and watches. It keeps the TLS state (it
//! reads every change), and times each click to its echo: the chunk sends
//! a change to every watcher, the clicker too.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

const CHUNK: u64 = 15_625;
const BOXES: u64 = CHUNK * 64;
const VIEW: u64 = 1000;
const RENEW: Duration = Duration::from_secs(20);
/// A click with no echo by now is counted lost.
const LOST: Duration = Duration::from_secs(10);

struct Page {
    tls: rustls::ClientConnection,
    tcp: mio::net::TcpStream,
    up: bool,
    from: u64,
    /// What it's seen of each chunk it watches.
    bits: HashMap<u64, Vec<u8>>,
    plain: Vec<u8>,
    clicks_left: u32,
    next_click: Instant,
    next_renew: Instant,
    /// Clicks out, by box: when, and what it asked for.
    waiting: HashMap<u64, (Instant, bool)>,
}

/// xorshift: plenty for spreading clicks.
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// A client text frame (masked, with a zero mask).
fn frame(msg: &str) -> Vec<u8> {
    let p = msg.as_bytes();
    let mut f = if p.len() < 126 { vec![0x81, 0x80 | p.len() as u8] } else { vec![0x81, 0x80 | 126, (p.len() >> 8) as u8, p.len() as u8] };
    f.extend_from_slice(&[0, 0, 0, 0]);
    f.extend_from_slice(p);
    f
}

/// The server's next whole frame off `buf` (unmasked, unfragmented): opcode
/// and payload.
fn next_frame(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    let (len, at) = match *buf.get(1)? & 0x7f {
        126 => (u16::from_be_bytes(buf.get(2..4)?.try_into().ok()?) as usize, 4),
        127 => (u64::from_be_bytes(buf.get(2..10)?.try_into().ok()?) as usize, 10),
        n => (n as usize, 2),
    };
    let payload = buf.get(at..at + len)?.to_vec();
    let op = buf[0] & 0x0f;
    buf.drain(..at + len);
    Some((op, payload))
}

#[derive(Default)]
struct Tally {
    clicks: u64,
    echoes: Vec<u32>,
    lost: u64,
    changes: u64,
    slow: u64,
    closed: u64,
}

impl Page {
    fn send(&mut self, msg: &str) {
        let _ = self.tls.writer().write_all(&frame(msg));
        self.flush();
    }

    fn flush(&mut self) {
        while self.up && self.tls.wants_write() {
            match self.tls.write_tls(&mut self.tcp) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => self.up = false,
            }
        }
    }

    fn read(&mut self, t: &mut Tally) {
        loop {
            match self.tls.read_tls(&mut self.tcp) {
                Ok(0) => self.up = false,
                Ok(_) => {
                    if self.tls.process_new_packets().is_err() {
                        self.up = false;
                    }
                    // Drained as it goes: rustls holds only so much.
                    let _ = self.tls.reader().read_to_end(&mut self.plain);
                    continue;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => self.up = false,
            }
            break;
        }
        while let Some((op, p)) = next_frame(&mut self.plain) {
            if op == 0x8 {
                self.up = false;
            }
            if op != 0x1 {
                continue;
            }
            let v: serde_json::Value = serde_json::from_slice(&p).unwrap_or_default();
            let n = v["n"].as_u64().unwrap_or(0);
            match v["t"].as_str() {
                Some("all") => {
                    let hex = v["bits"].as_str().unwrap_or("").as_bytes();
                    let bits = hex.chunks(2).map(|h| u8::from_str_radix(std::str::from_utf8(h).unwrap_or("0"), 16).unwrap_or(0)).collect();
                    self.bits.insert(n, bits);
                }
                Some("set") => {
                    t.changes += 1;
                    let (i, on) = (v["i"].as_u64().unwrap_or(0), v["v"].as_bool().unwrap_or(false));
                    if let Some(b) = self.bits.get_mut(&n).and_then(|b| b.get_mut(i as usize / 8)) {
                        *b = if on { *b | 1 << (i % 8) } else { *b & !(1 << (i % 8)) };
                    }
                    let at = n * CHUNK + i;
                    if self.waiting.get(&at).is_some_and(|w| w.1 == on) {
                        t.echoes.push(self.waiting.remove(&at).unwrap().0.elapsed().as_millis() as u32);
                    }
                }
                Some("slow") => t.slow += 1,
                _ => {}
            }
        }
    }

    fn range(&mut self) {
        self.send(&format!(r#"{{"t":"range","from":{},"to":{}}}"#, self.from, self.from + VIEW));
        self.next_renew = Instant::now() + RENEW;
    }

    fn click(&mut self, rng: &mut Rng, max_gap: u64, t: &mut Tally) {
        let at = self.from + rng.below(VIEW);
        let (n, i) = (at / CHUNK, at % CHUNK);
        let Some(on) = self.bits.get(&n).and_then(|b| b.get(i as usize / 8)).map(|b| b & 1 << (i % 8) == 0) else {
            self.next_click = Instant::now() + Duration::from_secs(1); // not shown yet
            return;
        };
        self.send(&format!(r#"{{"t":"set","i":{at},"v":{on}}}"#));
        self.waiting.insert(at, (Instant::now(), on));
        t.clicks += 1;
        self.clicks_left -= 1;
        self.next_click = Instant::now() + Duration::from_millis(1000 + rng.below(max_gap.saturating_sub(1) * 1000 + 1));
    }
}

fn pct(sorted: &[u32], p: usize) -> u32 {
    sorted.get((sorted.len() * p / 100).min(sorted.len().saturating_sub(1))).copied().unwrap_or(0)
}

pub fn run(addr: &str, host: &str, path: &str, n: usize, ca: &str, max_sets: u64, max_gap: u64) {
    let cfg = crate::wire::admin_client(std::path::Path::new(ca)).unwrap();
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
    let mut rng = Rng(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 | 1);
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);
    let mut pages = Vec::with_capacity(n);
    let t0 = Instant::now();
    for i in 0..n {
        let tcp = std::net::TcpStream::connect(addr).unwrap_or_else(|e| panic!("socket {i}: {e}"));
        let mut s = rustls::StreamOwned::new(rustls::ClientConnection::new(cfg.clone(), name.clone()).unwrap(), tcp);
        write!(s, "GET {path} HTTP/1.1\r\nhost: {host}\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n").unwrap();
        let mut head = vec![];
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut b).unwrap_or(0) == 0 {
                break;
            }
            head.push(b[0]);
        }
        if !head.starts_with(b"HTTP/1.1 101") {
            println!("ws-click: socket {i} refused: {}", String::from_utf8_lossy(&head).lines().next().unwrap_or("closed"));
            break;
        }
        let (tls, tcp) = s.into_parts();
        tcp.set_nonblocking(true).unwrap();
        let mut tcp = mio::net::TcpStream::from_std(tcp);
        poll.registry().register(&mut tcp, mio::Token(i), mio::Interest::READABLE).unwrap();
        let mut p = Page {
            tls,
            tcp,
            up: true,
            from: rng.below(BOXES - VIEW),
            bits: HashMap::new(),
            plain: vec![],
            clicks_left: 1 + rng.below(max_sets) as u32,
            next_click: Instant::now(),
            next_renew: Instant::now(),
            waiting: HashMap::new(),
        };
        p.range();
        pages.push(p);
        if (i + 1) % (n / 10).max(1) == 0 {
            println!("ws-click: {} open ({:.0} s)", i + 1, t0.elapsed().as_secs_f64());
        }
    }
    // Everyone starts clicking somewhere in the first `max_gap` seconds.
    let start = Instant::now();
    for p in &mut pages {
        p.next_click = start + Duration::from_millis(rng.below(max_gap * 1000));
    }
    let mut t = Tally::default();
    let mut report = start + Duration::from_secs(10);
    loop {
        poll.poll(&mut events, Some(Duration::from_millis(20))).unwrap();
        for e in &events {
            let p = &mut pages[e.token().0];
            if p.up {
                p.read(&mut t);
                if !p.up {
                    t.closed += 1;
                }
            }
        }
        let now = Instant::now();
        for p in pages.iter_mut().filter(|p| p.up) {
            if p.clicks_left > 0 && now >= p.next_click {
                p.click(&mut rng, max_gap, &mut t);
            }
            if now >= p.next_renew {
                p.range();
            }
            p.waiting.retain(|_, w| {
                let keep = now - w.0 < LOST;
                t.lost += !keep as u64;
                keep
            });
            p.flush();
        }
        if now >= report {
            report += Duration::from_secs(10);
            t.echoes.sort_unstable();
            let left: u64 = pages.iter().filter(|p| p.up).map(|p| p.clicks_left as u64).sum();
            println!(
                "ws-click +{}s: {} up, {} closed | clicks {} ({} to go), echo p50 {} ms p99 {} ms max {} ms, lost {} | changes seen {} | slowed {}",
                start.elapsed().as_secs(),
                pages.iter().filter(|p| p.up).count(),
                t.closed,
                t.clicks,
                left,
                pct(&t.echoes, 50),
                pct(&t.echoes, 99),
                t.echoes.last().copied().unwrap_or(0),
                t.lost,
                t.changes,
                t.slow
            );
            // Each line is its own 10 s.
            t = Tally { closed: t.closed, ..Tally::default() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_frames_come_off_whole() {
        let mut buf = crate::ws::encode(0x1, b"hi");
        buf.extend(crate::ws::encode(0x1, &[b'x'; 300]));
        buf.extend(&crate::ws::encode(0x1, b"partial")[..4]);
        assert_eq!(next_frame(&mut buf), Some((0x1, b"hi".to_vec())));
        assert_eq!(next_frame(&mut buf).map(|f| f.1.len()), Some(300));
        assert_eq!(next_frame(&mut buf), None);
        assert_eq!(buf.len(), 4);
    }
}
