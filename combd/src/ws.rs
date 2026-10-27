//! WebSocket protocol (RFC 6455): the handshake, and frames. The event loop
//! (`http.rs`) owns the sockets; this is only bytes in, events out.
//!
//! Client frames must be masked. Messages may come in fragments; they're
//! joined, up to `MAX_MESSAGE` (1 MiB, as Cloudflare's). Anything off-spec
//! gets a close code, and the socket closes. No extensions.

/// The biggest message a client may send (joined fragments).
pub const MAX_MESSAGE: usize = 1 << 20;

/// What a client sent.
#[derive(Debug, PartialEq)]
pub enum Event {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    /// The client is closing: its code (1005: none given).
    Close(u16),
}

/// A close code an endpoint may put on the wire (RFC 6455 section 7.4, and
/// IANA's registry since: 1012 to 1014). 1004, 1005, 1006, and 1015 are
/// reserved; 3000 to 4999 are for libraries and apps.
pub fn sendable(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

/// Close codes we send.
pub const PROTOCOL: u16 = 1002;
pub const BAD_DATA: u16 = 1007;
pub const TOO_BIG: u16 = 1009;
/// We closed it: it read too slowly (policy violation).
pub const TOO_SLOW: u16 = 1008;

/// Joins fragments across reads, up to `max` bytes a message.
pub struct Decoder {
    pub max: usize,
    /// A fragmented message so far: (text?, bytes).
    frag: Option<(bool, Vec<u8>)>,
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder::with_max(MAX_MESSAGE)
    }
}

impl Decoder {
    pub fn with_max(max: usize) -> Decoder {
        Decoder { max, frag: None }
    }
}

impl Decoder {
    /// Take the next event off the front of `buf`. `Ok(None)`: need more
    /// bytes. `Err(code)`: close the socket with this code.
    pub fn next(&mut self, buf: &mut Vec<u8>) -> Result<Option<Event>, u16> {
        loop {
            let Some((fin, op, payload, used)) = frame(buf, self.max)? else { return Ok(None) };
            if op >= 8 && (!fin || payload.len() > 125) {
                return Err(PROTOCOL); // control frames are short and whole
            }
            buf.drain(..used);
            match op {
                0x8 => {
                    return match payload.len() {
                        0 => Ok(Some(Event::Close(1005))),
                        1 => Err(PROTOCOL),
                        _ if std::str::from_utf8(&payload[2..]).is_err() => Err(BAD_DATA),
                        _ => match u16::from_be_bytes([payload[0], payload[1]]) {
                            code if sendable(code) => Ok(Some(Event::Close(code))),
                            // 1005 and 1006 mean "no code" and "gone": never on the wire.
                            _ => Err(PROTOCOL),
                        },
                    };
                }
                0x9 => return Ok(Some(Event::Ping(payload))),
                0xA => return Ok(Some(Event::Pong)),
                0x1 | 0x2 => {
                    if self.frag.is_some() {
                        return Err(PROTOCOL); // a new message inside a fragmented one
                    }
                    if fin {
                        return finish(op == 0x1, payload).map(Some);
                    }
                    self.frag = Some((op == 0x1, payload));
                }
                0x0 => {
                    let Some((text, mut msg)) = self.frag.take() else { return Err(PROTOCOL) };
                    if msg.len() + payload.len() > self.max {
                        return Err(TOO_BIG);
                    }
                    msg.extend_from_slice(&payload);
                    if fin {
                        return finish(text, msg).map(Some);
                    }
                    self.frag = Some((text, msg));
                }
                _ => return Err(PROTOCOL),
            }
        }
    }
}

fn finish(text: bool, msg: Vec<u8>) -> Result<Event, u16> {
    if !text {
        return Ok(Event::Binary(msg));
    }
    String::from_utf8(msg).map(Event::Text).map_err(|_| BAD_DATA)
}

