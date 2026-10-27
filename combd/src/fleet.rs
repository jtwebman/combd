//! D2 on real machines: the pieces the HTTP front uses when a node runs in
//! a fleet (when `COMBD_S3` is set).
//!
//! - The bucket: the decision core's calls (`own::Call`) done on S3.
//! - The wire: how nodes talk to each other. Long-lived mTLS connections
//!   carrying length-prefixed binary frames: requests forwarded to an
//!   object's owner, and D3 peer calls.
//! - The peer store on disk (`DiskPeers`): the entries this node keeps for
//!   other nodes' objects, in a log file with group commit.
//!
//! Settings:
//! - `COMBD_NODE`: this node's number.
//! - `COMBD_PEERS`: `0=host:7100,1=host:7100,...`, every node's wire address.
//! - `COMBD_WIRE_PORT`: where this node listens for peers (default 7100).
//! - `COMBD_LEASE_MS`: how long a silent node keeps its objects (default 10 s).
//! - `COMBD_REPLICAS`: peers per object for D3 fast writes (default 0: D2).
//! - `COMBD_WIRE_CERTS`: the wire's certificates (`combd wire-certs`),
//!   or `COMBD_WIRE_INSECURE=1` for plain TCP.
//! - `COMBD_S3`, `COMBD_S3_KEY`, `COMBD_S3_SECRET`, `COMBD_S3_BUCKET`: the bucket.

use crate::own::{Call, Cond, Node, PeerCall, PeerRes, PeerStore, Res, Tail, Val};
use crate::s3::{self, S3};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct FleetCfg {
    pub node: Node,
    /// `COMBD_LEASE_MS` (default 10 s), renewed every third of it.
    pub lease_ms: u64,
    pub replicas: usize,
    pub s3: S3,
    pub peers: HashMap<Node, String>,
    pub wire_port: u16,
    /// The wire's certificates, or `None` with `insecure` for plain TCP.
    pub wire_certs: Option<std::path::PathBuf>,
    pub insecure: bool,
    /// How long an object's uploads may wait while its peers hold the
    /// writes (`COMBD_UPLOAD_MS`, default 10 s), or until this many bytes
    /// build up (`COMBD_UPLOAD_BYTES`, default 1 MB, at most 16 MB).
    pub upload_ms: u64,
    pub upload_bytes: usize,
    /// Read leases (`COMBD_READ_LEASE=1`): owners answer reads with no round
    /// while their peers' promises hold. Peers always promise; turn this on
    /// only once every node runs a version that does (an older peer would
    /// promise nothing).
    pub read_lease: bool,
    /// Where each node is (`COMBD_REGIONS`: `0=nyc3/a,1=fra1/a,...`, a
    /// region and, after a slash, a zone), for placement's scope. A node
    /// not listed is in the region (and zone) of every other one not listed.
    pub regions: HashMap<Node, String>,
    /// Placement: how often an owner looks at where its objects' requests
    /// come in, and moves one whose traffic is elsewhere (`COMBD_PLACE_MS`,
    /// default 30 minutes; 0: never).
    pub place_ms: u64,
    /// Named sets of regions, for `pin` (`COMBD_AREAS`: `eu=fra1+ams3,us=nyc3`).
    pub areas: HashMap<String, Vec<String>>,
}

/// Placement's scope: which nodes a spread object may start on, seen from
/// `me`: those in its zone, its region, or all of them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scope {
    Zone,
    Region,
    Fleet,
}

pub fn among(regions: &HashMap<Node, String>, me: Node, all: &[Node], scope: Scope) -> Vec<Node> {
    let place = |n: Node| regions.get(&n).map_or("", String::as_str);
    let region = |n: Node| place(n).split('/').next().unwrap_or("");
    all.iter()
        .copied()
        .filter(|&n| match scope {
            Scope::Fleet => true,
            Scope::Region => region(n) == region(me),
            Scope::Zone => place(n) == place(me),
        })
        .collect()
}

/// The nodes a pinned object may live on (`placement = "pin"`): those in
/// the regions it names, each a region or an area (`COMBD_AREAS`).
pub fn pinned(regions: &HashMap<Node, String>, areas: &HashMap<String, Vec<String>>, all: &[Node], pin: &[String]) -> Vec<Node> {
    let ok: Vec<&str> = pin.iter().flat_map(|p| areas.get(p).map_or(vec![p.as_str()], |a| a.iter().map(String::as_str).collect())).collect();
    all.iter().copied().filter(|n| regions.get(n).is_some_and(|r| ok.contains(&r.split('/').next().unwrap_or("")))).collect()
}

