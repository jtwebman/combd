//! The line between a node's two sides: the node (connections, routing,
//! the fleet, the bucket) and its cells (objects, sessions, their storage).
//! The cells will run in a child process (plan.md, the second wall); until
//! then both sides share a loop, and each crossing is one of these, encoded
//! and decoded as it will be on the socket between them (`encode`,
//! `decode`). The node will read what a child that runs anyone's code
//! wrote, so decoding never panics and never trusts a length: it's `None`
//! for anything malformed.

use crate::app::{ForCells, Pin, Proc, Prog, Sleep};
use crate::engine::Kind;
use crate::fleet::Scope;
use crate::host::WsOut;
use crate::http::Origin;
use crate::own::{Act, ReqId};
use std::time::Duration;

/// Where an answer goes on the cells' side: a parked cell, or an event
/// cell's ticket (instance, generation, ticket).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CallTo {
    Park(u64),
    Ev(usize, u64, u64),
}

/// A request the cells start themselves.
#[derive(Debug, PartialEq)]
pub enum Asked {
    /// An object's beat (`tick_every`).
    Tick,
    /// An alarm firing (due at `at`), on its `tries`-th retry.
    Alarm { at: i64, tries: u32 },
    /// A socket in an object's list is gone without a close (1006).
    Lost,
}

/// A database call a cell makes (`pg.rs`).
#[derive(Debug, PartialEq)]
pub enum DbAsk {
    /// Lease a connection, for a handle (`pg_open`).
    Open(Vec<u8>),
    /// A query on the binding it names (`pg_query` with no handle).
    Query(Vec<u8>),
    /// A query on a handle the cell may use (the cells checked).
    QueryOn(u64, Vec<u8>),
}

/// What a database call, or a `call`, came back with.
#[derive(Debug, PartialEq)]
pub enum Back {
    Answer(Vec<u8>),
    /// A handle, and how long it may sit unused (its pool's
    /// `session_idle_ms`) before the cells give it back.
    Handle(u64, Duration),
    Failed(String),
}

/// From the node to the cells. Both sides queue what they send, and the
/// loop hands it over between turns: never inside the other side's turn.
#[derive(Debug, PartialEq)]
pub enum Down {
    /// A request for object or session `name`. `new`: one the host doesn't
    /// owe it (not an alarm, a beat, or a close), so a full mailbox refuses it.
    Run { origin: Origin, name: String, req: String, new: bool },
    /// A request for a worker process.
    Worker { origin: Origin, proc: usize, req: String },
    /// The node's memory pressure (`http::pressure`: 1, reclaim; 2,
    /// refuse) and whether its disk is nearly full, when either changes.
    Health { pressure: u64, disk_low: bool },
    /// A socket this loop holds is open to its object or session, or gone.
    SockOpen(u64),
    SockGone(u64),
    /// A send to `name`'s socket found it gone without a close (1006).
    SockLost { name: String, sock: u64 },
    /// `name`'s close of its socket went out: it hears nothing more of it.
    SockClosed { name: String, sock: u64 },
    /// A database call, or a `call`, is back.
    Back { to: CallTo, back: Back },
    /// The connection a waiting request came on is gone: skip it.
    Cancel(u64),
    /// Fleet mode: seal `name` for activation `act` (its committed changes
    /// since the last seal), with an image under `image` if asked.
    Seal { name: String, act: Act, image: Option<String> },
    /// Fleet mode: `name` again, from its segments (or why not), for `act`;
    /// and an image of it as restored, under `image`, if asked.
    Restore { name: String, act: Act, segs: Result<Vec<Vec<u8>>, String>, image: Option<String> },
    /// Fleet mode: `name` lives elsewhere now: its file goes too.
    Drop { name: String },
    /// A new socket process session, `name`, for socket `sock` (its
    /// handshake comes next, as a request).
    Session { name: String, sock: u64, proc: usize },
    /// A deploy: the cells load its new programs, and take it.
    Deploy(ForCells),
    /// Someone asked for the stats page: the cells' counts, please.
    Stats,
}

/// From the cells to the node. A request the node handed the cells
/// carries a token (`http::Origin::Token`), and its answer goes back with it.
#[derive(Debug, PartialEq)]
pub enum Up {
    Answer { token: u64, reply: (u16, String) },
    /// A cell's `call` (its answer comes back to `to`) or `send` (`None`),
    /// from process `from`: the node routes it, and the manifest's
    /// `callers` decide.
    Call { to: Option<CallTo>, from: usize, msg: Vec<u8> },
    /// Sends on `name`'s sockets.
    Ws { name: String, out: Vec<WsOut> },
    /// Fleet mode: the core's request `req` ran on `name` and committed (or
    /// didn't): its answer, what it wrote (the alarm its storage holds
    /// now) if it wrote, the sends that wait with the answer, and the
    /// answers of the rest of its batch.
    Ran { name: String, req: ReqId, reply: (u16, String), wrote: Option<Option<i64>>, ws: Vec<WsOut>, rest: Vec<(ReqId, (u16, String))> },
    /// An alarm request found nothing due: done, and nothing changed.
    Skipped { name: String, req: ReqId },
    /// The seal: its changes (or why it failed: the object's gone), and
    /// the image, if asked and taken.
    Sealed { name: String, act: Act, changes: Result<Vec<u8>, String>, image: Option<(String, Vec<u8>)> },
    /// The restore: done, with the alarm its storage holds, or failed. And
    /// the image, if asked and made.
    Restored { name: String, act: Act, alarm: Option<Option<i64>>, image: Option<(String, Vec<u8>)> },
    /// A request the cells start, routed like any other.
    Submit { from: Asked, name: String, req: String },
    /// Fleet mode: objects idle long enough to let go, if the node agrees.
    Idle(Vec<String>),
    /// Fleet mode: `name` holds a database session or has a beat (`on`),
    /// or no longer does: a game in play isn't moved.
    Anchor { name: String, on: bool },
    /// As many sessions wait to wake as may be at work (`max_awake`), or
    /// no longer: the node takes no new connections while it's so.
    Behind(bool),
    /// A database call, for a cell of process `proc`.
    Db { to: CallTo, proc: usize, ask: DbAsk },
    /// A handle is done with: its connection goes back.
    DbClose(u64),
    /// The cells' counts, as asked (`Down::Stats`).
    Stats(crate::cells::Counts),
}

