//! TLS ended in the kernel (Linux kTLS): after rustls does the handshake,
//! the session's keys go to the socket, and from then on the kernel
//! encrypts what's written and decrypts what's read. The connection keeps
//! no TLS state of its own, which is most of what an idle one costs.
//!
//! This crate is the `unsafe` code for it: the socket options, and sending
//! and receiving the records that aren't data (alerts, a key update). What
//! it relies on: the struct layouts below are `linux/tls.h`'s; `fd` is a
//! connected TCP socket the caller owns; nothing has been read from or
//! written to the socket past what rustls accounted for in the sequence
//! numbers it handed over.

use std::io;
use std::os::fd::RawFd;

/// Which TLS: the kernel needs it for its record format.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Version {
    Tls12,
    Tls13,
}

/// One direction's keys: the record sequence number it's at, and the AEAD.
pub struct Keys<'a> {
    pub seq: u64,
    pub cipher: Cipher<'a>,
}

/// The AEADs the kernel does. `iv` is the full 12 bytes rustls gives.
pub enum Cipher<'a> {
    AesGcm128 { key: &'a [u8], iv: &'a [u8] },
    AesGcm256 { key: &'a [u8], iv: &'a [u8] },
    Chacha20Poly1305 { key: &'a [u8], iv: &'a [u8] },
}

/// Record types (RFC 8446 5.1).
pub const ALERT: u8 = 21;
pub const HANDSHAKE: u8 = 22;
pub const APPLICATION_DATA: u8 = 23;

#[cfg(target_os = "linux")]
mod sys {
    pub const SOL_TLS: libc::c_int = 282;
    pub const TCP_ULP: libc::c_int = 31;
    pub const TLS_TX: libc::c_int = 1;
    pub const TLS_RX: libc::c_int = 2;
    pub const TLS_SET_RECORD_TYPE: libc::c_int = 1;
    pub const TLS_GET_RECORD_TYPE: libc::c_int = 2;
    pub const TLS_1_2_VERSION: u16 = 0x0303;
    pub const TLS_1_3_VERSION: u16 = 0x0304;
    pub const TLS_CIPHER_AES_GCM_128: u16 = 51;
    pub const TLS_CIPHER_AES_GCM_256: u16 = 52;
    pub const TLS_CIPHER_CHACHA20_POLY1305: u16 = 54;

    #[repr(C)]
    pub struct Info {
        pub version: u16,
        pub cipher_type: u16,
    }
    #[repr(C)]
    pub struct AesGcm128 {
        pub info: Info,
        pub iv: [u8; 8],
        pub key: [u8; 16],
        pub salt: [u8; 4],
        pub rec_seq: [u8; 8],
    }
    #[repr(C)]
    pub struct AesGcm256 {
        pub info: Info,
        pub iv: [u8; 8],
        pub key: [u8; 32],
        pub salt: [u8; 4],
        pub rec_seq: [u8; 8],
    }
    #[repr(C)]
    pub struct Chacha20Poly1305 {
        pub info: Info,
        pub iv: [u8; 12],
        pub key: [u8; 32],
        pub rec_seq: [u8; 8],
    }
}

