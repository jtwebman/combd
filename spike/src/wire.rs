//! The wire between fleet nodes: one hub thread runs every peer connection on
//! one `mio` poll, with mutual TLS.
//!
//! Each node has a certificate for `node<N>.combd`, signed by the fleet's CA
//! (`make_certs`). A node accepts a connection only from a client with a
//! certificate from that CA, and checks the server it dials against the exact
//! name `node<N>.combd`, so no node can pass itself off as another.
//! Without certificates, a node refuses to join a fleet unless
//! `COMBD_WIRE_INSECURE=1` asks for plain TCP.
//!
//! Frames (`fleet::Frame`) go in both directions on a connection: forwards
//! and peer calls out, replies back. If a connection breaks, every request
//! waiting on it gets a 503 at once, and every peer call hears "lost".

use crate::fleet::{Frame, MAX_FRAME};
use crate::own::{Node, PeerCall, PeerRes};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

const LISTENER: Token = Token(usize::MAX);
const WAKE: Token = Token(usize::MAX - 1);

pub struct Tls {
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn server_name(n: Node) -> String {
    format!("node{n}.combd")
}

/// This node's TLS setup, from `dir`: `ca.pem`, `node<N>.pem`, `node<N>.key`.
pub fn load_tls(dir: &Path, node: Node) -> Result<Tls, String> {
    let e = |what: &str, x: &dyn std::fmt::Display| format!("{what}: {x}");
    let ca: Vec<CertificateDer> = CertificateDer::pem_file_iter(dir.join("ca.pem")).map_err(|x| e("ca.pem", &x))?.collect::<Result<_, _>>().map_err(|x| e("ca.pem", &x))?;
    let mut roots = rustls::RootCertStore::empty();
    for c in ca {
        roots.add(c).map_err(|x| e("ca.pem", &x))?;
    }
    let roots = Arc::new(roots);
    let cert_file = dir.join(format!("node{node}.pem"));
    let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(&cert_file).map_err(|x| e("node cert", &x))?.collect::<Result<_, _>>().map_err(|x| e("node cert", &x))?;
    let key = || PrivateKeyDer::from_pem_file(dir.join(format!("node{node}.key"))).map_err(|x| e("node key", &x));
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(roots.clone(), provider()).build().map_err(|x| e("client verifier", &x))?;
    let server = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|x| e("tls", &x))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs.clone(), key()?)
        .map_err(|x| e("server cert", &x))?;
    let client = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|x| e("tls", &x))?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key()?)
        .map_err(|x| e("client cert", &x))?;
    Ok(Tls { server: Arc::new(server), client: Arc::new(client) })
}

/// The admin API's TLS: this node's certificate, and no client
/// certificate (clients show a token instead).
pub fn admin_tls(dir: &Path, node: Node) -> Result<Arc<rustls::ServerConfig>, String> {
    let e = |what: &str, x: &dyn std::fmt::Display| format!("{what}: {x}");
    let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(dir.join(format!("node{node}.pem"))).map_err(|x| e("node cert", &x))?.collect::<Result<_, _>>().map_err(|x| e("node cert", &x))?;
    let key = PrivateKeyDer::from_pem_file(dir.join(format!("node{node}.key"))).map_err(|x| e("node key", &x))?;
    let c = rustls::ServerConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().map_err(|x| e("tls", &x))?.with_no_client_auth().with_single_cert(certs, key).map_err(|x| e("server cert", &x))?;
    Ok(Arc::new(c))
}

/// A client for the admin API that trusts the fleet CA in `ca`.
pub fn admin_client(ca: &Path) -> Result<Arc<rustls::ClientConfig>, String> {
    let e = |x: &dyn std::fmt::Display| format!("{}: {x}", ca.display());
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(ca).map_err(|x| e(&x))? {
        roots.add(c.map_err(|x| e(&x))?).map_err(|x| e(&x))?;
    }
    let c = rustls::ClientConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().map_err(|x| e(&x))?.with_root_certificates(roots).with_no_client_auth();
    Ok(Arc::new(c))
}

