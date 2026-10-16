//! `hfs` stops promptly on SIGTERM, also while it is still starting up.
//!
//! In a container the server is PID 1, where the kernel discards a signal
//! the process has no handler for: a SIGTERM during the (possibly
//! minutes-long) startup was ignored and `docker stop` ended in SIGKILL.
//! Outside a container the default action kills the process either way, so
//! these tests check the log line that only the installed handler writes.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start(dir: &Path, port: u16) -> Child {
    let log = std::fs::File::create(dir.join("hfs.log")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_hfs"))
        .env("HFS_DATABASE_URL", dir.join("fhir.db"))
        .env("HFS_SERVER_HOST", "127.0.0.1")
        .env("HFS_SERVER_PORT", port.to_string())
        .env("HFS_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("HFS_UI_ENABLED", "false")
        .env("HFS_LOG_LEVEL", "info")
        .env("HFS_PACKAGE_REGISTRY", "off")
        .env_remove("HFS_TERMINOLOGY_SERVER")
        .current_dir(dir)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("start hfs")
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("hfs.log")).unwrap_or_default()
}

fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ready() {
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn answers(port: u16) -> bool {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    if s
        .write_all(b"GET /metadata HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 12];
    s.read_exact(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200")
}

fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Waits for the process to exit; returns its status and how long it took.
fn exit_within(child: &mut Child, limit: Duration) -> (std::process::ExitStatus, Duration) {
    let t0 = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status, t0.elapsed());
        }
        if t0.elapsed() > limit {
            let _ = child.kill();
            panic!("hfs still running {limit:?} after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sigterm_while_starting_up_exits_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = start(dir.path(), port);
    // The first log line comes before the storage is opened and seeded.
    wait_for("the startup log line", Duration::from_secs(60), || {
        log(dir.path()).contains("Starting Helios FHIR Server")
    });
    sigterm(&child);
    let (status, took) = exit_within(&mut child, Duration::from_secs(10));
    let out = log(dir.path());
    assert!(status.success(), "{status:?}\n{out}");
    assert!(
        out.contains("while starting up") || out.contains("draining connections"),
        "the SIGTERM handler was not installed:\n{out}"
    );
    assert!(took < Duration::from_secs(5), "took {took:?}\n{out}");
}

#[test]
fn sigterm_while_serving_drains_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = start(dir.path(), port);
    wait_for("the server to answer", Duration::from_secs(120), || {
        answers(port)
    });
    sigterm(&child);
    let (status, took) = exit_within(&mut child, Duration::from_secs(10));
    let out = log(dir.path());
    assert!(status.success(), "{status:?}\n{out}");
    assert!(out.contains("draining connections"), "{out}");
    assert!(took < Duration::from_secs(5), "took {took:?}\n{out}");
    // The port is free again.
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
}
