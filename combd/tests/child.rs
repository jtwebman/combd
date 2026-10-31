//! The cells in a process of their own (`cells::spawn`), from the built
//! binary: a unit test's binary is the test runner, which can't be one.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn http(port: u16, method: &str, path: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(s, "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").unwrap();
    let mut r = String::new();
    s.read_to_string(&mut r).unwrap();
    let status = r.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, r.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

/// Process `pid` is still there.
fn alive(pid: u32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[test]
fn the_cells_run_in_a_child_that_ends_with_the_node() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("combd-child-{}-{port}", std::process::id()));
    let mut node = Command::new(env!("CARGO_BIN_EXE_combd"))
        .args(["serve", "interp", "rust", &port.to_string()])
        .env("COMBD_DATA", &dir)
        .env("COMBD_THREADS", "2")
        .env("COMBD_S3_SECRET", "not-the-cells-business")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(node.stdout.take().unwrap()).lines();
    let cells: u32 = out.by_ref().map_while(Result::ok).find_map(|l| l.split("cells in process ").nth(1).map(|p| p.trim().parse().unwrap())).expect("the cells' process");
    // What the child says of its lockdown, before it serves.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    let locked = out.by_ref().map_while(Result::ok).find_map(|l| l.split_once("cells ").filter(|(_, r)| r.contains("locked down")).map(|(_, r)| r.trim_start_matches("locked down: ").to_string()));
    let start = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(start.elapsed() < Duration::from_secs(10), "the node didn't listen");
        std::thread::sleep(Duration::from_millis(20));
    }
    // An object through the child: run, committed, fsynced, answered.
    for n in 1..=3 {
        assert_eq!(http(port, "POST", "/do/room1/incr"), (200, format!("{{\"count\":{n}}}")));
    }
    assert_eq!(http(port, "GET", "/_combd/stats").0, 200);
    #[cfg(target_os = "linux")]
    {
        // An empty environment, which another process of the same user
        // can't read anyway: like the node, the cells are non-dumpable.
        // (A reader that may trace any process sees it, empty.)
        match std::fs::read(format!("/proc/{cells}/environ")) {
            Ok(env) => assert_eq!(env, b"", "the cells start with an empty environment"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}"),
        }
        // Locked down (`combd_wall`, whose own tests try to get out): a
        // seccomp filter on, and no gaining rights.
        let status = std::fs::read_to_string(format!("/proc/{cells}/status")).unwrap();
        for want in ["NoNewPrivs:\t1", "Seccomp:\t2"] {
            assert!(status.lines().any(|l| l == want), "the cells' process: no {want:?} in\n{status}");
        }
        assert!(locked.as_ref().is_some_and(|l| l.starts_with("landlock ")), "{locked:?}");
    }
    // The node gone (killed: no goodbye), its sockets close, and the
    // child goes with it.
    node.kill().unwrap();
    node.wait().unwrap();
    let start = Instant::now();
    while alive(cells) {
        assert!(start.elapsed() < Duration::from_secs(5), "the cells' process outlived the node");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A deploy's check is the cells' (the node loads no program): an app they
/// refuse doesn't start, and says why.
#[test]
fn the_cells_check_a_deploy_and_say_why_not() {
    let million = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/million/app");
    let dir = std::env::temp_dir().join(format!("combd-child-check-{}", std::process::id()));
    let app = dir.join("million");
    std::fs::create_dir_all(app.join("modules")).unwrap();
    let toml = std::fs::read_to_string(format!("{million}/combd.toml")).unwrap().replace("engine = \"compiled\"", "engine = \"interpreted\"").replace("memory_kb = 512", "memory_kb = 64");
    std::fs::write(app.join("combd.toml"), toml).unwrap();
    for f in ["million.wasm", "viewer.wasm", "web.wasm"] {
        std::fs::copy(format!("{million}/modules/{f}"), app.join("modules").join(f)).expect("build it first: examples/million/build.sh");
    }
    let out = Command::new(env!("CARGO_BIN_EXE_combd")).args(["serve-app", "interp", app.to_str().unwrap(), "0"]).env("COMBD_DATA", dir.join("data")).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("million/viewer: memory_kb 64 is under this module's starting memory (128 KB)"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
