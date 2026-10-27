//! A WebSocket echo, for the Autobahn TestSuite: a `ws` process that sends
//! every message back as it came, text as text and binary as binary.

#[link(wasm_import_module = "combd")]
extern "C" {
    fn ws_send(id: u64, ptr: *const u8, len: u32) -> u32;
    fn ws_send_binary(id: u64, ptr: *const u8, len: u32) -> u32;
    fn ws_msg(ptr: *mut u8) -> u32;
}

/// `do WSOPEN <path> <id>`, `do WSMSG /<id> <text>`, `do WSBIN /<id> <len>`.
fn run(req: &[u8]) -> &'static str {
    let mut parts = req.splitn(4, |&b| b == b' ');
    let (method, path, body) = (parts.nth(1).unwrap_or(b""), parts.next().unwrap_or(b""), parts.next().unwrap_or(b""));
    let sock: u64 = std::str::from_utf8(path).ok().and_then(|p| p.trim_start_matches('/').parse().ok()).unwrap_or(0);
    match method {
        b"WSOPEN" => "101 {}",
        b"WSMSG" => {
            unsafe { ws_send(sock, body.as_ptr(), body.len() as u32) };
            "200 {}"
        }
        b"WSBIN" => {
            let n: usize = std::str::from_utf8(body).ok().and_then(|n| n.trim().parse().ok()).unwrap_or(0);
            let mut msg = vec![0u8; n];
            unsafe { ws_msg(msg.as_mut_ptr()) };
            unsafe { ws_send_binary(sock, msg.as_ptr(), n as u32) };
            "200 {}"
        }
        _ => "200 {}",
    }
}

#[no_mangle]
pub extern "C" fn alloc(len: u32) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len as usize);
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn release(ptr: *mut u8, len: u32) {
    drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr, len as usize)) });
}

#[no_mangle]
pub extern "C" fn handle(ptr: *mut u8, len: u32) -> u64 {
    let req = unsafe { Vec::from_raw_parts(ptr, len as usize, len as usize) };
    let answer: Box<[u8]> = run(&req).as_bytes().into();
    let out = Box::into_raw(answer);
    ((out as *mut u8 as u64) << 32) | out.len() as u64
}