impl FleetCfg {
    pub fn from_env() -> Option<FleetCfg> {
        let s3 = S3::from_env()?;
        let node = std::env::var("COMBD_NODE").ok()?.parse().ok()?;
        let peers = std::env::var("COMBD_PEERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|p| {
                let (n, addr) = p.split_once('=')?;
                Some((n.trim().parse().ok()?, addr.trim().to_string()))
            })
            .collect();
        let wire_port = std::env::var("COMBD_WIRE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(7100);
        let lease_ms = std::env::var("COMBD_LEASE_MS").ok().and_then(|p| p.parse().ok()).unwrap_or(10_000);
        let replicas = std::env::var("COMBD_REPLICAS").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        let wire_certs = std::env::var("COMBD_WIRE_CERTS").ok().map(Into::into);
        let insecure = std::env::var("COMBD_WIRE_INSECURE").as_deref() == Ok("1");
        let upload_ms = std::env::var("COMBD_UPLOAD_MS").ok().and_then(|p| p.parse().ok()).unwrap_or(10_000);
        if upload_ms > 300_000 {
            eprintln!("combd     warning: COMBD_UPLOAD_MS is {upload_ms}: an object's newest writes live only on its peers for up to {} minutes", upload_ms / 60_000);
        }
        // Under the wire's frame limit: a takeover gets the rest in one reply.
        let upload_bytes = std::env::var("COMBD_UPLOAD_BYTES").ok().and_then(|p| p.parse().ok()).unwrap_or(1 << 20).min(16 << 20);
        let read_lease = std::env::var("COMBD_READ_LEASE").as_deref() == Ok("1");
        let regions = std::env::var("COMBD_REGIONS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|p| {
                let (n, place) = p.split_once('=')?;
                Some((n.trim().parse().ok()?, place.trim().to_string()))
            })
            .collect();
        let place_ms = std::env::var("COMBD_PLACE_MS").ok().and_then(|p| p.parse().ok()).unwrap_or(30 * 60_000);
        let areas = std::env::var("COMBD_AREAS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|a| {
                let (name, regions) = a.split_once('=')?;
                Some((name.trim().to_string(), regions.split('+').map(|r| r.trim().to_string()).collect()))
            })
            .collect();
        Some(FleetCfg { node, lease_ms, replicas, s3, peers, wire_port, wire_certs, insecure, upload_ms, upload_bytes, read_lease, regions, place_ms, areas })
    }
}

/// One of the core's bucket calls, on S3. Any error is `Lost`: the core
/// treats it as "may or may not have happened", which is the truth.
pub fn run_call(s3: &S3, call: &Call) -> Res {
    let r = match call {
        Call::Get(k) => s3.get(k).map(|v| Res::Got(v.and_then(|(b, etag)| Val::decode(k, &b).map(|v| (v, etag))))),
        Call::Put(k, v, cond) => {
            let cond = match cond {
                Cond::Any => s3::Cond::Any,
                Cond::IfAbsent => s3::Cond::IfAbsent,
                Cond::IfMatch(e) => s3::Cond::IfMatch(e.clone()),
            };
            s3.put(k, &v.encode(), &cond).map(Res::Put)
        }
        Call::List(prefix) => s3.list(prefix).map(Res::Listed),
        Call::Delete(k) => s3.delete(k).map(|_| Res::Deleted),
    };
    r.unwrap_or_else(|e| {
        eprintln!("bucket: {call:?}: {e}");
        Res::Lost
    })
}