/// A fleet CA and a certificate for each of `nodes` nodes, into `dir`.
pub fn make_certs(dir: &Path, nodes: u32) -> Result<(), String> {
    let e = |x: rcgen::Error| x.to_string();
    std::fs::create_dir_all(dir).map_err(|x| x.to_string())?;
    let ca_key = rcgen::KeyPair::generate().map_err(e)?;
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(e)?;
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(rcgen::DnType::CommonName, "combd fleet CA");
    let ca_cert = ca.self_signed(&ca_key).map_err(e)?;
    let write = |name: String, body: String| std::fs::write(dir.join(name), body).map_err(|x| x.to_string());
    write("ca.pem".into(), ca_cert.pem())?;
    write("ca.key".into(), ca_key.serialize_pem())?;
    for n in 0..nodes {
        let key = rcgen::KeyPair::generate().map_err(e)?;
        let mut p = rcgen::CertificateParams::new(vec![server_name(n)]).map_err(e)?;
        p.distinguished_name.push(rcgen::DnType::CommonName, server_name(n));
        let cert = p.signed_by(&key, &ca_cert, &ca_key).map_err(e)?;
        write(format!("node{n}.pem"), cert.pem())?;
        write(format!("node{n}.key"), key.serialize_pem())?;
    }
    Ok(())
}

/// Where the hub hands what arrives.
pub trait Sink: Send + 'static {
    /// A peer forwarded us a request; answer with `Wire::reply(conn, ..)`.
    fn forward(&self, conn: usize, id: u64, name: String, req: String);
    /// The answer to a request loop `back` forwarded (a 503 if the link broke).
    fn reply(&self, back: usize, id: u64, reply: (u16, String));
    /// A peer called our peer store; answer with `Wire::reply(conn, ..)`.
    fn peer(&self, conn: usize, id: u64, call: PeerCall);
    /// The answer to a peer call loop `back` made (`None`: lost).
    fn peer_reply(&self, back: usize, id: u64, res: Option<PeerRes>);
    /// A node asks us to send on one of our WebSockets.
    fn ws_push(&self, name: String, sock: u64, op: u8, payload: Vec<u8>);
    /// A push of ours couldn't reach its node: the socket is as good as gone.
    fn ws_lost(&self, name: String, sock: u64);
}

enum Cmd {
    Send { node: Node, frame: Frame, back: usize },
    Reply { conn: usize, frame: Frame },
    /// One way: no answer is waited for.
    Push { node: Node, frame: Frame },
}

/// The handle the event loops use.
#[derive(Clone)]
pub struct Wire {
    cmds: Sender<Cmd>,
    waker: Arc<Waker>,
}

impl Wire {
    /// Forward a request to `node`; its reply goes to loop `back`.
    pub fn send(&self, node: Node, frame: Frame, back: usize) {
        let _ = self.cmds.send(Cmd::Send { node, frame, back });
        let _ = self.waker.wake();
    }

    /// Send a one-way frame (a `WsPush`) to `node`.
    pub fn push(&self, node: Node, frame: Frame) {
        let _ = self.cmds.send(Cmd::Push { node, frame });
        let _ = self.waker.wake();
    }

    /// Answer a request a peer forwarded on connection `conn`.
    pub fn reply(&self, conn: usize, frame: Frame) {
        let _ = self.cmds.send(Cmd::Reply { conn, frame });
        let _ = self.waker.wake();
    }
}

struct Conn {
    sock: TcpStream,
    /// `None`: plain TCP (only with COMBD_WIRE_INSECURE=1).
    tls: Option<rustls::Connection>,
    rbuf: Vec<u8>,
    /// Plain mode only: bytes not written yet.
    wbuf: Vec<u8>,
    /// Outgoing links: the node, and calls waiting for a reply (id -> loop,
    /// and whether it's a peer call).
    node: Option<Node>,
    waiting: HashMap<u64, (usize, bool)>,
    writing: bool,
}

/// Start the hub on `port`. `peers`: every node's wire address.
pub fn start(port: u16, peers: HashMap<Node, String>, tls: Option<Tls>, sink: impl Sink) -> std::io::Result<Wire> {
    let poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
    let mut listener = TcpListener::bind(([0, 0, 0, 0], port).into())?;
    poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
    let (tx, rx) = channel();
    let mut hub = Hub { poll, listener, peers, tls, sink, rx, conns: HashMap::new(), links: HashMap::new(), next: 0 };
    std::thread::spawn(move || {
        if let Err(e) = hub.run() {
            eprintln!("wire: the hub stopped: {e}");
        }
    });
    Ok(Wire { cmds: tx, waker })
}

