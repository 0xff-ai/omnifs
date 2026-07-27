//! The daemon's unconditional TCP namespace endpoint.

#![cfg(not(target_os = "wasi"))]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use omnifs_api::{ControlOperation, ControlOutcome, DaemonStatus};
use omnifs_itest::live::{control_ready, control_request, daemon_args, omnifs_bin};
use tempfile::TempDir;

struct NamespaceOnlyDaemon {
    child: Child,
    home: TempDir,
}

impl Drop for NamespaceOnlyDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl NamespaceOnlyDaemon {
    fn control_socket(&self) -> PathBuf {
        self.home.path().join("control.sock")
    }

    fn status(&self) -> DaemonStatus {
        let reply = control_request(&self.control_socket(), ControlOperation::Status)
            .expect("status control reply");
        match reply.outcome {
            ControlOutcome::Status(status) => status,
            other => panic!("unexpected status reply: {other:?}"),
        }
    }
}

fn spawn_namespace_only(attach_port: Option<u16>) -> NamespaceOnlyDaemon {
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::create_dir_all(home.path().join("mounts")).expect("mounts dir");
    if let Some(port) = attach_port {
        std::fs::write(
            home.path().join("config.toml"),
            format!("[filesystem]\nattach_port = {port}\n"),
        )
        .expect("write attach port config");
    }
    let child = Command::new(omnifs_bin())
        .args(daemon_args(home.path()))
        .env("OMNIFS_HOME", home.path())
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("spawn omnifs daemon");
    let daemon = NamespaceOnlyDaemon { child, home };
    wait_ready(&daemon.control_socket(), Duration::from_secs(30));
    daemon
}

fn wait_ready(control_socket: &Path, deadline: Duration) {
    let start = Instant::now();
    while !control_ready(control_socket) {
        assert!(
            start.elapsed() < deadline,
            "daemon never became ready within {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn unused_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind probe port");
    listener.local_addr().expect("probe address").port()
}

#[test]
fn daemon_always_publishes_a_live_tcp_endpoint() {
    let daemon = spawn_namespace_only(None);
    let addr = daemon
        .status()
        .attach_tcp
        .expect("ready daemon must publish TCP attach endpoint");
    TcpStream::connect(addr)
        .unwrap_or_else(|error| panic!("connect to TCP attach endpoint {addr}: {error}"));
}

#[test]
fn configured_nonzero_port_is_honored_exactly() {
    let port = unused_port();
    let daemon = spawn_namespace_only(Some(port));
    let addr = daemon
        .status()
        .attach_tcp
        .expect("ready daemon must publish TCP attach endpoint");
    assert_eq!(addr.port(), port);
    TcpStream::connect(addr)
        .unwrap_or_else(|error| panic!("connect to configured endpoint {addr}: {error}"));
}