/// Every entry of a restore, in order: each segment holds several.
pub fn fetch(s3: &S3, segs: &[String]) -> Result<Vec<Vec<u8>>, String> {
    // Many GETs at once (the client keeps that many connections): an
    // object's segments pile up, one per upload, and one at a time from a
    // node an ocean from the bucket was a minute (200 segments, 300 ms each).
    const AT_ONCE: usize = 16;
    let get = |k: &String| -> Result<Vec<Vec<u8>>, String> {
        match s3.get(k) {
            Ok(Some((b, _))) => crate::own::unframe(&b).ok_or_else(|| format!("segment {k} is garbled")),
            Ok(None) => Err(format!("segment {k} is missing")),
            Err(e) => Err(format!("segment {k}: {e}")),
        }
    };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let got: Vec<std::sync::Mutex<Option<Result<Vec<Vec<u8>>, String>>>> = segs.iter().map(|_| Default::default()).collect();
    std::thread::scope(|sc| {
        for _ in 0..AT_ONCE.min(segs.len()) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(k) = segs.get(i) else { break };
                let r = get(k);
                let stop = r.is_err();
                *got[i].lock().unwrap() = Some(r);
                if stop {
                    // No point fetching the rest.
                    next.store(segs.len(), std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
    });
    let mut out = vec![];
    for (k, g) in segs.iter().zip(got) {
        out.extend(g.into_inner().unwrap().unwrap_or_else(|| Err(format!("segment {k}: not fetched")))?);
    }
    Ok(out)
}

/// A frame on the wire.
#[derive(Debug, PartialEq)]
pub enum Frame {
    /// Run this object request on the node that owns it.
    Forward { id: u64, name: String, req: String },
    /// Its answer.
    Reply { id: u64, status: u16, body: String },
    /// A call to this node's peer store, and its answer.
    Peer { id: u64, call: PeerCall },
    PeerReply { id: u64, res: PeerRes },
    /// One way, no answer: send this on a WebSocket whose connection is on
    /// your node (op: 1 text, 2 binary, 8 close with a 2-byte code).
    WsPush { name: String, sock: u64, op: u8, payload: Vec<u8> },
    /// One way: the catalog's entry at slot `seq` (`catalog.rs`), pushed to
    /// every node after its write.
    Catalog { seq: u64, entry: Vec<u8> },
    /// One way: "node `node` is alive", sent to every peer a few times a
    /// lease. `boot` tells one process of a node from another.
    Heartbeat { node: u32, boot: u64 },
    /// One way: a message between the pools of two nodes (`pg.rs`).
    Pool { body: Vec<u8> },
}

impl Frame {
    pub fn id(&self) -> u64 {
        match self {
            Frame::Forward { id, .. } | Frame::Reply { id, .. } | Frame::Peer { id, .. } | Frame::PeerReply { id, .. } => *id,
            Frame::WsPush { .. } | Frame::Catalog { .. } | Frame::Heartbeat { .. } | Frame::Pool { .. } => 0,
        }
    }
}

fn put_ents(b: &mut Vec<u8>, ents: &[Vec<u8>]) {
    let f = crate::own::frame(ents);
    b.extend_from_slice(&(f.len() as u32).to_le_bytes());
    b.extend_from_slice(&f);
}

/// Reads little-endian fields off the front; `None` when it runs short.
struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.take(16)?.try_into().ok()?))
    }
    fn ents(&mut self) -> Option<Vec<Vec<u8>>> {
        let n = self.u32()? as usize;
        crate::own::unframe(self.take(n)?)
    }
}

pub fn encode_call(c: &PeerCall, b: &mut Vec<u8>) {
    match c {
        PeerCall::Append { obj, epoch, from, ents, trunc } => {
            b.push(1);
            b.extend_from_slice(&obj.to_le_bytes());
            b.extend_from_slice(&epoch.to_le_bytes());
            b.extend_from_slice(&from.to_le_bytes());
            b.extend_from_slice(&trunc.to_le_bytes());
            put_ents(b, ents);
        }
        PeerCall::Fence { obj, epoch } => {
            b.push(2);
            b.extend_from_slice(&obj.to_le_bytes());
            b.extend_from_slice(&epoch.to_le_bytes());
        }
        PeerCall::Learn { obj, epoch } => {
            b.push(3);
            b.extend_from_slice(&obj.to_le_bytes());
            b.extend_from_slice(&epoch.to_le_bytes());
        }
    }
}

fn decode_call(r: &mut Rd) -> Option<PeerCall> {
    Some(match r.u8()? {
        1 => PeerCall::Append { obj: r.u128()?, epoch: r.u32()?, from: r.u64()?, trunc: r.u64()?, ents: r.ents()? },
        2 => PeerCall::Fence { obj: r.u128()?, epoch: r.u32()? },
        3 => PeerCall::Learn { obj: r.u128()?, epoch: r.u32()? },
        _ => return None,
    })
}

fn encode_res(res: &PeerRes, b: &mut Vec<u8>) {
    match res {
        PeerRes::Have(n) => {
            b.push(1);
            b.extend_from_slice(&n.to_le_bytes());
        }
        PeerRes::Refused => b.push(2),
        PeerRes::Wait(ms) => {
            b.push(4);
            b.extend_from_slice(&ms.to_le_bytes());
        }
        PeerRes::Kept(n) => {
            b.push(5);
            b.extend_from_slice(&n.to_le_bytes());
        }
        PeerRes::Tails { born, tails: ts } => {
            b.push(3);
            b.extend_from_slice(&born.to_le_bytes());
            b.extend_from_slice(&(ts.len() as u32).to_le_bytes());
            for t in ts {
                b.extend_from_slice(&t.epoch.to_le_bytes());
                b.extend_from_slice(&t.from.to_le_bytes());
                put_ents(b, &t.ents);
            }
        }
    }
}

