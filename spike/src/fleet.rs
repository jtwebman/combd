//! D2 on real machines: the pieces the HTTP front uses when a node runs in
//! a fleet (when `COMBD_S3` is set).
//!
//! - The bucket: the decision core's calls (`own::Call`) done on S3.
//! - The wire: how nodes talk to each other. Long-lived TCP connections
//!   carrying length-prefixed binary frames, so a request that lands on the
//!   wrong node is forwarded to its owner instead of bouncing the client.
//!   ponytail: plain TCP, inside a private network. mTLS before nodes cross
//!   networks.
//!
//! Settings:
//! - `COMBD_NODE`: this node's number.
//! - `COMBD_PEERS`: `0=host:7100,1=host:7100,...`, every node's wire address.
//! - `COMBD_WIRE_PORT`: where this node listens for peers (default 7100).
//! - `COMBD_S3`, `COMBD_S3_KEY`, `COMBD_S3_SECRET`, `COMBD_S3_BUCKET`: the bucket.

use crate::own::{Call, Cond, Node, Res, Val};
use crate::s3::{self, S3};
use std::collections::HashMap;
use std::io::{Read, Write};

pub struct FleetCfg {
    pub node: Node,
    pub s3: S3,
    pub peers: HashMap<Node, String>,
    pub wire_port: u16,
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
        Some(FleetCfg { node, s3, peers, wire_port })
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
    };
    r.unwrap_or_else(|e| {
        eprintln!("bucket: {call:?}: {e}");
        Res::Lost
    })
}

/// Every segment of a restore, in order.
pub fn fetch(s3: &S3, segs: &[String]) -> Result<Vec<Vec<u8>>, String> {
    segs.iter()
        .map(|k| match s3.get(k) {
            Ok(Some((b, _))) => Ok(b),
            Ok(None) => Err(format!("segment {k} is missing")),
            Err(e) => Err(format!("segment {k}: {e}")),
        })
        .collect()
}

/// A frame on the wire.
#[derive(Debug, PartialEq)]
pub enum Frame {
    /// Run this object request on the node that owns it.
    Forward { id: u64, name: String, req: String },
    /// Its answer.
    Reply { id: u64, status: u16, body: String },
}

const MAX_FRAME: usize = 16 << 20;

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
        let mut b = vec![0u8; n];
        r.read_exact(&mut b)?;
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
            _ => Err(bad("unknown frame kind")),
        }
    }

    pub fn write(&self, w: &mut impl Write) -> std::io::Result<()> {
        w.write_all(&self.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_reject_garbage() {
        for f in [
            Frame::Forward { id: 7, name: "room/1".into(), req: "do POST /incr {\"a\":1}".into() },
            Frame::Reply { id: u64::MAX, status: 503, body: String::new() },
        ] {
            let b = f.encode();
            assert_eq!(Frame::read(&mut &b[..]).unwrap(), f);
        }
        assert!(Frame::read(&mut &[0xff, 0xff, 0xff, 0x7f, 1][..]).is_err(), "a huge length must be refused");
        assert!(Frame::read(&mut &[10, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0][..]).is_err(), "an unknown kind must be refused");
    }
}
