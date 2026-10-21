//! TLS ended here, with buffers we own (rustls's unbuffered API). rustls's
//! own buffering keeps a 4 KB read buffer for every connection, idle or not;
//! here the encrypted bytes in and out live in two `Vec`s that are given back
//! once empty, so an idle connection holds only rustls's state: its keys and
//! where it is in the protocol.
//!
//! `read` takes what the socket has and decrypts it into the plaintext the
//! parser reads; `write` encrypts plaintext (once the handshake allows) and
//! sends what it can. Both go through `pump`, which runs rustls's state
//! machine until it needs more bytes or has nothing more to do.

use rustls::server::UnbufferedServerConnection;
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use std::io::{ErrorKind, Read, Write};
use std::sync::Arc;

/// What an empty buffer keeps: past this it's given back.
const IDLE: usize = 1024;
/// The most encrypted bytes waiting for the socket: past it, `write` stops
/// taking plaintext, so a slow reader's backlog stays with the caller.
const OUT_MAX: usize = 64 << 10;
/// Plaintext encrypted at a time (a TLS record holds 16 KB).
const CHUNK: usize = 16 << 10;

pub struct Tls {
    conn: UnbufferedServerConnection,
    /// Encrypted bytes from the socket, not processed yet.
    incoming: Vec<u8>,
    /// Encrypted bytes for the socket, not sent yet.
    outgoing: Vec<u8>,
    /// The client sent close_notify: it's done sending.
    peer_closed: bool,
}

impl Tls {
    pub fn new(cfg: Arc<rustls::ServerConfig>) -> Result<Tls, rustls::Error> {
        Ok(Tls { conn: UnbufferedServerConnection::new(cfg)?, incoming: Vec::new(), outgoing: Vec::new(), peer_closed: false })
    }

    pub fn is_handshaking(&self) -> bool {
        self.conn.is_handshaking()
    }

    /// Done with the handshake, and nothing half read or unsent: the moment
    /// the session can go to the kernel (`into_kernel`).
    pub fn kernel_ready(&self) -> bool {
        !self.conn.is_handshaking() && self.incoming.is_empty() && self.outgoing.is_empty() && !self.peer_closed
    }