#[cfg(target_os = "linux")]
fn set<T>(fd: RawFd, level: libc::c_int, name: libc::c_int, v: &T) -> io::Result<()> {
    // SAFETY: `v` is a live value of the size we pass; the kernel copies it.
    let r = unsafe { libc::setsockopt(fd, level, name, v as *const T as *const libc::c_void, std::mem::size_of::<T>() as libc::socklen_t) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn direction(fd: RawFd, name: libc::c_int, version: Version, k: &Keys) -> io::Result<()> {
    use sys::*;
    let version = match version {
        Version::Tls12 => TLS_1_2_VERSION,
        Version::Tls13 => TLS_1_3_VERSION,
    };
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "a key or IV of the wrong length");
    let seq = k.seq.to_be_bytes();
    // AES-GCM: the kernel takes the IV's first 4 bytes as the salt and the
    // last 8 as its IV; ChaCha20 takes all 12.
    match k.cipher {
        Cipher::AesGcm128 { key, iv } => {
            let (key, iv): (&[u8; 16], &[u8; 12]) = (key.try_into().map_err(|_| bad())?, iv.try_into().map_err(|_| bad())?);
            let v = AesGcm128 { info: Info { version, cipher_type: TLS_CIPHER_AES_GCM_128 }, iv: iv[4..].try_into().unwrap(), key: *key, salt: iv[..4].try_into().unwrap(), rec_seq: seq };
            set(fd, SOL_TLS, name, &v)
        }
        Cipher::AesGcm256 { key, iv } => {
            let (key, iv): (&[u8; 32], &[u8; 12]) = (key.try_into().map_err(|_| bad())?, iv.try_into().map_err(|_| bad())?);
            let v = AesGcm256 { info: Info { version, cipher_type: TLS_CIPHER_AES_GCM_256 }, iv: iv[4..].try_into().unwrap(), key: *key, salt: iv[..4].try_into().unwrap(), rec_seq: seq };
            set(fd, SOL_TLS, name, &v)
        }
        Cipher::Chacha20Poly1305 { key, iv } => {
            let (key, iv): (&[u8; 32], &[u8; 12]) = (key.try_into().map_err(|_| bad())?, iv.try_into().map_err(|_| bad())?);
            let v = Chacha20Poly1305 { info: Info { version, cipher_type: TLS_CIPHER_CHACHA20_POLY1305 }, iv: *iv, key: *key, rec_seq: seq };
            set(fd, SOL_TLS, name, &v)
        }
    }
}

/// Hand the socket's TLS to the kernel: both directions. On an error the
/// socket is in no state to go on (rustls gave its keys away): close it.
#[cfg(target_os = "linux")]
pub fn enable(fd: RawFd, version: Version, tx: &Keys, rx: &Keys) -> io::Result<()> {
    // SAFETY: the name's 3 bytes, their length given (not a C string).
    let r = unsafe { libc::setsockopt(fd, libc::SOL_TCP, sys::TCP_ULP, b"tls".as_ptr() as *const libc::c_void, 3) };
    if r != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("TCP_ULP tls: {e}")));
    }
    direction(fd, sys::TLS_TX, version, tx).map_err(|e| io::Error::new(e.kind(), format!("TLS_TX: {e}")))?;
    direction(fd, sys::TLS_RX, version, rx).map_err(|e| io::Error::new(e.kind(), format!("TLS_RX: {e}")))
}

/// Can this kernel do it: tried once on a loopback connection, with a key
/// that's never used.
#[cfg(target_os = "linux")]
pub fn available() -> bool {
    use std::os::fd::AsRawFd;
    let Ok(l) = std::net::TcpListener::bind("127.0.0.1:0") else { return false };
    let Ok(c) = l.local_addr().and_then(std::net::TcpStream::connect) else { return false };
    let Ok((s, _)) = l.accept() else { return false };
    drop(c);
    let k = Keys { seq: 0, cipher: Cipher::AesGcm128 { key: &[0; 16], iv: &[0; 12] } };
    enable(s.as_raw_fd(), Version::Tls13, &k, &k).is_ok()
}

#[cfg(not(target_os = "linux"))]
pub fn available() -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub fn enable(_fd: RawFd, _version: Version, _tx: &Keys, _rx: &Keys) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "kernel TLS is Linux's"))
}

/// A record that isn't data, after a read failed with EIO (the kernel's way
/// of saying one is next): its type and its bytes, read into `buf`.
#[cfg(target_os = "linux")]
pub fn recv_control(fd: RawFd, buf: &mut [u8]) -> io::Result<(u8, usize)> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: a zeroed msghdr pointing at our iovec and control buffer,
    // both alive and of the sizes given.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: walking the control messages the kernel just wrote, with its macros.
    let mut c = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !c.is_null() {
        let h = unsafe { &*c };
        if h.cmsg_level == sys::SOL_TLS && h.cmsg_type == sys::TLS_GET_RECORD_TYPE {
            let typ = unsafe { *libc::CMSG_DATA(c) };
            return Ok((typ, n as usize));
        }
        c = unsafe { libc::CMSG_NXTHDR(&msg, c) };
    }
    Ok((APPLICATION_DATA, n as usize))
}

#[cfg(not(target_os = "linux"))]
pub fn recv_control(_fd: RawFd, _buf: &mut [u8]) -> io::Result<(u8, usize)> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "kernel TLS is Linux's"))
}