/// The most a message from the cells may be: a seal's changes, a restore's
/// image, a stats page. Past it, the cells are broken or hostile.
pub const MAX_UP: usize = 256 << 20;

/// One end of the line, on a socket: messages out, each framed (its length,
/// then it), sent as the socket takes them; and bytes in, handed on a whole
/// message at a time.
pub struct End {
    pub s: mio::net::UnixStream,
    out: Vec<u8>,
    sent: usize,
    /// Bytes in: read up to `end`, handed on up to `read`. All of it is
    /// initialized (zeroed once, as it grows), so a read goes straight in,
    /// with no copy and no zeroing each time.
    inb: Vec<u8>,
    end: usize,
    read: usize,
    /// What `take_whole` handed out last, given back (`recycle`) to be the
    /// next `inb`: two buffers that keep their room, not a new one a round.
    spare: Vec<u8>,
    /// The most a message coming in may be.
    max: usize,
}

impl End {
    pub fn new(s: mio::net::UnixStream, max: usize) -> End {
        End { s, out: vec![], sent: 0, inb: vec![], end: 0, read: 0, spare: vec![], max }
    }

    /// Queue a message: `put` writes it.
    pub fn send(&mut self, put: impl FnOnce(&mut Vec<u8>)) {
        let at = self.out.len();
        self.out.extend_from_slice(&[0; 4]);
        put(&mut self.out);
        let n = u32::try_from(self.out.len() - at - 4).expect("a message under 4 GiB");
        self.out[at..at + 4].copy_from_slice(&n.to_le_bytes());
    }

    /// Send what the socket takes now. `true`: it's all out.
    pub fn flush(&mut self) -> std::io::Result<bool> {
        use std::io::Write;
        while self.sent < self.out.len() {
            match self.s.write(&self.out[self.sent..]) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // What's sent goes, once it's half: under steady load
                    // the socket may never take it all, and kept, it grew
                    // without end (a 1 GB node ran out in seconds). Moved
                    // on every short write, what's left was copied over and
                    // over (27% of a loaded node's time).
                    if self.sent * 2 >= self.out.len() {
                        self.out.drain(..self.sent);
                        self.sent = 0;
                    }
                    return Ok(false);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.out.clear();
        self.sent = 0;
        Ok(true)
    }

    pub fn pending(&self) -> bool {
        self.sent < self.out.len()
    }

    /// Read what's there. `false`: the other end has closed. `closed`: the
    /// wake-up says it may have (mio's `is_read_closed`), so read on to the
    /// end, as a short read doesn't say so.
    pub fn fill(&mut self, closed: bool) -> std::io::Result<bool> {
        use std::io::Read;
        loop {
            if self.end == self.inb.len() {
                self.inb.resize((self.inb.len() * 2).max(64 << 10), 0);
            }
            match self.s.read(&mut self.inb[self.end..]) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.end += n;
                    // Short: the socket is empty, and what comes next wakes
                    // the loop again (mio's readiness is edge-triggered,
                    // and a stream socket's every arrival is an edge, its
                    // close too). One read a round, not one more to hear it
                    // would block; unless it closed with this edge.
                    if self.end < self.inb.len() && !closed {
                        return Ok(true);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// The next whole message read, decoded where it lies, if there is
    /// one. An `Err` if one says it's over `max`, or doesn't decode.
    pub fn next<T>(&mut self, decode: impl FnOnce(&[u8]) -> Option<T>) -> std::io::Result<Option<T>> {
        let rest = &self.inb[self.read..self.end];
        let Some(n) = rest.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize) else { return Ok(self.compact()) };
        if n > self.max {
            return Err(std::io::Error::other(format!("a message of {n} bytes on the line")));
        }
        let Some(m) = rest.get(4..4 + n) else { return Ok(self.compact()) };
        let m = decode(m).ok_or_else(|| std::io::Error::other("a message on the line that doesn't decode"))?;
        self.read += 4 + n;
        Ok(Some(m))
    }

    /// All the whole messages read so far, as they came (each its length,
    /// then it: `frames` walks them); the rest stays for the next read. An
    /// `Err` if one says it's over `max`.
    pub fn take_whole(&mut self) -> std::io::Result<Whole> {
        let mut at = self.read;
        while let Some(n) = self.inb[..self.end].get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize) {
            if n > self.max {
                return Err(std::io::Error::other(format!("a message of {n} bytes on the line")));
            }
            if self.end < at + 4 + n {
                break;
            }
            at += 4 + n;
        }
        // The part message left over goes to the spare buffer, which
        // becomes `inb`: a new one each round was, past glibc's mmap
        // threshold, a new mapping and its page faults.
        let tail = self.end - at;
        let mut next = std::mem::take(&mut self.spare);
        if next.len() < tail {
            next.resize(tail, 0);
        }
        next[..tail].copy_from_slice(&self.inb[at..self.end]);
        let whole = Whole { buf: std::mem::replace(&mut self.inb, next), from: std::mem::take(&mut self.read), to: at };
        self.end = tail;
        Ok(whole)
    }

    /// Give back what `take_whole` handed out, done with, to use again
    /// (unless a big message grew it: that room goes).
    pub fn recycle(&mut self, w: Whole) {
        if w.buf.len() <= 4 << 20 {
            self.spare = w.buf;
        }
    }

    /// Drop what's been read, once nothing whole is left (and the room a
    /// big message made, once it's read).
    fn compact<T>(&mut self) -> Option<T> {
        self.inb.copy_within(self.read..self.end, 0);
        self.end -= self.read;
        self.read = 0;
        if self.end == 0 && self.inb.len() > 4 << 20 {
            self.inb = vec![];
        }
        None
    }
}

/// The whole messages `End::take_whole` found, where they were read.
pub struct Whole {
    buf: Vec<u8>,
    from: usize,
    to: usize,
}

impl std::ops::Deref for Whole {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.buf[self.from..self.to]
    }
}

/// What a child process of cells is told first (`cells::child`), before
/// any message: who it must be, and what it needs to start.
#[derive(Debug, PartialEq)]
pub struct Start {
    /// The parent's build (`cells::build_id`): a child of another build
    /// (the binary changed under a running node) mustn't run.
    pub build: String,
    /// The engine (`Engine::child`), and where its compiled modules are
    /// (`COMBD_AOT_DIR`), if anywhere.
    pub engine: String,
    pub aot_dir: Option<std::path::PathBuf>,
    pub set: crate::cells::Settings,
    pub sync_delay: Duration,
    /// Lock the child down before it loads a module (`COMBD_LOCKDOWN`, on
    /// unless `off`): landlock and seccomp, `combd_wall`.
    pub lock: bool,
}

impl Start {
    pub fn encode(&self, b: &mut Vec<u8>) {
        use std::os::unix::ffi::OsStrExt;
        let mut w = W(b);
        w.str(&self.build);
        w.str(&self.engine);
        w.opt(&self.aot_dir, |w, d| w.bytes(d.as_os_str().as_bytes()));
        let s = &self.set;
        w.bytes(s.data.as_os_str().as_bytes());
        w.u64(s.threads as u64);
        w.dur(s.idle);
        w.u64(s.max_awake as u64);
        w.u64(s.max_objects as u64);
        w.list(&s.environ, |w, v| w.bytes(v));
        w.opt(&s.node, |w, n| w.u32(*n));
        w.dur(self.sync_delay);
        w.bool(self.lock);
    }