/// One whole frame at the front of `buf`: (fin, opcode, unmasked payload,
/// bytes used), or `None` if it hasn't all arrived.
fn frame(buf: &[u8], max: usize) -> Result<Option<(bool, u8, Vec<u8>, usize)>, u16> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    if b0 & 0x70 != 0 {
        return Err(PROTOCOL); // reserved bits: we agreed to no extensions
    }
    if b1 & 0x80 == 0 {
        return Err(PROTOCOL); // a client must mask
    }
    let (len, mut at) = match b1 & 0x7f {
        126 => match buf.get(2..4) {
            Some(b) => (u16::from_be_bytes([b[0], b[1]]) as u64, 4),
            None => return Ok(None),
        },
        127 => match buf.get(2..10) {
            Some(b) => (u64::from_be_bytes(b.try_into().unwrap()), 10),
            None => return Ok(None),
        },
        n => (n as u64, 2),
    };
    if len > max as u64 {
        return Err(TOO_BIG);
    }
    let len = len as usize;
    let Some(mask) = buf.get(at..at + 4) else { return Ok(None) };
    let mask = [mask[0], mask[1], mask[2], mask[3]];
    at += 4;
    let Some(data) = buf.get(at..at + len) else { return Ok(None) };
    let payload = data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect();
    Ok(Some((b0 & 0x80 != 0, b0 & 0x0f, payload, at + len)))
}

