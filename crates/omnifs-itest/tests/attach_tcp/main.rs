//! The TCP namespace-attach transport: `--attach-tcp` and the typed local
//! control operation that ensures the VFS TCP target.
//!
//! Both entry points converge on one binding. No frontend runner is launched,
//! so no OS mount is served and this suite needs no acceptance gate.

#![cfg(not(target_os = "wasi"))]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use omnifs_api::{ControlOperation, ControlOutcome};
use omnifs_itest::live::{control_ready, control_request, daemon_args, omnifs_bin};
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

    fn attach_targets(&self) -> Vec<omnifs_workspace::attach::Target> {
        omnifs_workspace::attach::Store::open(self.home.path().join("frontends/targets.json"))
            .expect("open attach target store")
            .targets()
    }
}

/// Spawn a daemon in a fresh, empty `OMNIFS_HOME` (no provider,
/// no mount spec: reconcile converges an empty set immediately).
fn spawn_namespace_only(extra_args: &[&str]) -> NamespaceOnlyDaemon {
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::create_dir_all(home.path().join("mounts")).expect("mounts dir");

    let mut args = daemon_args(home.path());
    args.extend(extra_args.iter().map(std::ffi::OsString::from));
    let child = Command::new(omnifs_bin())
        .args(&args)
        .env("OMNIFS_HOME", home.path())
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("spawn omnifs daemon");
    NamespaceOnlyDaemon { child, home }
}

/// Poll the typed Ready operation over the Unix control socket until it succeeds. Ready means
/// mounts reconciled and every requested surface (including a startup
/// `--attach-tcp` bind) is up, so once this returns, `daemon.json` reflects
/// every startup listener.
fn wait_ready(ctrl_socket: &Path, deadline: Duration) {
    let start = Instant::now();
    loop {
        let ok = control_ready(ctrl_socket);
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

/// Ensure the VFS TCP target over the typed local control socket, with an
/// optional JSON-shaped bind address retained for the existing fixture calls.
fn post_frontend_attach_target(ctrl_socket: &Path, body: Option<&str>) -> serde_json::Value {
    let bind_ip = body
        .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .and_then(|body| {
            body.get("bind_ip")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .map(|ip| ip.parse().expect("bind_ip is IPv4"));
    let reply = control_request(ctrl_socket, ControlOperation::AttachTcp { bind_ip })
        .expect("attach target control reply");
    match reply.outcome {
        ControlOutcome::AttachTcp(target) => serde_json::to_value(target).unwrap(),
        other => panic!("unexpected attach target reply: {other:?}"),
    }
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
fn attach_tcp_flag_binds_at_start_and_persists_the_target() {
    let daemon = spawn_namespace_only(&["--attach-tcp", "0"]);
    wait_ready(&daemon.control_socket(), Duration::from_secs(30));

    let targets = daemon.attach_targets();
    let [omnifs_workspace::attach::Target::Tcp { addr, token }] = targets.as_slice() else {
        panic!("expected one durable TCP attach target, got {targets:?}");
    };
    assert_looks_like_a_token(token);

    // The listener is really up: a bare TCP connect succeeds (the handshake
    // itself, including the token, is proven at the wire-crate level).
    TcpStream::connect(addr)
        .unwrap_or_else(|error| panic!("connect to the attach listener at {addr}: {error}"));
}

#[test]
fn no_attach_tcp_flag_means_no_durable_attach_target() {
    let daemon = spawn_namespace_only(&[]);
    wait_ready(&daemon.control_socket(), Duration::from_secs(30));

    assert!(daemon.attach_targets().is_empty());
}

#[test]
fn frontend_attach_target_route_binds_on_demand_and_is_idempotent() {
    let daemon = spawn_namespace_only(&[]);
    let ctrl_socket = daemon.control_socket();
    wait_ready(&ctrl_socket, Duration::from_secs(30));
    assert!(daemon.attach_targets().is_empty());
    let daemon_record = daemon.record();

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

    // Attach authority is durable but separate from process identity. Reusing
    // the binding must not rewrite the daemon record.
    std::thread::sleep(Duration::from_millis(50));
    post_frontend_attach_target(&ctrl_socket, None);
    assert_eq!(daemon.record(), daemon_record);
    let targets = daemon.attach_targets();
    let [
        omnifs_workspace::attach::Target::Tcp {
            addr: stored_addr,
            token: stored_token,
        },
    ] = targets.as_slice()
    else {
        panic!("expected one durable TCP attach target, got {targets:?}");
    };
    assert_eq!(stored_addr.to_string(), addr);
    assert_eq!(stored_token, &token);

    TcpStream::connect(&addr)
        .unwrap_or_else(|error| panic!("connect to the attach listener at {addr}: {error}"));
}