    pub fn decode(b: &[u8]) -> Option<Start> {
        use std::os::unix::ffi::OsStrExt;
        let path = |r: &mut Rd| Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&r.bytes()?)));
        let mut r = Rd(b);
        let m = Start {
            build: r.str()?,
            engine: r.str()?,
            aot_dir: r.opt(path)?,
            set: crate::cells::Settings {
                data: path(&mut r)?,
                threads: r.usize()?,
                idle: r.dur()?,
                max_awake: r.usize()?,
                max_objects: r.usize()?,
                environ: std::sync::Arc::new(r.list(Rd::bytes)?),
                node: r.opt(Rd::u32)?,
            },
            sync_delay: r.dur()?,
            lock: r.bool()?,
        };
        r.0.is_empty().then_some(m)
    }
}

/// On the control socket (`cells::Link`), outside the lines: the node asks
/// whether a deploy will go, and the cells, which load programs, answer.
#[derive(Debug, PartialEq)]
pub enum Ctl {
    /// All of a candidate `App` (`from` 0): does it load, and start?
    Check(ForCells),
}

impl Ctl {
    pub fn encode(&self, b: &mut Vec<u8>) {
        let mut w = W(b);
        match self {
            Ctl::Check(d) => {
                w.u8(0);
                w.for_cells(d);
            }
        }
    }

    pub fn decode(b: &[u8]) -> Option<Ctl> {
        let mut r = Rd(b);
        let m = match r.u8()? {
            0 => Ctl::Check(r.for_cells()?),
            _ => return None,
        };
        r.0.is_empty().then_some(m)
    }

    pub fn encode_answer(a: &Result<(), String>, b: &mut Vec<u8>) {
        W(b).result(a, |_, ()| {});
    }

    pub fn decode_answer(b: &[u8]) -> Option<Result<(), String>> {
        let mut r = Rd(b);
        let a = r.result(|_| Some(()))?;
        r.0.is_empty().then_some(a)
    }
}

/// The messages in what `End::take_whole` gave.
pub fn frames(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let n = u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
        let m = b.get(at + 4..at + 4 + n)?;
        at += 4 + n;
        Some(m)
    })
}

impl Down {
    pub fn encode(&self, b: &mut Vec<u8>) {
        let mut w = W(b);
        match self {
            Down::Run { origin, name, req, new } => {
                w.u8(0);
                w.origin(origin);
                w.str(name);
                w.str(req);
                w.bool(*new);
            }
            Down::Worker { origin, proc, req } => {
                w.u8(1);
                w.origin(origin);
                w.u64(*proc as u64);
                w.str(req);
            }
            Down::Health { pressure, disk_low } => {
                w.u8(4);
                w.u64(*pressure);
                w.bool(*disk_low);
            }
            Down::SockOpen(sock) => {
                w.u8(5);
                w.u64(*sock);
            }
            Down::SockGone(sock) => {
                w.u8(6);
                w.u64(*sock);
            }
            Down::SockLost { name, sock } => {
                w.u8(7);
                w.str(name);
                w.u64(*sock);
            }
            Down::SockClosed { name, sock } => {
                w.u8(8);
                w.str(name);
                w.u64(*sock);
            }
            Down::Back { to, back } => {
                w.u8(9);
                w.call_to(to);
                w.back(back);
            }
            Down::Cancel(token) => {
                w.u8(10);
                w.u64(*token);
            }
            Down::Seal { name, act, image } => {
                w.u8(11);
                w.str(name);
                w.u64(*act);
                w.opt(image, |w, k| w.str(k));
            }
            Down::Restore { name, act, segs, image } => {
                w.u8(12);
                w.str(name);
                w.u64(*act);
                w.result(segs, |w, segs| w.list(segs, |w, seg| w.bytes(seg)));
                w.opt(image, |w, k| w.str(k));
            }
            Down::Drop { name } => {
                w.u8(13);
                w.str(name);
            }
            Down::Session { name, sock, proc } => {
                w.u8(14);
                w.str(name);
                w.u64(*sock);
                w.u64(*proc as u64);
            }
            Down::Deploy(d) => {
                w.u8(15);
                w.for_cells(d);
            }
            Down::Stats => w.u8(16),
        }
    }