fn decode_res(r: &mut Rd) -> Option<PeerRes> {
    Some(match r.u8()? {
        1 => PeerRes::Have(r.u64()?),
        2 => PeerRes::Refused,
        4 => PeerRes::Wait(r.u64()?),
        5 => PeerRes::Kept(r.u64()?),
        3 => {
            let born = r.u32()?;
            let n = r.u32()?;
            let mut ts = vec![];
            for _ in 0..n {
                ts.push(Tail { epoch: r.u32()?, from: r.u64()?, ents: r.ents()? });
            }
            PeerRes::Tails { born, tails: ts }
        }
        _ => return None,
    })
}

// A fence's tails must fit too: uploads start by COMBD_UPLOAD_BYTES (at
// most 16 MB), well under this.
pub const MAX_FRAME: usize = 64 << 20;

impl Frame {
    /// `[u32 length][u8 kind][u64 id][...]`, little-endian.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; 4];
        match self {
            Frame::Forward { id, name, req } => {
                b.push(1);
                b.extend_from_slice(&id.to_le_bytes());
                b.extend_from_slice(&(name.len() as u16).to_le_bytes());
                b.extend_from_slice(name.as_bytes());
                b.extend_from_slice(req.as_bytes());
            }
            Frame::Reply { id, status, body } => {
                b.push(2);
                b.extend_from_slice(&id.to_le_bytes());
                b.extend_from_slice(&status.to_le_bytes());
                b.extend_from_slice(body.as_bytes());
            }
            Frame::Peer { id, call } => {
                b.push(3);
                b.extend_from_slice(&id.to_le_bytes());
                encode_call(call, &mut b);
            }
            Frame::PeerReply { id, res } => {
                b.push(4);
                b.extend_from_slice(&id.to_le_bytes());
                encode_res(res, &mut b);
            }
            Frame::Heartbeat { node, boot } => {
                b.push(7);
                b.extend_from_slice(&boot.to_le_bytes());
                b.extend_from_slice(&node.to_le_bytes());
            }
            Frame::Pool { body } => {
                b.push(8);
                b.extend_from_slice(&0u64.to_le_bytes());
                b.extend_from_slice(body);
            }
            Frame::Catalog { seq, entry } => {
                b.push(6);
                b.extend_from_slice(&seq.to_le_bytes());
                b.extend_from_slice(entry);
            }
            Frame::WsPush { name, sock, op, payload } => {
                b.push(5);
                b.extend_from_slice(&sock.to_le_bytes());
                b.push(*op);
                b.extend_from_slice(&(name.len() as u16).to_le_bytes());
                b.extend_from_slice(name.as_bytes());
                b.extend_from_slice(payload);
            }
        }
        let n = (b.len() - 4) as u32;
        b[..4].copy_from_slice(&n.to_le_bytes());
        b
    }

    pub fn read(r: &mut impl Read) -> std::io::Result<Frame> {
        let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
        let mut n = [0u8; 4];
        r.read_exact(&mut n)?;
        let n = u32::from_le_bytes(n) as usize;
        if !(9..=MAX_FRAME).contains(&n) {
            return Err(bad("bad frame length"));
        }
        // Only as much memory as really arrives: a length alone can't make
        // us allocate 64 MB.
        let mut b = Vec::new();
        r.take(n as u64).read_to_end(&mut b)?;
        if b.len() < n {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short frame"));
        }
        let id = u64::from_le_bytes(b[1..9].try_into().unwrap());
        let text = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| bad("frame text isn't UTF-8"));
        match b[0] {
            1 => {
                let l = u16::from_le_bytes(b.get(9..11).ok_or_else(|| bad("short frame"))?.try_into().unwrap()) as usize;
                let name = text(b.get(11..11 + l).ok_or_else(|| bad("short frame"))?)?;
                Ok(Frame::Forward { id, name, req: text(&b[11 + l..])? })
            }
            2 => {
                let status = u16::from_le_bytes(b.get(9..11).ok_or_else(|| bad("short frame"))?.try_into().unwrap());
                Ok(Frame::Reply { id, status, body: text(&b[11..])? })
            }
            3 => {
                let mut r = Rd(&b[9..]);
                match decode_call(&mut r) {
                    // `Learn` lives only in a store's own log.
                    Some(PeerCall::Learn { .. }) => Err(bad("a peer can't send that")),
                    Some(call) if r.0.is_empty() => Ok(Frame::Peer { id, call }),
                    _ => Err(bad("bad peer call")),
                }
            }
            4 => {
                let mut r = Rd(&b[9..]);
                match decode_res(&mut r) {
                    Some(res) if r.0.is_empty() => Ok(Frame::PeerReply { id, res }),
                    _ => Err(bad("bad peer reply")),
                }
            }
            5 => {
                // `id` holds the socket id here.
                let mut r = Rd(&b[9..]);
                let (op, l) = (r.u8().ok_or_else(|| bad("short frame"))?, r.take(2).map(|x| u16::from_le_bytes([x[0], x[1]]) as usize).ok_or_else(|| bad("short frame"))?);
                let name = text(r.take(l).ok_or_else(|| bad("short frame"))?)?;
                Ok(Frame::WsPush { name, sock: id, op, payload: r.0.to_vec() })
            }
            // `id` holds the slot here.
            6 => Ok(Frame::Catalog { seq: id, entry: b[9..].to_vec() }),
            // `id` holds the boot here.
            7 => match b.get(9..) {
                Some(n) if n.len() == 4 => Ok(Frame::Heartbeat { node: u32::from_le_bytes(n.try_into().unwrap()), boot: id }),
                _ => Err(bad("bad heartbeat")),
            },
            8 => Ok(Frame::Pool { body: b[9..].to_vec() }),
            _ => Err(bad("unknown frame kind")),
        }
    }
}