/// A server frame (never masked, never fragmented).
pub fn encode(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![0x80 | op];
    match payload.len() {
        n if n < 126 => b.push(n as u8),
        n if n <= 0xffff => {
            b.push(126);
            b.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            b.push(127);
            b.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    b.extend_from_slice(payload);
    b
}

pub fn close(code: u16) -> Vec<u8> {
    encode(0x8, &code.to_be_bytes())
}

pub fn pong(payload: &[u8]) -> Vec<u8> {
    encode(0xA, payload)
}

/// The 101 that accepts a client's handshake.
pub fn accept(key: &str) -> String {
    let digest = sha1(format!("{}258EAFA5-E914-47DA-95CA-C5AB0DC85B11", key.trim()).as_bytes());
    format!("HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {}\r\n\r\n", base64(&digest))
}

/// SHA-1, for the handshake only (it's what RFC 6455 says to use; nothing
/// here relies on it being collision-proof).
fn sha1(msg: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut m = msg.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&((msg.len() as u64) * 8).to_be_bytes());
    for chunk in m.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

pub fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

/// The other way. `None` on anything that isn't base64 we made.
pub fn unbase64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let b = s.as_bytes();
    if b.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for c in b.chunks(4) {
        let pad = c.iter().rev().take_while(|&&x| x == b'=').count();
        let mut n = 0u32;
        for &x in &c[..4 - pad] {
            n = n << 6 | val(x)? as u32;
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A client frame, masked.
    pub fn client(fin: bool, op: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut b = vec![(fin as u8) << 7 | op];
        match payload.len() {
            n if n < 126 => b.push(0x80 | n as u8),
            n if n <= 0xffff => {
                b.push(0x80 | 126);
                b.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                b.push(0x80 | 127);
                b.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        b.extend_from_slice(&mask);
        b.extend(payload.iter().enumerate().map(|(i, x)| x ^ mask[i % 4]));
        b
    }

    #[test]
    fn base64_round_trips() {
        for n in 0..300usize {
            let b: Vec<u8> = (0..n).map(|i| (i * 7 + n) as u8).collect();
            assert_eq!(unbase64(&base64(&b)), Some(b));
        }
        assert_eq!(unbase64("abc"), None);
        assert_eq!(unbase64("ab!d"), None);
    }

    #[test]
    fn the_handshake_matches_the_rfc() {
        assert_eq!(base64(&sha1(b"abc")), "qZk+NkcGgWq6PiVxeFDCbJzQ2J0=");
        // RFC 6455, section 1.3.
        assert!(accept("dGhlIHNhbXBsZSBub25jZQ==").contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
    }

    #[test]
    fn frames_decode_and_join() {
        let mut d = Decoder::default();
        let mut buf = client(true, 1, b"hi");
        buf.extend(client(false, 1, b"hel"));
        buf.extend(client(true, 9, b"p")); // a ping between fragments is fine
        buf.extend(client(true, 0, b"lo"));
        buf.extend(client(true, 2, &[0; 70_000]));
        buf.extend(client(true, 8, &[3, 232, b'b', b'y', b'e']));
        let mut got = vec![];
        while let Some(e) = d.next(&mut buf).unwrap() {
            got.push(e);
        }
        assert_eq!(got, vec![Event::Text("hi".into()), Event::Ping(b"p".to_vec()), Event::Text("hello".into()), Event::Binary(vec![0; 70_000]), Event::Close(1000)]);
        // Codes no endpoint may send are a protocol error (Autobahn 7.9.x).
        for (code, want) in [(0u16, Err(PROTOCOL)), (999, Err(PROTOCOL)), (1004, Err(PROTOCOL)), (1005, Err(PROTOCOL)), (1006, Err(PROTOCOL)), (1015, Err(PROTOCOL)), (1016, Err(PROTOCOL)), (2999, Err(PROTOCOL)), (1001, Ok(Some(Event::Close(1001)))), (1013, Ok(Some(Event::Close(1013)))), (4999, Ok(Some(Event::Close(4999)))), (5000, Err(PROTOCOL))] {
            let mut buf = client(true, 8, &code.to_be_bytes());
            assert_eq!(Decoder::default().next(&mut buf), want, "close {code}");
        }
        assert!(buf.is_empty());
        // Partial frames wait for more bytes.
        let whole = client(true, 1, b"later");
        let mut buf = whole[..3].to_vec();
        assert_eq!(d.next(&mut buf), Ok(None));
        buf.extend_from_slice(&whole[3..]);
        assert_eq!(d.next(&mut buf), Ok(Some(Event::Text("later".into()))));
    }

    #[test]
    fn bad_frames_get_close_codes() {
        let cases: Vec<(Vec<u8>, u16)> = vec![
            ({ let mut f = client(true, 1, b"x"); f[1] &= 0x7f; f }, PROTOCOL), // unmasked
            ({ let mut f = client(true, 1, b"x"); f[0] |= 0x40; f }, PROTOCOL), // reserved bit
            (client(true, 3, b"x"), PROTOCOL), // unknown opcode
            (client(false, 9, b"x"), PROTOCOL), // fragmented ping
            (client(true, 0, b"x"), PROTOCOL), // continuation with nothing to continue
            (client(true, 1, &[0xff, 0xfe]), BAD_DATA), // not UTF-8
            (client(true, 2, &vec![0; MAX_MESSAGE + 1]), TOO_BIG),
            ([client(false, 1, &vec![b'a'; MAX_MESSAGE]), client(true, 0, b"a")].concat(), TOO_BIG),
            ([client(false, 1, b"a"), client(true, 1, b"b")].concat(), PROTOCOL),
        ];
        for (i, (mut buf, code)) in cases.into_iter().enumerate() {
            let mut d = Decoder::default();
            let r = loop {
                match d.next(&mut buf) {
                    Ok(Some(_)) => continue,
                    other => break other,
                }
            };
            assert_eq!(r, Err(code), "case {i}");
        }
    }

    /// Damaged frames: the decoder never panics, and never takes more bytes
    /// than it has.
    #[test]
    fn mangled_frames_never_panic_the_decoder() {
        let mut rng = crate::sim::Rng::new(5);
        for i in 0..20_000u64 {
            let mut b: Vec<u8> = (0..1 + rng.below(4)).flat_map(|_| {
                let n = [0, 1, 125, 126, 300, 70_000][rng.below(6) as usize];
                client(rng.chance(0.7), [0, 1, 2, 8, 9, 10, 3][rng.below(7) as usize], &vec![b'a'; n])
            }).collect();
            for _ in 0..rng.below(4) {
                if b.is_empty() {
                    break;
                }
                let at = rng.below(b.len() as u64) as usize;
                match rng.below(3) {
                    0 => b[at] ^= 1 << rng.below(8),
                    1 => b.truncate(at),
                    _ => b.insert(at, rng.next() as u8),
                }
            }
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut d = Decoder::default();
                let before = b.len();
                while let Ok(Some(_)) = d.next(&mut b) {}
                assert!(b.len() <= before);
            }));
            assert!(r.is_ok(), "case {i}");
        }
    }
}