    pub fn decode(b: &[u8]) -> Option<Down> {
        let mut r = Rd(b);
        let m = match r.u8()? {
            0 => Down::Run { origin: r.origin()?, name: r.str()?, req: r.str()?, new: r.bool()? },
            1 => Down::Worker { origin: r.origin()?, proc: r.usize()?, req: r.str()? },
            4 => Down::Health { pressure: r.u64()?, disk_low: r.bool()? },
            5 => Down::SockOpen(r.u64()?),
            6 => Down::SockGone(r.u64()?),
            7 => Down::SockLost { name: r.str()?, sock: r.u64()? },
            8 => Down::SockClosed { name: r.str()?, sock: r.u64()? },
            9 => Down::Back { to: r.call_to()?, back: r.back()? },
            10 => Down::Cancel(r.u64()?),
            11 => Down::Seal { name: r.str()?, act: r.u64()?, image: r.opt(Rd::str)? },
            12 => Down::Restore { name: r.str()?, act: r.u64()?, segs: r.result(|r| r.list(Rd::bytes))?, image: r.opt(Rd::str)? },
            13 => Down::Drop { name: r.str()? },
            14 => Down::Session { name: r.str()?, sock: r.u64()?, proc: r.usize()? },
            15 => Down::Deploy(r.for_cells()?),
            16 => Down::Stats,
            _ => return None,
        };
        r.0.is_empty().then_some(m)
    }
}

impl Up {
    /// An `Answer`'s token and reply, read where they lie (no copy), or
    /// `None` if it's another message (or a malformed one: `decode` says).
    /// The body as bytes: one for a connection goes out as it is, and one
    /// that must be text is checked where it's made a `String`.
    pub fn answer(b: &[u8]) -> Option<(u64, u16, &[u8])> {
        let mut r = Rd(b);
        if r.u8()? != 0 {
            return None;
        }
        let (token, status) = (r.u64()?, r.u16()?);
        let n = r.u32()? as usize;
        let body = r.take(n)?;
        r.0.is_empty().then_some((token, status, body))
    }

    pub fn encode(&self, b: &mut Vec<u8>) {
        let mut w = W(b);
        match self {
            Up::Answer { token, reply } => {
                w.u8(0);
                w.u64(*token);
                w.reply(reply);
            }
            Up::Call { to, from, msg } => {
                w.u8(1);
                w.opt(to, |w, t| w.call_to(t));
                w.u64(*from as u64);
                w.bytes(msg);
            }
            Up::Ws { name, out } => {
                w.u8(2);
                w.str(name);
                w.list(out, |w, o| w.ws(o));
            }
            Up::Ran { name, req, reply, wrote, ws, rest } => {
                w.u8(3);
                w.str(name);
                w.u64(*req);
                w.reply(reply);
                w.opt(wrote, |w, a| w.opt(a, |w, at| w.u64(*at as u64)));
                w.list(ws, |w, o| w.ws(o));
                w.list(rest, |w, (r, reply)| {
                    w.u64(*r);
                    w.reply(reply);
                });
            }
            Up::Skipped { name, req } => {
                w.u8(4);
                w.str(name);
                w.u64(*req);
            }
            Up::Sealed { name, act, changes, image } => {
                w.u8(5);
                w.str(name);
                w.u64(*act);
                w.result(changes, |w, c| w.bytes(c));
                w.opt(image, |w, (k, e)| {
                    w.str(k);
                    w.bytes(e);
                });
            }
            Up::Restored { name, act, alarm, image } => {
                w.u8(6);
                w.str(name);
                w.u64(*act);
                w.opt(alarm, |w, a| w.opt(a, |w, at| w.u64(*at as u64)));
                w.opt(image, |w, (k, e)| {
                    w.str(k);
                    w.bytes(e);
                });
            }
            Up::Submit { from, name, req } => {
                w.u8(7);
                match from {
                    Asked::Tick => w.u8(0),
                    Asked::Alarm { at, tries } => {
                        w.u8(1);
                        w.u64(*at as u64);
                        w.u32(*tries);
                    }
                    Asked::Lost => w.u8(2),
                }
                w.str(name);
                w.str(req);
            }
            Up::Idle(names) => {
                w.u8(8);
                w.list(names, |w, n| w.str(n));
            }
            Up::Anchor { name, on } => {
                w.u8(9);
                w.str(name);
                w.bool(*on);
            }
            Up::Behind(b) => {
                w.u8(10);
                w.bool(*b);
            }
            Up::Db { to, proc, ask } => {
                w.u8(11);
                w.call_to(to);
                w.u64(*proc as u64);
                match ask {
                    DbAsk::Open(r) => {
                        w.u8(0);
                        w.bytes(r);
                    }
                    DbAsk::Query(r) => {
                        w.u8(1);
                        w.bytes(r);
                    }
                    DbAsk::QueryOn(h, r) => {
                        w.u8(2);
                        w.u64(*h);
                        w.bytes(r);
                    }
                }
            }
            Up::DbClose(h) => {
                w.u8(12);
                w.u64(*h);
            }
            Up::Stats(c) => {
                w.u8(13);
                w.list(&c.numbers(), |w, n| w.u64(*n));
                w.list(&c.procs, |w, (name, us, slices, kind)| {
                    w.str(name);
                    w.u64(*us);
                    w.u64(*slices);
                    w.kind(*kind);
                });
            }
        }
    }

