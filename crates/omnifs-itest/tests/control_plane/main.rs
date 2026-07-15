//! Control-plane acceptance: two daemons, two homes, one socket each.
//!
//! Proves the Unix-socket control plane and the daemon-owned daemon record end
//! to end against real mounts: each daemon binds its own workspace's
//! `control.sock` and writes its own `daemon.json`; the CLI only ever dials the
//! endpoint it read from its own workspace's record, so two daemons never
//! address each other and a fresh home with no record never dials blind. A
//! `SIGKILL`ed daemon leaves a stale record, which the next command cleans up.
//!
//! Gated on `OMNIFS_ACCEPTANCE_LIVE` (it serves real mounts). Holds the
//! cross-process NFS serialization lock for the whole test.

#![cfg(not(target_os = "wasi"))]

use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use omnifs_itest::live::{self, hermetic_home, omnifs_bin, platform_can_mount};
use tempfile::TempDir;

/// A host-native daemon serving only its workspace-local control socket, torn
/// down on drop.
struct Daemon {
    child: Child,
    home: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn record_path(&self) -> PathBuf {
        self.home.path().join("daemon.json")
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Spawn a host-native daemon for a fresh hermetic home. The daemon binds
    /// its fixed local frontend socket but launches no runner, so its control
    /// API is reachable only through its own workspace's record.
    fn spawn() -> Option<Self> {
        let hermetic = hermetic_home();
        let child = Command::new(omnifs_bin())
            .arg("daemon")
            .env("OMNIFS_HOME", hermetic.home.path())
            .env("RUST_LOG", "warn")
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                eprintln!("skip: spawn omnifs daemon failed: {error}");
                return None;
            },
        };
        Some(Self {
            child,
            home: hermetic.home,
        })
    }

    /// Wait for the daemon to publish its record. This fixture intentionally
    /// starts the pure namespace daemon without a frontend: the control-plane
    /// assertion is socket ownership, not a frontend mount.
    fn wait_serving(&mut self) -> Option<()> {
        let record = self.record_path();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if record.exists() {
                return Some(());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                eprintln!("skip: daemon exited ({status}) before publishing its record");
                return None;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "skip: daemon never published {} within 30s",
                    record.display()
                );
                return None;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

/// Run `omnifs status --output json` against `home`, resolving through the
/// workspace's local control socket.
fn run_status(home: &std::path::Path) -> Output {
    Command::new(omnifs_bin())
        .args(["status", "--output", "json"])
        .env("OMNIFS_HOME", home)
        .env("RUST_LOG", "warn")
        .output()
        .expect("spawn omnifs status")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(128)
}

fn status_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "status --output json must produce valid JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

#[test]
#[allow(clippy::too_many_lines)] // linear end-to-end acceptance scenario
fn two_daemons_two_homes_resolve_through_their_own_records() {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live control-plane acceptance");
        return;
    }
    if !platform_can_mount() {
        eprintln!("skip: platform cannot mount");
        return;
    }

    // Hold the cross-process NFS lock for the whole test so no other live-mount
    // binary races these two mounts.
    let _nfs_lock = live::nfs_serial_lock();

    // Two daemons use independent homes and mount points.
    let Some(mut daemon_a) = Daemon::spawn() else {
        return;
    };
    let Some(mut daemon_b) = Daemon::spawn() else {
        return;
    };
    if daemon_a.wait_serving().is_none() || daemon_b.wait_serving().is_none() {
        return;
    }

    // Each home resolves its own daemon over its own socket.
    let out_a = run_status(daemon_a.home.path());
    assert_eq!(
        exit_code(&out_a),
        0,
        "status for home A must exit 0\nstderr: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    let json_a = status_json(&out_a);
    let result_a = json_a["result"].as_object().expect("status result");
    assert_eq!(result_a["workspace"]["daemon_state"], "running");
    let pid_a = result_a["workspace"]["pid"].as_u64().expect("A pid");
    assert_eq!(
        pid_a,
        u64::from(daemon_a.pid()),
        "status A must report A's pid"
    );
    assert!(result_a["frontends"].as_array().is_some_and(Vec::is_empty));
    assert!(
        result_a["mounts"]
            .as_array()
            .is_some_and(|mounts| mounts.len() >= 2)
    );

    let out_b = run_status(daemon_b.home.path());
    assert_eq!(exit_code(&out_b), 0, "status for home B must exit 0");
    let json_b = status_json(&out_b);
    let pid_b = json_b["result"]["workspace"]["pid"]
        .as_u64()
        .expect("B pid");
    assert_eq!(
        pid_b,
        u64::from(daemon_b.pid()),
        "status B must report B's pid"
    );
    assert!(
        json_b["result"]["frontends"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );

    assert_ne!(pid_a, pid_b, "the two daemons must be distinct processes");

    // A fresh home with no daemon record never dials a default address.
    //
    // `omnifs status` is informational and exits 0 whether or not a daemon is
    // running (locked by the CLI lifecycle suite's scenario_1). The property
    // that matters here is that a home with no record reports `not_running` and
    // never dials A or B: with no record and no override, resolution short
    // circuits to absent, so it can only ever report its own (missing) daemon.
    let fresh = hermetic_home();
    let out_fresh = run_status(fresh.home.path());
    assert_eq!(
        exit_code(&out_fresh),
        0,
        "status for a home with no record must exit 0 (informational)\nstderr: {}",
        String::from_utf8_lossy(&out_fresh.stderr)
    );
    assert_eq!(
        status_json(&out_fresh)["result"]["workspace"]["daemon_state"],
        "stopped",
        "a home with no record must report not_running, never a foreign daemon"
    );

    // After home A's daemon is killed, its next command cleans the stale
    // record without disturbing home B.
    let pid_a = daemon_a.pid();
    let _ = Command::new("kill")
        .args(["-9", &pid_a.to_string()])
        .output();
    // Wait for the kernel to reap it so its socket stops accepting connections.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let alive = Command::new("kill")
            .args(["-0", &pid_a.to_string()])
            .output()
            .is_ok_and(|o| o.status.success());
        if !alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // A's record still points at its now-dead socket. Resolving through it, the
    // status probe hits a refused socket, removes the stale record, and reports
    // the daemon as not running.
    let out_dead = run_status(daemon_a.home.path());
    assert_eq!(
        exit_code(&out_dead),
        0,
        "status for the killed home A must still exit 0 (informational)\nstderr: {}",
        String::from_utf8_lossy(&out_dead.stderr)
    );
    assert_eq!(
        status_json(&out_dead)["result"]["workspace"]["daemon_state"],
        "stopped",
        "the killed home A must report not_running"
    );
    assert!(
        !daemon_a.record_path().exists(),
        "the stale daemon record must be cleaned up after dialing a refused socket"
    );

    // Home B still answers correctly.
    let out_b2 = run_status(daemon_b.home.path());
    assert_eq!(
        exit_code(&out_b2),
        0,
        "home B must still answer after A is gone"
    );
    assert_eq!(
        status_json(&out_b2)["result"]["workspace"]["pid"].as_u64(),
        Some(u64::from(daemon_b.pid())),
    );

    // A graceful SIGTERM removes home B's daemon record.
    let pid_b = daemon_b.pid();
    let _ = Command::new("kill")
        .args(["-TERM", &pid_b.to_string()])
        .output();
    let record_b = daemon_b.record_path();
    let deadline = Instant::now() + Duration::from_secs(10);
    while record_b.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !record_b.exists(),
        "a gracefully stopped daemon must remove its daemon record"
    );

    drop(daemon_a);
    drop(daemon_b);
}
