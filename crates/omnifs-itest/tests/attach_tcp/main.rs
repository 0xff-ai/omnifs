//! The daemon's unconditional TCP namespace endpoint.

#![cfg(not(target_os = "wasi"))]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use omnifs_api::DaemonStatus;
use omnifs_itest::live::{control_ready, control_status, daemon_args, omnifs_bin};
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
        control_status(&self.control_socket())
    }

    fn restart(&mut self) {
        self.child.kill().expect("kill namespace daemon");
        self.child.wait().expect("reap namespace daemon");
        self.child = spawn_daemon(self.home.path());
        wait_ready(&self.control_socket(), Duration::from_secs(30));
    }
}

fn spawn_daemon(home: &Path) -> Child {
    Command::new(omnifs_bin())
        .args(daemon_args(home))
        .env("OMNIFS_HOME", home)
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("spawn omnifs daemon")
}

fn spawn_namespace_only() -> NamespaceOnlyDaemon {
    let home = tempfile::tempdir().expect("home tempdir");
    let child = spawn_daemon(home.path());
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

#[test]
fn daemon_always_publishes_a_live_tcp_endpoint() {
    let daemon = spawn_namespace_only();
    let addr = daemon
        .status()
        .attach_tcp
        .expect("ready daemon must publish TCP attach endpoint");
    TcpStream::connect(addr)
        .unwrap_or_else(|error| panic!("connect to TCP attach endpoint {addr}: {error}"));
}

#[test]
fn daemon_persists_its_selected_port_across_restart() {
    let mut daemon = spawn_namespace_only();
    let first = daemon
        .status()
        .attach_tcp
        .expect("ready daemon must publish TCP attach endpoint");
    daemon.restart();
    let second = daemon
        .status()
        .attach_tcp
        .expect("restarted daemon must publish TCP attach endpoint");
    assert_eq!(second.port(), first.port());
    TcpStream::connect(second)
        .unwrap_or_else(|error| panic!("connect to persisted endpoint {second}: {error}"));
}