    pub fn decode(b: &[u8]) -> Option<Up> {
        let mut r = Rd(b);
        let alarm = |r: &mut Rd| r.opt(|r| r.u64().map(|at| at as i64));
        let image = |r: &mut Rd| r.opt(|r| Some((r.str()?, r.bytes()?)));
        let m = match r.u8()? {
            0 => Up::Answer { token: r.u64()?, reply: r.reply()? },
            1 => Up::Call { to: r.opt(Rd::call_to)?, from: r.usize()?, msg: r.bytes()? },
            2 => Up::Ws { name: r.str()?, out: r.list(Rd::ws)? },
            3 => Up::Ran { name: r.str()?, req: r.u64()?, reply: r.reply()?, wrote: r.opt(alarm)?, ws: r.list(Rd::ws)?, rest: r.list(|r| Some((r.u64()?, r.reply()?)))? },
            4 => Up::Skipped { name: r.str()?, req: r.u64()? },
            5 => Up::Sealed { name: r.str()?, act: r.u64()?, changes: r.result(Rd::bytes)?, image: image(&mut r)? },
            6 => Up::Restored { name: r.str()?, act: r.u64()?, alarm: r.opt(alarm)?, image: image(&mut r)? },
            7 => {
                let from = match r.u8()? {
                    0 => Asked::Tick,
                    1 => Asked::Alarm { at: r.u64()? as i64, tries: r.u32()? },
                    2 => Asked::Lost,
                    _ => return None,
                };
                Up::Submit { from, name: r.str()?, req: r.str()? }
            }
            8 => Up::Idle(r.list(Rd::str)?),
            9 => Up::Anchor { name: r.str()?, on: r.bool()? },
            10 => Up::Behind(r.bool()?),
            11 => {
                let (to, proc) = (r.call_to()?, r.usize()?);
                let ask = match r.u8()? {
                    0 => DbAsk::Open(r.bytes()?),
                    1 => DbAsk::Query(r.bytes()?),
                    2 => DbAsk::QueryOn(r.u64()?, r.bytes()?),
                    _ => return None,
                };
                Up::Db { to, proc, ask }
            }
            12 => Up::DbClose(r.u64()?),
            13 => {
                let n = r.list(Rd::u64)?;
                Up::Stats(crate::cells::Counts::from_numbers(&n, r.list(|r| Some((r.str()?, r.u64()?, r.u64()?, r.kind()?)))?)?)
            }
            _ => return None,
        };
        r.0.is_empty().then_some(m)
    }
}

/// Writes little-endian fields: a length before bytes, and a tag before
/// each choice.
struct W<'a>(&'a mut Vec<u8>);

impl W<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(u32::try_from(b.len()).expect("a message's part under 4 GiB"));
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    fn opt<T>(&mut self, v: &Option<T>, f: impl FnOnce(&mut Self, &T)) {
        match v {
            None => self.u8(0),
            Some(x) => {
                self.u8(1);
                f(self, x);
            }
        }
    }
    fn result<T>(&mut self, v: &Result<T, String>, f: impl FnOnce(&mut Self, &T)) {
        match v {
            Ok(x) => {
                self.u8(0);
                f(self, x);
            }
            Err(e) => {
                self.u8(1);
                self.str(e);
            }
        }
    }
    fn list<T>(&mut self, v: &[T], f: impl Fn(&mut Self, &T)) {
        self.u32(u32::try_from(v.len()).expect("under 4 billion"));
        for x in v {
            f(self, x);
        }
    }
    fn reply(&mut self, (status, body): &(u16, String)) {
        self.u16(*status);
        self.str(body);
    }
    /// Only the kinds the cells see: the node keeps the rest (`tok`).
    fn origin(&mut self, o: &Origin) {
        match o {
            Origin::Token(t) => {
                self.u8(0);
                self.u64(*t);
            }
            Origin::Fleet(r) => {
                self.u8(1);
                self.u64(*r);
            }
            Origin::Alarm { at, tries } => {
                self.u8(2);
                self.u64(*at as u64);
                self.u32(*tries);
            }
            Origin::Tick => self.u8(3),
            Origin::Discard => self.u8(4),
            _ => {
                #[cfg(test)]
                panic!("a node's origin on the line: {o:?}");
                #[cfg(not(test))]
                self.u8(4)
            }
        }
    }
    fn call_to(&mut self, t: &CallTo) {
        match t {
            CallTo::Park(p) => {
                self.u8(0);
                self.u64(*p);
            }
            CallTo::Ev(i, gen, ticket) => {
                self.u8(1);
                self.u64(*i as u64);
                self.u64(*gen);
                self.u64(*ticket);
            }
        }
    }
    fn back(&mut self, b: &Back) {
        match b {
            Back::Answer(a) => {
                self.u8(0);
                self.bytes(a);
            }
            Back::Handle(h, idle) => {
                self.u8(1);
                self.u64(*h);
                self.dur(*idle);
            }
            Back::Failed(e) => {
                self.u8(2);
                self.str(e);
            }
        }
    }
    fn for_cells(&mut self, d: &ForCells) {
        self.u64(d.from as u64);
        self.list(&d.progs, |w, p| {
            w.bytes(&p.wasm);
            w.opt(&p.kind, |w, k| w.kind(*k));
            w.str(&p.what);
        });
        self.list(&d.procs, |w, p| w.proc(p));
        self.list(&d.tenants, |w, (org, app, env)| {
            w.str(org);
            w.str(app);
            w.list(env, |w, v| w.bytes(v));
        });
        self.bool(d.legacy);
        self.list(&d.gone, |w, t| w.u64(*t as u64));
    }
    fn dur(&mut self, d: Duration) {
        self.u64(d.as_secs());
        self.u32(d.subsec_nanos());
    }
    fn kind(&mut self, k: Kind) {
        self.u8(matches!(k, Kind::Compiled) as u8);
    }
    fn proc(&mut self, p: &Proc) {
        self.str(&p.name);
        self.bool(p.retired);
        self.u64(p.tenant as u64);
        self.u64(p.prog as u64);
        self.bool(p.durable);
        self.bool(p.socket);
        self.bool(p.fresh);
        self.dur(p.hibernate);
        self.u8(match p.sleep {
            Sleep::Auto => 0,
            Sleep::App => 1,
            Sleep::Off => 2,
        });
        self.bool(p.wake_on_close);
        self.u64(p.state_max as u64);
        self.u64(p.mailbox as u64);
        self.u64(p.batch as u64);
        self.opt(&p.spread, |w, s| {
            w.u8(match s {
                Scope::Zone => 0,
                Scope::Region => 1,
                Scope::Fleet => 2,
            })
        });
        self.bool(p.nearest);
        self.opt(&p.pin, |w, pin| {
            w.list(&pin.places, |w, s| w.str(s));
            w.bool(pin.fallback);
        });
        self.opt(&p.memory_pages, |w, n| w.u32(*n));
        self.kind(p.engine);
        self.u64(p.max_concurrent as u64);
        self.dur(p.timeout);
        self.list(&p.callers, |w, c| w.u64(*c as u64));
        self.list(&p.owns, |w, o| w.str(o));
    }
    fn ws(&mut self, o: &WsOut) {
        match o {
            WsOut::Send(id, m) => {
                self.u8(0);
                self.u64(*id);
                self.str(m);
            }
            WsOut::Binary(id, b) => {
                self.u8(1);
                self.u64(*id);
                self.bytes(b);
            }
            WsOut::Close(id, code) => {
                self.u8(2);
                self.u64(*id);
                self.u16(*code);
            }
            WsOut::Message(m) => {
                self.u8(3);
                self.bytes(m);
            }
        }
    }
}

