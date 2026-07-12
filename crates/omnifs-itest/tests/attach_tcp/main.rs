//! The TCP namespace-attach transport: `--attach-tcp` and
//! `POST /v1/frontend/attach-target`.
//!
//! Both entry points converge on one binding. No frontend runner is launched,
//! so no OS mount is served and this suite needs no acceptance gate.

#![cfg(not(target_os = "wasi"))]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use omnifs_itest::live::omnifs_bin;
use tempfile::TempDir;

/// A host-native daemon with no frontend runner. It reconciles an empty mount
/// set and serves its control and namespace sockets, with no OS mount anywhere.
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

    fn record_path(&self) -> PathBuf {
        self.home.path().join("daemon.json")
    }

    fn record(&self) -> serde_json::Value {
        let bytes = std::fs::read(self.record_path()).expect("read daemon.json");
        serde_json::from_slice(&bytes).expect("daemon.json is valid JSON")
    }
}

/// Spawn a daemon in a fresh, empty `OMNIFS_HOME` (no provider,
/// no mount spec: reconcile converges an empty set immediately).
fn spawn_namespace_only(extra_args: &[&str]) -> NamespaceOnlyDaemon {
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::create_dir_all(home.path().join("mounts")).expect("mounts dir");

    let mut args = vec!["daemon"];
    args.extend_from_slice(extra_args);
    let child = Command::new(omnifs_bin())
        .args(&args)
        .env("OMNIFS_HOME", home.path())
        .env_remove("OMNIFS_DAEMON_ADDR")
        .env_remove("OMNIFS_CONTROL_TOKEN")
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("spawn omnifs daemon");
    NamespaceOnlyDaemon { child, home }
}

/// Poll `/v1/ready` over the Unix control socket until it succeeds. Ready means
/// mounts reconciled and every requested surface (including a startup
/// `--attach-tcp` bind) is up, so once this returns, `daemon.json` reflects
/// every startup flag.
fn wait_ready(ctrl_socket: &Path, deadline: Duration) {
    let start = Instant::now();
    loop {
        let ok = Command::new("curl")
            .args(["-fs", "-o", "/dev/null", "--unix-socket"])
            .arg(ctrl_socket)
            .arg("http://localhost/v1/ready")
            .status()
            .is_ok_and(|status| status.success());
        if ok {
            return;
        }
        assert!(
            start.elapsed() < deadline,
            "daemon never became ready within {deadline:?} (control socket {})",
            ctrl_socket.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `POST /v1/frontend/attach-target` over the Unix control socket, with an optional
/// JSON body, returning the parsed response. Panics on a non-2xx status (the
/// namespace-not-ready 503 included) since every call in this suite happens
/// after `wait_ready`.
fn post_frontend_attach_target(ctrl_socket: &Path, body: Option<&str>) -> serde_json::Value {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsS", "--unix-socket"]).arg(ctrl_socket);
    if let Some(body) = body {
        cmd.args(["-H", "content-type: application/json", "-d", body]);
    } else {
        cmd.args(["-X", "POST"]);
    }
    cmd.arg("http://localhost/v1/frontend/attach-target");
    let output = cmd.output().expect("spawn curl");
    assert!(
        output.status.success(),
        "POST /v1/frontend/attach-target failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("attach-target response is JSON")
}

fn assert_looks_like_a_token(token: &str) {
    assert_eq!(
        token.len(),
        32,
        "attach token must be 32 hex chars: {token}"
    );
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "attach token must be hex: {token}"
    );
}

#[test]
fn attach_tcp_flag_binds_at_start_and_the_record_carries_it() {
    let daemon = spawn_namespace_only(&["--attach-tcp", "0"]);
    wait_ready(&daemon.control_socket(), Duration::from_secs(30));

    let record = daemon.record();
    let attach = record
        .get("attach")
        .and_then(serde_json::Value::as_array)
        .expect("daemon.json must carry an attach array");
    assert_eq!(attach.len(), 1, "exactly one attach target: {record}");
    let entry = &attach[0];
    assert_eq!(entry["transport"], "tcp", "the entry must be tcp: {entry}");
    let addr = entry["addr"].as_str().expect("attach addr").to_string();
    let token = entry["token"].as_str().expect("attach token").to_string();
    assert_looks_like_a_token(&token);

    // The listener is really up: a bare TCP connect succeeds (the handshake
    // itself, including the token, is proven at the wire-crate level).
    TcpStream::connect(&addr)
        .unwrap_or_else(|error| panic!("connect to the attach listener at {addr}: {error}"));
}

#[test]
fn no_attach_tcp_flag_means_no_attach_in_the_record() {
    let daemon = spawn_namespace_only(&[]);
    wait_ready(&daemon.control_socket(), Duration::from_secs(30));

    let record = daemon.record();
    assert!(
        record.get("attach").is_none(),
        "daemon.json must not carry attach when --attach-tcp was never passed: {record}"
    );
}

#[test]
fn frontend_attach_target_route_binds_on_demand_and_is_idempotent() {
    let daemon = spawn_namespace_only(&[]);
    let ctrl_socket = daemon.control_socket();
    wait_ready(&ctrl_socket, Duration::from_secs(30));
    assert!(
        daemon.record().get("attach").is_none(),
        "no attach listener before the route is ever called"
    );

    let first = post_frontend_attach_target(&ctrl_socket, Some(r#"{"bind_ip":"127.0.0.1"}"#));
    let addr = first["addr"].as_str().expect("addr").to_string();
    let token = first["token"].as_str().expect("token").to_string();
    assert_looks_like_a_token(&token);
    assert!(
        addr.starts_with("127.0.0.1:"),
        "the requested bind address must be honored: {addr}"
    );

    // A listener cannot be re-pointed once serving: a repeat call (even with a
    // another request returns the same binding rather than rebinding.
    let second = post_frontend_attach_target(&ctrl_socket, Some("{}"));
    assert_eq!(
        second["addr"], first["addr"],
        "a repeat call must not rebind"
    );
    assert_eq!(
        second["token"], first["token"],
        "a repeat call must return the same token"
    );

    // The record was patched in place with the same values, and nothing else
    // about it (notably started_at) shifted just because attach bound later.
    let before = daemon.record();
    std::thread::sleep(Duration::from_millis(50));
    post_frontend_attach_target(&ctrl_socket, None);
    let after = daemon.record();
    assert_eq!(before["started_at"], after["started_at"]);
    let attach = after
        .get("attach")
        .and_then(serde_json::Value::as_array)
        .expect("daemon.json must carry an attach array");
    assert_eq!(attach.len(), 1, "exactly one attach target: {after}");
    let entry = &attach[0];
    assert_eq!(entry["transport"], "tcp", "the entry must be tcp: {entry}");
    assert_eq!(entry["addr"], addr);
    assert_eq!(entry["token"], token);

    TcpStream::connect(&addr)
        .unwrap_or_else(|error| panic!("connect to the attach listener at {addr}: {error}"));
}