/// A record of type `typ` (an alert, say), not data.
#[cfg(target_os = "linux")]
pub fn send_control(fd: RawFd, typ: u8, data: &[u8]) -> io::Result<usize> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: as in recv_control: our iovec and control buffer, alive, sized.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(1) } as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = sys::SOL_TLS;
        (*c).cmsg_type = sys::TLS_SET_RECORD_TYPE;
        (*c).cmsg_len = libc::CMSG_LEN(1) as _;
        *libc::CMSG_DATA(c) = typ;
    }
    let n = unsafe { libc::sendmsg(fd, &msg, 0) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

#[cfg(not(target_os = "linux"))]
pub fn send_control(_fd: RawFd, _typ: u8, _data: &[u8]) -> io::Result<usize> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "kernel TLS is Linux's"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::sync::Arc;

    fn keys(s: &rustls::ConnectionTrafficSecrets) -> Cipher<'_> {
        match s {
            rustls::ConnectionTrafficSecrets::Aes128Gcm { key, iv } => Cipher::AesGcm128 { key: key.as_ref(), iv: iv.as_ref() },
            rustls::ConnectionTrafficSecrets::Aes256Gcm { key, iv } => Cipher::AesGcm256 { key: key.as_ref(), iv: iv.as_ref() },
            rustls::ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv } => Cipher::Chacha20Poly1305 { key: key.as_ref(), iv: iv.as_ref() },
            _ => panic!("a cipher the kernel doesn't do"),
        }
    }

    /// A real handshake (rustls on both ends), then the server's TLS goes
    /// to the kernel: the client, still rustls, can't tell. Data both ways,
    /// then the client's close_notify arrives as an alert record. For each
    /// TLS version and cipher the kernel does.
    #[test]
    fn a_session_handed_to_the_kernel_goes_on() {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let suites = [rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256, rustls::crypto::ring::cipher_suite::TLS13_AES_256_GCM_SHA384, rustls::crypto::ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256, rustls::crypto::ring::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256];
        for suite in suites {
            let p = Arc::new(rustls::crypto::CryptoProvider { cipher_suites: vec![suite], ..(*provider).clone() });
            let mut sc = rustls::ServerConfig::builder_with_provider(p.clone()).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert(vec![cert.cert.der().clone()], rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into())).unwrap();
            sc.enable_secret_extraction = true;
            sc.send_tls13_tickets = 0;
            let mut roots = rustls::RootCertStore::empty();
            roots.add(cert.cert.der().clone()).unwrap();
            let cc = rustls::ClientConfig::builder_with_provider(p).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth();
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = l.local_addr().unwrap();
            let client = std::thread::spawn(move || {
                let tcp = std::net::TcpStream::connect(addr).unwrap();
                let mut s = rustls::StreamOwned::new(rustls::ClientConnection::new(Arc::new(cc), "localhost".try_into().unwrap()).unwrap(), tcp);
                // The handshake (the write drives it), then wait for the server's word.
                s.write_all(b"").unwrap();
                while s.conn.is_handshaking() {
                    s.conn.complete_io(&mut s.sock).unwrap();
                }
                let mut b = [0u8; 5];
                s.read_exact(&mut b).unwrap();
                assert_eq!(&b, b"ready");
                s.write_all(b"ping").unwrap();
                s.read_exact(&mut b[..4]).unwrap();
                assert_eq!(&b[..4], b"pong");
                s.conn.send_close_notify();
                s.conn.complete_io(&mut s.sock).unwrap();
            });
            let (mut tcp, _) = l.accept().unwrap();
            let mut conn = rustls::ServerConnection::new(Arc::new(sc)).unwrap();
            while conn.is_handshaking() {
                conn.complete_io(&mut tcp).unwrap();
            }
            conn.writer().write_all(b"ready").unwrap();
            while conn.wants_write() {
                conn.write_tls(&mut tcp).unwrap();
            }
            let version = if conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3) { Version::Tls13 } else { Version::Tls12 };
            let s = conn.dangerous_extract_secrets().unwrap();
            if let Err(e) = enable(tcp.as_raw_fd(), version, &Keys { seq: s.tx.0, cipher: keys(&s.tx.1) }, &Keys { seq: s.rx.0, cipher: keys(&s.rx.1) }) {
                // A kernel without kTLS (or this cipher): say so, don't fail.
                eprintln!("skipping {suite:?}: this kernel won't take it: {e}");
                drop(tcp);
                let _ = client.join();
                continue;
            }
            // Plain bytes on the socket now: the kernel does the rest.
            let mut b = [0u8; 4];
            tcp.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"ping", "{suite:?}");
            tcp.write_all(b"pong").unwrap();
            // The client's close_notify: not data, so a read says EIO, and
            // the record comes with its type.
            let e = tcp.read(&mut b).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::EIO), "{suite:?}");
            let mut rec = [0u8; 64];
            let (typ, n) = recv_control(tcp.as_raw_fd(), &mut rec).unwrap();
            assert_eq!((typ, &rec[..n]), (ALERT, &[1u8, 0][..]), "{suite:?}: a warning close_notify");
            client.join().unwrap();
        }
    }
}