/// Reads them back: `None` when the bytes run short or don't make sense.
struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn usize(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?).ok()
    }
    fn opt<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<Option<T>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(f(self)?)),
            _ => None,
        }
    }
    fn result<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<Result<T, String>> {
        match self.u8()? {
            0 => Some(Ok(f(self)?)),
            1 => Some(Err(self.str()?)),
            _ => None,
        }
    }
    /// Each item takes at least a byte, so a count can't outrun the bytes:
    /// nothing is set aside for it ahead.
    fn list<T>(&mut self, f: impl Fn(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let n = self.u32()?;
        let mut v = vec![];
        for _ in 0..n {
            v.push(f(self)?);
        }
        Some(v)
    }
    fn reply(&mut self) -> Option<(u16, String)> {
        Some((self.u16()?, self.str()?))
    }
    fn origin(&mut self) -> Option<Origin> {
        Some(match self.u8()? {
            0 => Origin::Token(self.u64()?),
            1 => Origin::Fleet(self.u64()?),
            2 => Origin::Alarm { at: self.u64()? as i64, tries: self.u32()? },
            3 => Origin::Tick,
            4 => Origin::Discard,
            _ => return None,
        })
    }
    fn call_to(&mut self) -> Option<CallTo> {
        Some(match self.u8()? {
            0 => CallTo::Park(self.u64()?),
            1 => CallTo::Ev(self.usize()?, self.u64()?, self.u64()?),
            _ => return None,
        })
    }
    fn back(&mut self) -> Option<Back> {
        Some(match self.u8()? {
            0 => Back::Answer(self.bytes()?),
            1 => Back::Handle(self.u64()?, self.dur()?),
            2 => Back::Failed(self.str()?),
            _ => return None,
        })
    }
    fn for_cells(&mut self) -> Option<ForCells> {
        Some(ForCells {
            from: self.usize()?,
            progs: self.list(|r| Some(Prog { wasm: r.bytes()?.into(), kind: r.opt(Rd::kind)?, what: r.str()? }))?,
            procs: self.list(Rd::proc)?,
            tenants: self.list(|r| Some((r.str()?, r.str()?, std::sync::Arc::new(r.list(Rd::bytes)?))))?,
            legacy: self.bool()?,
            gone: self.list(Rd::usize)?,
        })
    }
    fn dur(&mut self) -> Option<Duration> {
        let (secs, nanos) = (self.u64()?, self.u32()?);
        // `Duration::new` panics past a second's nanoseconds.
        Some(Duration::new(secs, (nanos < 1_000_000_000).then_some(nanos)?))
    }
    fn kind(&mut self) -> Option<Kind> {
        Some(if self.bool()? { Kind::Compiled } else { Kind::Interpreted })
    }
    fn proc(&mut self) -> Option<Proc> {
        Some(Proc {
            name: self.str()?,
            retired: self.bool()?,
            tenant: self.usize()?,
            prog: self.usize()?,
            durable: self.bool()?,
            socket: self.bool()?,
            fresh: self.bool()?,
            hibernate: self.dur()?,
            sleep: match self.u8()? {
                0 => Sleep::Auto,
                1 => Sleep::App,
                2 => Sleep::Off,
                _ => return None,
            },
            wake_on_close: self.bool()?,
            state_max: self.usize()?,
            mailbox: self.usize()?,
            batch: self.usize()?,
            spread: self.opt(|r| match r.u8()? {
                0 => Some(Scope::Zone),
                1 => Some(Scope::Region),
                2 => Some(Scope::Fleet),
                _ => None,
            })?,
            nearest: self.bool()?,
            pin: self.opt(|r| Some(Pin { places: r.list(Rd::str)?, fallback: r.bool()? }))?,
            memory_pages: self.opt(Rd::u32)?,
            engine: self.kind()?,
            max_concurrent: self.usize()?,
            timeout: self.dur()?,
            callers: self.list(Rd::usize)?,
            owns: self.list(Rd::str)?,
        })
    }
    fn ws(&mut self) -> Option<WsOut> {
        Some(match self.u8()? {
            0 => WsOut::Send(self.u64()?, self.str()?),
            1 => WsOut::Binary(self.u64()?, self.bytes()?),
            2 => WsOut::Close(self.u64()?, self.u16()?),
            3 => WsOut::Message(self.bytes()?),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn downs() -> Vec<Down> {
        vec![
            Down::Run { origin: Origin::Token(7), name: "acme/app/room/r1".into(), req: "do POST /x é".into(), new: true },
            Down::Run { origin: Origin::Alarm { at: -5, tries: 3 }, name: String::new(), req: "do ALARM / 3".into(), new: false },
            Down::Run { origin: Origin::Fleet(u64::MAX), name: "n".into(), req: "r".into(), new: true },
            Down::Run { origin: Origin::Tick, name: "n".into(), req: "do TICK / 1".into(), new: false },
            Down::Worker { origin: Origin::Discard, proc: 3, req: "api GET /".into() },
            Down::Health { pressure: 2, disk_low: true },
            Down::SockOpen(1 << 60),
            Down::SockGone(0),
            Down::SockLost { name: "s".into(), sock: 4 },
            Down::SockClosed { name: "s".into(), sock: 5 },
            Down::Back { to: CallTo::Park(1), back: Back::Answer(vec![0, 1, 255]) },
            Down::Back { to: CallTo::Ev(2, 3, 4), back: Back::Handle(77, Duration::from_millis(1500)) },
            Down::Back { to: CallTo::Park(0), back: Back::Failed("no".into()) },
            Down::Cancel(11),
            Down::Seal { name: "o".into(), act: 3, image: Some("k".into()) },
            Down::Seal { name: "o".into(), act: 3, image: None },
            Down::Restore { name: "o".into(), act: 4, segs: Ok(vec![vec![], vec![1, 2]]), image: None },
            Down::Restore { name: "o".into(), act: 4, segs: Err("gone".into()), image: Some("k".into()) },
            Down::Drop { name: "o".into() },
            Down::Session { name: "a/b/c/~9".into(), sock: 9, proc: 2 },
            Down::Deploy(ForCells { from: 1, progs: vec![], procs: vec![], tenants: vec![], legacy: true, gone: vec![] }),
            Down::Deploy(deploy()),
            Down::Stats,
        ]
    }

    fn deploy() -> ForCells {
        let proc = |name: &str, socket: bool| Proc {
            name: name.into(),
            retired: false,
            tenant: 0,
            prog: 1,
            durable: !socket,
            socket,
            fresh: false,
            hibernate: Duration::from_millis(2000),
            sleep: Sleep::App,
            wake_on_close: socket,
            state_max: 1024,
            mailbox: usize::MAX,
            batch: 1,
            spread: Some(Scope::Fleet),
            nearest: false,
            pin: Some(Pin { places: vec!["fra1".into(), "ams3".into()], fallback: false }),
            memory_pages: Some(8),
            engine: Kind::Compiled,
            max_concurrent: 4,
            timeout: Duration::from_secs(30),
            callers: vec![1],
            owns: vec!["main".into()],
        };
        ForCells {
            from: 0,
            progs: vec![Prog { wasm: b"\0asm".to_vec().into(), kind: None, what: "the module".into() }, Prog { wasm: vec![1; 300].into(), kind: Some(Kind::Compiled), what: "acme/app: modules/x.wasm".into() }],
            procs: vec![proc("room", false), proc("ws", true)],
            tenants: vec![("acme".into(), "app".into(), std::sync::Arc::new(vec![b"A=1".to_vec()]))],
            legacy: false,
            gone: vec![0],
        }
    }

    fn ups() -> Vec<Up> {
        vec![
            Up::Answer { token: 7, reply: (200, "{}".into()) },
            Up::Call { to: Some(CallTo::Ev(1, 2, 3)), from: 4, msg: b"{\"to\":\"x\"}".to_vec() },
            Up::Call { to: None, from: 0, msg: vec![] },
            Up::Ws { name: "o".into(), out: vec![WsOut::Send(1, "hi".into()), WsOut::Binary(2, vec![9]), WsOut::Close(3, 1011), WsOut::Message(vec![1])] },
            Up::Ran { name: "o".into(), req: 5, reply: (503, "x".into()), wrote: Some(Some(-1)), ws: vec![], rest: vec![(6, (200, "a".into()))] },
            Up::Ran { name: "o".into(), req: 5, reply: (200, String::new()), wrote: Some(None), ws: vec![WsOut::Send(1, "m".into())], rest: vec![] },
            Up::Ran { name: "o".into(), req: 5, reply: (200, String::new()), wrote: None, ws: vec![], rest: vec![] },
            Up::Skipped { name: "o".into(), req: 8 },
            Up::Sealed { name: "o".into(), act: 1, changes: Ok(vec![1, 2, 3]), image: Some(("k".into(), vec![4])) },
            Up::Sealed { name: "o".into(), act: 1, changes: Err("bad".into()), image: None },
            Up::Restored { name: "o".into(), act: 2, alarm: Some(Some(i64::MAX)), image: None },
            Up::Restored { name: "o".into(), act: 2, alarm: None, image: Some(("k".into(), vec![])) },
            Up::Submit { from: Asked::Tick, name: "o".into(), req: "do TICK / 2".into() },
            Up::Submit { from: Asked::Alarm { at: 123, tries: 1 }, name: "o".into(), req: "r".into() },
            Up::Submit { from: Asked::Lost, name: "o".into(), req: "r".into() },
            Up::Idle(vec!["a".into(), "b".into()]),
            Up::Anchor { name: "o".into(), on: true },
            Up::Behind(false),
            Up::Db { to: CallTo::Park(3), proc: 1, ask: DbAsk::Open(b"{}".to_vec()) },
            Up::Db { to: CallTo::Ev(0, 0, 0), proc: 1, ask: DbAsk::Query(vec![]) },
            Up::Db { to: CallTo::Park(3), proc: 1, ask: DbAsk::QueryOn(5, vec![1]) },
            Up::DbClose(6),
            Up::Stats(crate::cells::Counts { objects_live: 3, fsync_max_us: u64::MAX, procs: vec![("a/b/c".into(), 1, 2, Kind::Compiled)], ..Default::default() }),
        ]
    }

    /// Through a socket, in order, whole; and once sent, a message's bytes
    /// go: under steady load the socket may never take all that's queued,
    /// and kept, they grew without end (a 1 GB node ran out in seconds).
    #[test]
    fn the_line_keeps_order_and_lets_go_of_what_it_sent() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        let (mut tx, mut rx) = (End::new(mio::net::UnixStream::from_std(a), usize::MAX), End::new(mio::net::UnixStream::from_std(b), 1 << 20));
        let (mut sent, mut got, mut most, mut most_in) = (0u64, 0u64, 0, 0);
        while got < 10_000 {
            // More whenever the line is short of a few: it's never all out.
            while sent < 10_000 && tx.out.len() - tx.sent < 100_000 {
                let m = Up::Answer { token: sent, reply: (200, "x".repeat(10_000)) };
                tx.send(|b| m.encode(b));
                sent += 1;
            }
            tx.flush().unwrap();
            assert!(tx.sent * 2 <= tx.out.len(), "what's sent is let go: {} of {}", tx.sent, tx.out.len());
            most = most.max(tx.out.capacity());
            assert!(rx.fill(false).unwrap());
            // Half a time as the cells read the line, half as the node does.
            if got % 2 == 0 {
                while let Some(m) = rx.next(Up::decode).unwrap() {
                    assert_eq!(m, Up::Answer { token: got, reply: (200, "x".repeat(10_000)) });
                    got += 1;
                }
            } else {
                let whole = rx.take_whole().unwrap();
                for m in frames(&whole) {
                    assert_eq!(Up::answer(m), Some((got, 200, "x".repeat(10_000).as_bytes())));
                    got += 1;
                }
                rx.recycle(whole);
            }
            most_in = most_in.max(rx.inb.capacity() + rx.spare.capacity());
        }
        assert!(most < 1 << 20, "{most} bytes held to send");
        assert!(most_in < 1 << 21, "{most_in} bytes held to read into");
        // A length past the most the reader takes is refused, not read.
        tx.send(|b| b.extend_from_slice(&[0; 2 << 20]));
        while !tx.flush().unwrap() {
            assert!(rx.fill(false).unwrap());
            if rx.next(Up::decode).is_err() {
                return;
            }
        }
        assert!(rx.fill(false).unwrap());
        assert!(rx.next(Up::decode).is_err());
    }

    /// One read a round stops at a short read, which doesn't say the other
    /// end closed: a message and the close both there before the read, in
    /// one wake-up, must still be heard as both (a child that missed it
    /// would outlive its node).
    #[test]
    fn a_close_right_after_a_message_is_heard() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        let (mut tx, mut rx) = (End::new(mio::net::UnixStream::from_std(a), usize::MAX), End::new(mio::net::UnixStream::from_std(b), usize::MAX));
        tx.send(|b| Up::Behind(true).encode(b));
        assert!(tx.flush().unwrap());
        drop(tx);
        assert!(!rx.fill(true).unwrap(), "closed");
        assert_eq!(rx.next(Up::decode).unwrap(), Some(Up::Behind(true)), "and the message before it");
    }

    #[test]
    fn a_check_and_its_answer_come_back_as_they_went() {
        let c = Ctl::Check(deploy());
        let mut b = vec![];
        c.encode(&mut b);
        assert_eq!(Ctl::decode(&b), Some(c));
        for a in [Ok(()), Err("modules/x.wasm: no".to_string())] {
            let mut b = vec![];
            Ctl::encode_answer(&a, &mut b);
            assert_eq!(Ctl::decode_answer(&b), Some(a));
        }
    }

    #[test]
    fn a_start_comes_back_as_it_went() {
        let s = Start {
            build: "abc".into(),
            engine: "both:16384:/var/lib/combd/builds".into(),
            aot_dir: Some("/aot".into()),
            set: crate::cells::Settings { data: "/data".into(), threads: 2, idle: Duration::from_secs(60), max_awake: 1000, max_objects: 10_000, environ: std::sync::Arc::new(vec![b"A=1".to_vec()]), node: Some(3) },
            sync_delay: Duration::ZERO,
            lock: true,
        };
        let mut b = vec![];
        s.encode(&mut b);
        assert_eq!(Start::decode(&b), Some(s));
        assert_eq!(Start::decode(&b[..b.len() - 1]), None);
    }

    #[test]
    fn every_message_comes_back_as_it_went() {
        for m in downs() {
            let mut b = vec![];
            m.encode(&mut b);
            assert_eq!(Down::decode(&b).as_ref(), Some(&m));
        }
        for m in ups() {
            let mut b = vec![];
            m.encode(&mut b);
            assert_eq!(Up::decode(&b).as_ref(), Some(&m));
            // Read in place, an answer is the same answer, and nothing else is one.
            let read = Up::answer(&b).map(|(token, status, body)| Up::Answer { token, reply: (status, String::from_utf8(body.to_vec()).unwrap()) });
            assert_eq!(read.as_ref(), matches!(m, Up::Answer { .. }).then_some(&m));
        }
    }

    /// What a child sends may be anything: cut short, run on, or noise, it
    /// decodes to `None` or to some message, and never panics.
    #[test]
    fn bad_bytes_never_panic() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut all = vec![];
        for m in downs() {
            let mut b = vec![];
            m.encode(&mut b);
            all.push((b, true));
        }
        for m in ups() {
            let mut b = vec![];
            m.encode(&mut b);
            all.push((b, false));
        }
        // Cut short, or with a byte more: not a message of its kind.
        let mine = |b: &[u8], down: bool| if down { Down::decode(b).is_some() } else { Up::decode(b).is_some() };
        for (b, down) in &all {
            for n in 0..b.len() {
                assert!(!mine(&b[..n], *down), "{b:?} cut to {n}");
            }
            let mut long = b.clone();
            long.push(0);
            assert!(!mine(&long, *down), "{b:?} and a byte");
        }
        let all: Vec<Vec<u8>> = all.into_iter().map(|(b, _)| b).collect();
        for _ in 0..200_000 {
            let mut b = all[rand() as usize % all.len()].clone();
            for _ in 0..1 + rand() % 4 {
                if b.is_empty() {
                    break;
                }
                let i = rand() as usize % b.len();
                b[i] = rand() as u8;
            }
            let _ = (Down::decode(&b), Up::decode(&b));
            let noise: Vec<u8> = (0..rand() % 64).map(|_| rand() as u8).collect();
            let _ = (Down::decode(&noise), Up::decode(&noise));
        }
        // A handle's idle time past a second's nanoseconds is refused, not a panic.
        let mut b = vec![9, 0];
        b.extend_from_slice(&1u64.to_le_bytes());
        b.push(1);
        b.extend_from_slice(&5u64.to_le_bytes());
        b.extend_from_slice(&u64::MAX.to_le_bytes());
        b.extend_from_slice(&2_000_000_000u32.to_le_bytes());
        assert_eq!(Down::decode(&b), None);
    }
}