/// The peer store, kept in a log file: every call that changed it, each as
/// `[u32 length][4 bytes of SHA-256][call]`. Startup replays the log and
/// cuts off a torn last record. When the log grows past `COMPACT`, it's
/// rewritten from a snapshot (usually small: peers drop what's uploaded).
/// A store that lost its disk keeps a `wiped` marker file beside the log.
pub struct DiskPeers {
    pub store: PeerStore,
    path: PathBuf,
    file: std::fs::File,
    size: u64,
    /// The size right after the last compaction: what the store holds.
    base: u64,
    /// This process's start, for the store's clock.
    t0: std::time::Instant,
    dirty: bool,
}

/// Compact past this, or past twice what the last compaction left (so a
/// store that holds a lot, with uploads waiting, doesn't compact on every
/// sync).
const COMPACT: u64 = 64 << 20;

fn record(c: &PeerCall) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut body = vec![];
    encode_call(c, &mut body);
    let mut b = (body.len() as u32).to_le_bytes().to_vec();
    b.extend_from_slice(&Sha256::digest(&body)[..4]);
    b.extend_from_slice(&body);
    b
}

impl DiskPeers {
    /// `ran_before`: this node has served before (its lease is in the
    /// bucket). If so and the log is gone, the disk was lost: the store
    /// comes back wiped, and stays so.
    pub fn open(dir: &Path, ran_before: bool) -> std::io::Result<DiskPeers> {
        use sha2::{Digest, Sha256};
        std::fs::create_dir_all(dir)?;
        let path = dir.join("peers.log");
        let marker = dir.join("wiped");
        if !path.exists() && ran_before && !marker.exists() {
            eprintln!("combd     the peer store was lost: it comes back wiped, and relearns each object's fence from the bucket");
            std::fs::File::create(&marker)?.sync_all()?;
            std::fs::File::open(dir)?.sync_all()?;
        }
        let bytes = std::fs::read(&path).unwrap_or_default();
        let mut store = PeerStore::default();
        store.wiped = marker.exists();
        let mut at = 0;
        while let Some(h) = bytes.get(at..at + 8) {
            let n = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
            let Some(body) = bytes.get(at + 8..at + 8 + n) else { break };
            if Sha256::digest(body)[..4] != h[4..8] {
                break;
            }
            let mut r = Rd(body);
            let Some(c) = decode_call(&mut r) else { break };
            store.handle(c);
            at += 8 + n;
        }
        if at < bytes.len() {
            eprintln!("combd     peers.log: cut a torn record at byte {at} of {}", bytes.len());
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        file.set_len(at as u64)?;
        file.sync_all()?;
        Ok(DiskPeers { store, path, file, size: at as u64, base: 0, dirty: false, t0: std::time::Instant::now() })
    }

    /// Apply a call; it's durable after the next `sync`. A wiped store first
    /// relearns the object (`learn`: the epoch in its ownership record);
    /// `None` if it can't, and then it doesn't answer.
    pub fn handle(&mut self, c: PeerCall, learn: &mut impl FnMut(crate::own::Obj) -> Option<u32>) -> std::io::Result<Option<PeerRes>> {
        if let Some(obj) = self.store.must_learn(&c) {
            let Some(epoch) = learn(obj) else { return Ok(None) };
            self.apply(PeerCall::Learn { obj, epoch })?;
        }
        self.apply(c).map(Some)
    }

    /// Read leases: promise owners `ms` (see `PeerStore::promise_ms`),
    /// counted from now for this process (a restart forgets its promises,
    /// so it holds off every fence that long first).
    pub fn promise(&mut self, ms: u64) {
        self.store.promise_ms = ms;
        self.t0 = std::time::Instant::now();
        self.store.started(0);
    }

    fn apply(&mut self, c: PeerCall) -> std::io::Result<PeerRes> {
        let rec = record(&c);
        let res = self.store.handle_at(c, self.t0.elapsed().as_millis() as u64);
        // Refused, or "not yet" (a promise kept): nothing changed.
        if !matches!(res, PeerRes::Refused | PeerRes::Wait(_)) {
            self.file.write_all(&rec)?;
            self.size += rec.len() as u64;
            self.dirty = true;
        }
        Ok(res)
    }

    pub fn sync(&mut self) -> std::io::Result<()> {
        if std::mem::take(&mut self.dirty) {
            self.file.sync_data()?;
        }
        if self.size > COMPACT.max(2 * self.base) {
            self.compact()?;
        }
        Ok(())
    }

    /// Write a snapshot beside the log, fsync it, and swap it in.
    fn compact(&mut self) -> std::io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut b = vec![];
        for c in self.store.snapshot() {
            b.extend_from_slice(&record(&c));
        }
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&b)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        if let Some(dir) = self.path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        self.file = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        self.size = b.len() as u64;
        self.base = self.size;
        Ok(())
    }
}