struct Hub<S: Sink> {
    poll: Poll,
    listener: TcpListener,
    peers: HashMap<Node, String>,
    tls: Option<Tls>,
    sink: S,
    rx: Receiver<Cmd>,
    conns: HashMap<usize, Conn>,
    /// Our open connection to each node.
    links: HashMap<Node, usize>,
    next: usize,
}

impl<S: Sink> Hub<S> {
    fn run(&mut self) -> std::io::Result<()> {
        let mut events = Events::with_capacity(256);
        loop {
            match self.poll.poll(&mut events, None) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => r?,
            }
            for ev in events.iter() {
                match ev.token() {
                    LISTENER => self.accept(),
                    WAKE => {
                        while let Ok(c) = self.rx.try_recv() {
                            self.command(c);
                        }
                    }
                    Token(id) => {
                        if ev.is_readable() || ev.is_read_closed() {
                            self.read(id);
                        }
                        if ev.is_writable() {
                            self.flush(id);
                        }
                    }
                }
            }
        }
    }

    fn add(&mut self, mut sock: TcpStream, tls: Option<rustls::Connection>, node: Option<Node>) -> Option<usize> {
        let id = self.next;
        self.next += 1;
        self.poll.registry().register(&mut sock, Token(id), Interest::READABLE).ok()?;
        let _ = sock.set_nodelay(true);
        self.conns.insert(id, Conn { sock, tls, rbuf: vec![], wbuf: vec![], node, waiting: HashMap::new(), writing: false });
        Some(id)
    }

    fn accept(&mut self) {
        while let Ok((sock, _)) = self.listener.accept() {
            let tls = match &self.tls {
                Some(t) => match rustls::ServerConnection::new(t.server.clone()) {
                    Ok(c) => Some(rustls::Connection::Server(c)),
                    Err(_) => continue,
                },
                None => None,
            };
            self.add(sock, tls, None);
        }
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Reply { conn, frame } => self.queue(conn, &frame.encode()),
            Cmd::Push { node, frame } => {
                let conn = match self.links.get(&node) {
                    Some(&c) => Some(c),
                    None => self.connect(node),
                };
                match (conn, frame) {
                    (Some(c), f) => self.queue(c, &f.encode()),
                    (None, Frame::WsPush { name, sock, .. }) => self.sink.ws_lost(name, sock),
                    (None, _) => {}
                }
            }
            Cmd::Send { node, frame, back } => {
                let (id, peer) = (frame.id(), matches!(frame, Frame::Peer { .. }));
                let conn = match self.links.get(&node) {
                    Some(&c) => Some(c),
                    None => self.connect(node),
                };
                let Some(conn) = conn else {
                    if peer {
                        self.sink.peer_reply(back, id, None);
                    } else {
                        self.sink.reply(back, id, (503, "{\"error\":\"owner unreachable\"}".into()));
                    }
                    return;
                };
                self.conns.get_mut(&conn).unwrap().waiting.insert(id, (back, peer));
                self.queue(conn, &frame.encode());
            }
        }
    }

    /// ponytail: the dial is blocking (500 ms at most per address), on the
    /// hub thread.
    fn connect(&mut self, node: Node) -> Option<usize> {
        use std::net::ToSocketAddrs;
        let addr = self.peers.get(&node)?;
        // Try every address the name gives (localhost may be IPv6 first).
        let s = addr.to_socket_addrs().ok()?.find_map(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(500)).ok());
        let Some(s) = s else {
            eprintln!("wire: can't reach node {node} at {addr}");
            return None;
        };
        s.set_nonblocking(true).ok()?;
        let tls = match &self.tls {
            Some(t) => {
                let name = ServerName::try_from(server_name(node)).ok()?;
                Some(rustls::Connection::Client(rustls::ClientConnection::new(t.client.clone(), name).ok()?))
            }
            None => None,
        };
        let id = self.add(TcpStream::from_std(s), tls, Some(node))?;
        self.links.insert(node, id);
        Some(id)
    }

    fn queue(&mut self, id: usize, bytes: &[u8]) {
        let Some(c) = self.conns.get_mut(&id) else { return };
        match &mut c.tls {
            Some(t) => {
                if t.writer().write_all(bytes).is_err() {
                    return self.fail(id);
                }
            }
            None => c.wbuf.extend_from_slice(bytes),
        }
        self.flush(id);
    }

    fn flush(&mut self, id: usize) {
        let Some(c) = self.conns.get_mut(&id) else { return };
        let broke = match &mut c.tls {
            Some(t) => loop {
                if !t.wants_write() {
                    break false;
                }
                match t.write_tls(&mut c.sock) {
                    Ok(0) => break true,
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break false,
                    Err(_) => break true,
                }
            },
            None => loop {
                if c.wbuf.is_empty() {
                    break false;
                }
                match c.sock.write(&c.wbuf) {
                    Ok(0) => break true,
                    Ok(n) => {
                        c.wbuf.drain(..n);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break false,
                    Err(_) => break true,
                }
            },
        };
        if broke {
            return self.fail(id);
        }
        let want = c.tls.as_ref().map_or(!c.wbuf.is_empty(), |t| t.wants_write());
        if want != c.writing {
            let i = if want { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
            let _ = self.poll.registry().reregister(&mut c.sock, Token(id), i);
            c.writing = want;
        }
    }

    fn read(&mut self, id: usize) {
        let Some(c) = self.conns.get_mut(&id) else { return };
        let mut buf = [0u8; 16 * 1024];
        let broke = match &mut c.tls {
            Some(t) => loop {
                match t.read_tls(&mut c.sock) {
                    Ok(0) => break true,
                    Ok(_) => {
                        if t.process_new_packets().is_err() {
                            break true; // a bad certificate, or garbage
                        }
                        loop {
                            match t.reader().read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => c.rbuf.extend_from_slice(&buf[..n]),
                                Err(_) => break, // WouldBlock: no more plaintext yet
                            }
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break false,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break true,
                }
            },
            None => loop {
                match c.sock.read(&mut buf) {
                    Ok(0) => break true,
                    Ok(n) => c.rbuf.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break false,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break true,
                }
            },
        };
        // Whole frames first: a peer may send its last reply and close.
        loop {
            let c = self.conns.get_mut(&id).unwrap();
            if c.rbuf.len() < 4 {
                break;
            }
            let n = u32::from_le_bytes(c.rbuf[..4].try_into().unwrap()) as usize;
            if c.rbuf.len() < 4 + n {
                if n > MAX_FRAME {
                    return self.fail(id);
                }
                break;
            }
            let frame = Frame::read(&mut &c.rbuf[..4 + n]);
            c.rbuf.drain(..4 + n);
            match frame {
                Ok(Frame::Forward { id: fid, name, req }) if c.node.is_none() => self.sink.forward(id, fid, name, req),
                Ok(Frame::Reply { id: fid, status, body }) if c.node.is_some() => {
                    if let Some((back, _)) = c.waiting.remove(&fid) {
                        self.sink.reply(back, fid, (status, body));
                    }
                }
                Ok(Frame::Peer { id: fid, call }) if c.node.is_none() => self.sink.peer(id, fid, call),
                Ok(Frame::WsPush { name, sock, op, payload }) if c.node.is_none() => self.sink.ws_push(name, sock, op, payload),
                Ok(Frame::PeerReply { id: fid, res }) if c.node.is_some() => {
                    if let Some((back, _)) = c.waiting.remove(&fid) {
                        self.sink.peer_reply(back, fid, Some(res));
                    }
                }
                _ => return self.fail(id), // garbage, or a frame going the wrong way
            }
        }
        if broke {
            self.fail(id);
        } else {
            // TLS may have something to send back (handshake, key updates).
            self.flush(id);
        }
    }

    /// A connection broke: everything waiting on it gets a 503 now.
    fn fail(&mut self, id: usize) {
        let Some(mut c) = self.conns.remove(&id) else { return };
        let _ = self.poll.registry().deregister(&mut c.sock);
        if let Some(n) = c.node {
            if self.links.get(&n) == Some(&id) {
                self.links.remove(&n);
            }
        }
        for (fid, (back, peer)) in c.waiting {
            if peer {
                self.sink.peer_reply(back, fid, None);
            } else {
                self.sink.reply(back, fid, (503, "{\"error\":\"owner went away\"}".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Answers every forward with 200 and its request; collects replies.
    struct Echo {
        wire: Arc<Mutex<Option<Wire>>>,
        got: Sender<(u64, (u16, String))>,
    }

    impl Sink for Echo {
        fn forward(&self, conn: usize, id: u64, name: String, req: String) {
            if let Some(w) = self.wire.lock().unwrap().as_ref() {
                w.reply(conn, Frame::Reply { id, status: 200, body: format!("{name}:{req}") });
            }
        }
        fn reply(&self, _back: usize, id: u64, reply: (u16, String)) {
            let _ = self.got.send((id, reply));
        }
        /// Answers every peer call with `Have(id)`.
        fn peer(&self, conn: usize, id: u64, _call: PeerCall) {
            if let Some(w) = self.wire.lock().unwrap().as_ref() {
                w.reply(conn, Frame::PeerReply { id, res: PeerRes::Have(id) });
            }
        }
        fn ws_push(&self, _: String, _: u64, _: u8, _: Vec<u8>) {}
        fn ws_lost(&self, _: String, _: u64) {}
        fn peer_reply(&self, _back: usize, id: u64, res: Option<PeerRes>) {
            let status = match res {
                Some(PeerRes::Have(n)) if n == id => 200,
                _ => 503,
            };
            let _ = self.got.send((id, (status, "peer".into())));
        }
    }

    fn hub(port: u16, peers: &HashMap<Node, String>, tls: Option<Tls>) -> (Wire, Receiver<(u64, (u16, String))>) {
        let (got, rx) = channel();
        let slot = Arc::new(Mutex::new(None));
        let w = start(port, peers.clone(), tls, Echo { wire: slot.clone(), got }).unwrap();
        *slot.lock().unwrap() = Some(w.clone());
        (w, rx)
    }

    #[test]
    fn mtls_carries_frames_and_refuses_strangers() {
        let dir = crate::store::tempdir::Dir::new();
        make_certs(&dir.0.join("fleet"), 2).unwrap();
        make_certs(&dir.0.join("other"), 2).unwrap(); // another fleet's CA
        let base = 23000 + (std::process::id() % 1000) as u16 * 4;
        let peers: HashMap<Node, String> = (0..2).map(|n| (n, format!("127.0.0.1:{}", base + n as u16))).collect();
        let (a, ra) = hub(base, &peers, Some(load_tls(&dir.0.join("fleet"), 0).unwrap()));
        let (_b, _rb) = hub(base + 1, &peers, Some(load_tls(&dir.0.join("fleet"), 1).unwrap()));
        a.send(1, Frame::Forward { id: 7, name: "room".into(), req: "do GET /count".into() }, 0);
        assert_eq!(ra.recv_timeout(Duration::from_secs(5)).unwrap(), (7, (200, "room:do GET /count".into())));
        a.send(1, Frame::Peer { id: 10, call: PeerCall::Fence { obj: 1, epoch: 2 } }, 0);
        assert_eq!(ra.recv_timeout(Duration::from_secs(5)).unwrap(), (10, (200, "peer".into())));

        // A node with a certificate from another CA gets nothing through.
        let (x, rx) = hub(base + 2, &peers, Some(load_tls(&dir.0.join("other"), 0).unwrap()));
        x.send(1, Frame::Forward { id: 8, name: "room".into(), req: "do GET /count".into() }, 0);
        let (id, (status, _)) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((id, status), (8, 503), "a stranger's request must fail");
        x.send(1, Frame::Peer { id: 11, call: PeerCall::Fence { obj: 1, epoch: 2 } }, 0);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), (11, (503, "peer".into())), "a stranger's peer call is lost");

        // Nor does a plain-TCP node.
        let (p, rp) = hub(base + 3, &peers, None);
        p.send(1, Frame::Forward { id: 9, name: "room".into(), req: "do GET /count".into() }, 0);
        let (id, (status, _)) = rp.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((id, status), (9, 503), "a plain-TCP request must fail");
    }
}