    /// Hand the session to the kernel (Linux kTLS): its keys onto `fd`, and
    /// this connection's TLS state goes. An error leaves `fd` unusable.
    pub fn into_kernel(self, fd: std::os::fd::RawFd) -> std::io::Result<()> {
        let other = |e: rustls::Error| std::io::Error::other(e.to_string());
        let version = match self.conn.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => combd_ktls::Version::Tls13,
            _ => combd_ktls::Version::Tls12,
        };
        // The kernel connection rustls hands back is for what comes after the
        // handshake (key updates, tickets): a client's key update ends the
        // connection instead (the kernels we run can't rekey a socket).
        let (s, _) = self.conn.dangerous_into_kernel_connection().map_err(other)?;
        combd_ktls::enable(fd, version, &kernel_keys(&s.tx)?, &kernel_keys(&s.rx)?)
    }

    /// Encrypted bytes still to go out (the handshake writes before any answer).
    pub fn wants_write(&self) -> bool {
        !self.outgoing.is_empty()
    }

    /// Read what the socket has, and decrypt it onto `plain`. `None`: keep
    /// going. `Some(true)`: the peer is done (gone, closed TLS, or broke it).
    /// `Some(false)`: past `limit`, while it may still be sending.
    pub fn read(&mut self, s: &mut impl Read, plain: &mut Vec<u8>, limit: usize) -> Option<bool> {
        let mut buf = [0u8; 16 * 1024];
        let mut eof = false;
        loop {
            match s.read(&mut buf) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    self.incoming.extend_from_slice(&buf[..n]);
                    // Encrypted bytes pile up only while a record is partial:
                    // a record is at most 16 KB and change.
                    if self.incoming.len() > limit + (32 << 10) {
                        return Some(false);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return Some(true),
            }
        }
        let broken = self.pump(plain, &[], &mut 0).is_err();
        if broken || eof || self.peer_closed {
            Some(true)
        } else {
            (plain.len() > limit).then_some(false)
        }
    }

    /// Encrypt what it can of `wbuf[*wpos..]` (advancing `wpos`), and send
    /// what it can. Anything decrypted on the way lands on `plain`. `true`:
    /// the socket is gone, or TLS broke.
    pub fn write(&mut self, s: &mut impl Write, plain: &mut Vec<u8>, wbuf: &[u8], wpos: &mut usize) -> bool {
        if self.pump(plain, wbuf, wpos).is_err() {
            return true;
        }
        let mut sent = 0;
        let gone = loop {
            if sent == self.outgoing.len() {
                break false;
            }
            match s.write(&self.outgoing[sent..]) {
                Ok(0) => break true,
                Ok(n) => sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break false,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break true,
            }
        };
        self.outgoing.drain(..sent);
        if self.outgoing.is_empty() && self.outgoing.capacity() > IDLE {
            self.outgoing = Vec::new();
        }
        gone
    }

    /// Run the state machine on what's in `incoming` until it wants more bytes
    /// or there's nothing left to do: decrypt into `plain`, answer the
    /// handshake into `outgoing`, and once traffic flows, encrypt
    /// `wbuf[*wpos..]` while `outgoing` has room.
    fn pump(&mut self, plain: &mut Vec<u8>, wbuf: &[u8], wpos: &mut usize) -> Result<(), ()> {
        loop {
            let UnbufferedStatus { mut discard, state } = self.conn.process_tls_records(&mut self.incoming);
            let more = match state.map_err(|_| ())? {
                ConnectionState::ReadTraffic(mut r) => {
                    while let Some(rec) = r.next_record() {
                        let rec = rec.map_err(|_| ())?;
                        discard += rec.discard;
                        plain.extend_from_slice(rec.payload);
                    }
                    true
                }
                ConnectionState::PeerClosed => {
                    self.peer_closed = true;
                    true
                }
                ConnectionState::Closed => {
                    self.peer_closed = true;
                    false
                }
                // Early data is off (rustls's default): never offered, never taken.
                ConnectionState::ReadEarlyData(_) => return Err(()),
                ConnectionState::EncodeTlsData(mut e) => {
                    let at = self.outgoing.len();
                    let mut room = 2048;
                    loop {
                        self.outgoing.resize(at + room, 0);
                        match e.encode(&mut self.outgoing[at..]) {
                            Ok(n) => {
                                self.outgoing.truncate(at + n);
                                break;
                            }
                            Err(EncodeError::InsufficientSize(s)) => room = s.required_size,
                            Err(EncodeError::AlreadyEncoded) => {
                                self.outgoing.truncate(at);
                                break;
                            }
                        }
                    }
                    true
                }
                // Encoded already, in `outgoing`, which goes out in order.
                ConnectionState::TransmitTlsData(t) => {
                    t.done();
                    true
                }
                ConnectionState::BlockedHandshake => false,
                ConnectionState::WriteTraffic(mut w) => {
                    while *wpos < wbuf.len() && self.outgoing.len() < OUT_MAX {
                        let data = &wbuf[*wpos..(*wpos + CHUNK).min(wbuf.len())];
                        let at = self.outgoing.len();
                        let mut room = data.len() + 64;
                        loop {
                            self.outgoing.resize(at + room, 0);
                            match w.encrypt(data, &mut self.outgoing[at..]) {
                                Ok(n) => {
                                    self.outgoing.truncate(at + n);
                                    break;
                                }
                                Err(EncryptError::InsufficientSize(s)) => room = s.required_size,
                                Err(EncryptError::EncryptExhausted) => return Err(()),
                            }
                        }
                        *wpos += data.len();
                    }
                    false
                }
                _ => false,
            };
            self.incoming.drain(..discard);
            if !more {
                break;
            }
        }
        if self.incoming.is_empty() && self.incoming.capacity() > IDLE {
            self.incoming = Vec::new();
        }
        Ok(())
    }
}

/// One direction's keys, as the kernel takes them.
fn kernel_keys((seq, k): &(u64, rustls::ConnectionTrafficSecrets)) -> std::io::Result<combd_ktls::Keys<'_>> {
    let cipher = match k {
        rustls::ConnectionTrafficSecrets::Aes128Gcm { key, iv } => combd_ktls::Cipher::AesGcm128 { key: key.as_ref(), iv: iv.as_ref() },
        rustls::ConnectionTrafficSecrets::Aes256Gcm { key, iv } => combd_ktls::Cipher::AesGcm256 { key: key.as_ref(), iv: iv.as_ref() },
        rustls::ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv } => combd_ktls::Cipher::Chacha20Poly1305 { key: key.as_ref(), iv: iv.as_ref() },
        _ => return Err(std::io::Error::other("a cipher the kernel doesn't do")),
    };
    Ok(combd_ktls::Keys { seq: *seq, cipher })
}