/// The peer store's thread: take every call waiting, apply them, one fsync,
/// then answer them all (group commit). An answer means it's on disk.
/// `learn` reads an object's ownership record, for a wiped store.
pub fn serve_peers<B: Send + 'static>(
    mut disk: DiskPeers,
    rx: std::sync::mpsc::Receiver<(PeerCall, B)>,
    mut learn: impl FnMut(crate::own::Obj) -> Option<u32> + Send + 'static,
    answer: impl Fn(B, Option<PeerRes>) + Send + 'static,
) {
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            batch.extend(rx.try_iter());
            let done: Vec<(B, Option<PeerRes>)> = batch
                .into_iter()
                .map(|(c, back)| {
                    let res = disk.handle(c, &mut learn).unwrap_or_else(|e| fatal(e));
                    (back, res)
                })
                .collect();
            disk.sync().unwrap_or_else(|e| fatal(e));
            for (back, res) in done {
                answer(back, res);
            }
        }
    });
}

/// A peer store that can't write can't promise anything: stop the node
/// rather than answer without the entry on disk.
fn fatal(e: std::io::Error) -> ! {
    eprintln!("combd     the peer store can't write: {e}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    /// A pin names regions, or areas of them: its nodes are the ones there.
    #[test]
    fn a_pin_resolves_regions_and_areas() {
        let regions: HashMap<Node, String> = [(0, "nyc3/a"), (1, "fra1/a"), (2, "ams3/b")].map(|(n, r)| (n, r.to_string())).into();
        let areas: HashMap<String, Vec<String>> = [("eu".to_string(), vec!["fra1".to_string(), "ams3".to_string()])].into();
        let pin = |p: &[&str]| pinned(&regions, &areas, &[0, 1, 2, 3], &p.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(pin(&["eu"]), [1, 2]);
        assert_eq!(pin(&["nyc3"]), [0]);
        assert_eq!(pin(&["fra1", "nyc3"]), [0, 1]);
        assert!(pin(&["sgp1"]).is_empty(), "nowhere: node 3 has no region");
    }

    #[test]
    fn placement_scope_picks_the_nodes_near() {
        let regions: HashMap<Node, String> = [(0, "nyc3/a"), (1, "nyc3/b"), (2, "fra1/a"), (3, "nyc3/a")].map(|(n, p)| (n, p.to_string())).into();
        let all = [0, 1, 2, 3, 4];
        assert_eq!(among(&regions, 0, &all, Scope::Zone), [0, 3]);
        assert_eq!(among(&regions, 0, &all, Scope::Region), [0, 1, 3]);
        assert_eq!(among(&regions, 2, &all, Scope::Region), [2]);
        assert_eq!(among(&regions, 0, &all, Scope::Fleet), all);
        assert_eq!(among(&regions, 4, &all, Scope::Region), [4], "unlisted: with the unlisted");
        assert_eq!(among(&HashMap::new(), 1, &all, Scope::Zone), all, "none listed: one place");
    }

    use super::*;

    /// Frames from another node, damaged at random (a buggy or compromised
    /// peer): decoding never panics, and whatever decodes, the peer store
    /// takes without panicking. A damaged peer log replays without panicking.
    #[test]
    fn mangled_frames_and_logs_never_panic() {
        let mut rng = crate::sim::Rng::new(3);
        let ents = |rng: &mut crate::sim::Rng| (0..rng.below(4)).map(|_| vec![rng.next() as u8; rng.below(40) as usize]).collect::<Vec<_>>();
        let mut store = PeerStore::default();
        for i in 0..100_000u64 {
            let f = match rng.below(6) {
                0 => Frame::Forward { id: rng.next(), name: "room".into(), req: "do GET /".into() },
                1 => Frame::Reply { id: rng.next(), status: 200, body: "{}".into() },
                2 => Frame::Peer { id: 1, call: PeerCall::Append { obj: rng.below(4) as u128, epoch: rng.below(5) as u32, from: rng.below(8), ents: ents(&mut rng), trunc: rng.below(8) } },
                3 => Frame::Peer { id: 2, call: PeerCall::Fence { obj: rng.below(4) as u128, epoch: rng.below(5) as u32 } },
                4 => Frame::PeerReply { id: 3, res: PeerRes::Tails { born: 1, tails: vec![Tail { epoch: 1, from: 2, ents: ents(&mut rng) }] } },
                _ => Frame::WsPush { name: "room".into(), sock: rng.next(), op: 1, payload: ents(&mut rng).concat() },
            };
            let mut b = f.encode();
            for _ in 0..1 + rng.below(3) {
                let at = 4 + rng.below(b.len() as u64 - 3) as usize;
                match rng.below(4) {
                    0 if at < b.len() => b[at] ^= 1 << rng.below(8),
                    1 => b.insert(at.min(b.len()), rng.next() as u8),
                    2 => b.truncate(at),
                    _ => {
                        let n = rng.below(b.len() as u64) as usize;
                        b[..4].copy_from_slice(&(n as u32).to_le_bytes()); // a length that lies
                    }
                }
            }
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(Frame::Peer { call, .. }) = Frame::read(&mut &b[..]) {
                    store.handle(call);
                }
            }));
            assert!(r.is_ok(), "case {i}: {b:x?}");
        }
        // A peer log with damage in the middle replays what it can.
        let dir = std::env::temp_dir().join(format!("combd-peers-fuzz-{}", std::process::id()));
        for i in 0..200u64 {
            let _ = std::fs::remove_dir_all(&dir);
            let mut d = DiskPeers::open(&dir, false).unwrap();
            for e in 1..20u32 {
                d.handle(PeerCall::Append { obj: 1, epoch: e / 5 + 1, from: 1, ents: ents(&mut rng), trunc: 0 }, &mut |_| None).unwrap();
            }
            drop(d);
            let p = dir.join("peers.log");
            let mut bytes = std::fs::read(&p).unwrap();
            let at = rng.below(bytes.len() as u64) as usize;
            bytes[at] ^= 1 << rng.below(8);
            if rng.chance(0.3) {
                bytes.truncate(rng.below(bytes.len() as u64) as usize);
            }
            std::fs::write(&p, bytes).unwrap();
            let r = std::panic::catch_unwind(|| DiskPeers::open(&dir, false).map(|_| ()));
            assert!(matches!(r, Ok(Ok(()))), "log {i}: {r:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_store_survives_restarts_torn_records_and_compaction() {
        let dir = std::env::temp_dir().join(format!("combd-peers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = |d: &mut DiskPeers, epoch, from, n: u8| d.handle(PeerCall::Append { obj: 7, epoch, from, ents: vec![vec![n; 100]], trunc: 0 }, &mut |_| None).unwrap().unwrap();
        let fence = |d: &mut DiskPeers, epoch| d.handle(PeerCall::Fence { obj: 7, epoch }, &mut |_| None).unwrap().unwrap();
        let mut d = DiskPeers::open(&dir, false).unwrap();
        app(&mut d, 1, 1, 1);
        app(&mut d, 1, 2, 2);
        fence(&mut d, 2);
        d.sync().unwrap();
        let want = fence(&mut d, 2);
        drop(d);
        // A torn record at the end (a crash mid-write) is cut off.
        std::fs::OpenOptions::new().append(true).open(dir.join("peers.log")).unwrap().write_all(&[50, 0, 0, 0, 1, 2]).unwrap();
        let mut d = DiskPeers::open(&dir, true).unwrap();
        assert_eq!(fence(&mut d, 2), want);
        assert_eq!(app(&mut d, 1, 3, 3), PeerRes::Refused, "the fence survived");
        d.compact().unwrap();
        drop(d);
        let mut d = DiskPeers::open(&dir, true).unwrap();
        assert_eq!(fence(&mut d, 2), want, "compaction keeps the same store");
        assert_eq!(app(&mut d, 2, 1, 9), PeerRes::Have(1));
        assert!(!d.store.wiped, "it never lost its log");
        drop(d);
        // The disk is lost: the store comes back wiped, relearns from the
        // bucket before answering, and stays wiped after a restart.
        std::fs::remove_file(dir.join("peers.log")).unwrap();
        let mut d = DiskPeers::open(&dir, true).unwrap();
        assert!(d.store.wiped);
        let call = PeerCall::Append { obj: 7, epoch: 2, from: 1, ents: vec![vec![1]], trunc: 0 };
        assert_eq!(d.handle(call.clone(), &mut |_| None).unwrap(), None, "no answer without the bucket");
        assert_eq!(d.handle(call, &mut |_| Some(3)).unwrap(), Some(PeerRes::Refused), "epoch 2 is fenced by the record");
        d.sync().unwrap();
        drop(d);
        let mut d = DiskPeers::open(&dir, true).unwrap();
        assert!(d.store.wiped);
        let Some(PeerRes::Tails { born, .. }) = d.handle(PeerCall::Fence { obj: 7, epoch: 4 }, &mut |_| panic!("already learned")).unwrap() else { panic!() };
        assert_eq!(born, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Frames come from other nodes: damaged at random, the reader must
    /// never panic.
    #[test]
    fn mangled_frames_never_panic() {
        let frames = [
            Frame::Forward { id: 7, name: "acme/app/room/1".into(), req: "do POST /incr {\"a\":1}".into() },
            Frame::Reply { id: 9, status: 200, body: "{\"ok\":true}".into() },
            Frame::Peer { id: 3, call: PeerCall::Append { obj: 5, epoch: 2, from: 1, ents: vec![vec![1, 2, 3], vec![4; 40]], trunc: 0 } },
            Frame::Peer { id: 4, call: PeerCall::Fence { obj: 5, epoch: 3 } },
            Frame::PeerReply { id: 3, res: PeerRes::Tails { born: 1, tails: vec![Tail { epoch: 2, from: 0, ents: vec![vec![9; 12]] }] } },
            Frame::PeerReply { id: 4, res: PeerRes::Wait(1500) },
            Frame::WsPush { name: "acme/app/room/1".into(), sock: 77, op: 1, payload: b"hello".to_vec() },
            Frame::Catalog { seq: 12, entry: b"entry".to_vec() },
            Frame::Heartbeat { node: 2, boot: 99 },
            Frame::Pool { body: b"{\"pool\":\"a/b/c\",\"epoch\":1,\"from\":0,\"t\":\"free\",\"member\":3}".to_vec() },
        ];
        let mut rng = crate::sim::Rng::new(11);
        for i in 0..100_000 {
            let b = crate::sim::mangle(&mut rng, &frames[i % frames.len()].encode());
            let _ = std::panic::catch_unwind(|| Frame::read(&mut &b[..])).expect("the frame reader panicked");
        }
    }

    #[test]
    fn frames_round_trip_and_reject_garbage() {
        for f in [
            Frame::Forward { id: 7, name: "room/1".into(), req: "do POST /incr {\"a\":1}".into() },
            Frame::Reply { id: u64::MAX, status: 503, body: String::new() },
            Frame::Peer { id: 1, call: PeerCall::Append { obj: u128::MAX, epoch: 3, from: 9, ents: vec![vec![], vec![1, 2]], trunc: 8 } },
            Frame::Peer { id: 2, call: PeerCall::Fence { obj: 5, epoch: 4 } },
            Frame::PeerReply { id: 3, res: PeerRes::Have(12) },
            Frame::PeerReply { id: 4, res: PeerRes::Refused },
            Frame::WsPush { name: "room".into(), sock: 77, op: 2, payload: vec![0, 255, 7] },
            Frame::Catalog { seq: 42, entry: b"{\"id\":1}".to_vec() },
            Frame::Heartbeat { node: 7, boot: u64::MAX },
            Frame::Pool { body: b"{}".to_vec() },
            Frame::PeerReply { id: 9, res: PeerRes::Wait(1500) },
            Frame::PeerReply { id: 10, res: PeerRes::Kept(7) },
            Frame::PeerReply { id: 5, res: PeerRes::Tails { born: 4, tails: vec![Tail { epoch: 2, from: 7, ents: vec![vec![3; 5]] }] } },
        ] {
            let b = f.encode();
            assert_eq!(Frame::read(&mut &b[..]).unwrap(), f);
        }
        assert!(Frame::read(&mut &[0xff, 0xff, 0xff, 0x7f, 1][..]).is_err(), "a huge length must be refused");
        assert!(Frame::read(&mut &[10, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0][..]).is_err(), "an unknown kind must be refused");
    }
}
