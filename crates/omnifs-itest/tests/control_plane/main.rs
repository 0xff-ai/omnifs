//! Control-plane acceptance: two daemons, two homes, one socket each.
//!
//! Proves the Unix-socket control plane and the daemon-owned runtime record end
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

/// A host-native daemon serving only its Unix socket (no `--listen`, no
/// `OMNIFS_DAEMON_ADDR`), torn down on drop.
struct Daemon {
    child: Child,
    home: TempDir,
    mount_point: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.detach_mount();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn record_path(&self) -> PathBuf {
        self.home.path().join("daemon.json")
    }

    fn message_path(&self) -> PathBuf {
        self.mount_point.join("test/hello/message")
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Spawn a host-native daemon for a fresh hermetic home. The daemon serves
    /// the platform-default frontend and binds its Unix socket; it is handed no
    /// TCP listener and no `OMNIFS_DAEMON_ADDR`, so it is reachable only through
    /// its own workspace's record.
    fn spawn() -> Option<Self> {
        let hermetic = hermetic_home();
        let mount_point = hermetic.mount_point.clone();
        let child = Command::new(omnifs_bin())
            .arg("daemon")
            .env("OMNIFS_HOME", hermetic.home.path())
            .env("OMNIFS_MOUNT_POINT", &mount_point)
            .env_remove("OMNIFS_DAEMON_ADDR")
            .env_remove("OMNIFS_CONTROL_TOKEN")
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
            mount_point,
        })
    }

    /// Wait for the daemon to publish its record and serve the projected tree.
    /// `None` (skip) if the mount never comes up on this platform.
    fn wait_serving(&mut self) -> Option<()> {
        let record = self.record_path();
        let message = self.message_path();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if record.exists() && message.is_file() {
                return Some(());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                eprintln!("skip: daemon exited ({status}) before it served its mount");
                return None;
            }
            if Instant::now() >= deadline {
                eprintln!("skip: daemon never served {} within 30s", message.display());
                return None;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn detach_mount(&self) {
        force_unmount(&self.mount_point);
    }
}

/// Force-unmount `mount_point`. On macOS a `SIGKILL`ed daemon leaves a
/// dead-server NFS mount where a plain `umount` blocks in an uninterruptible
/// syscall, so `sudo -n umount -f` is the only teardown that returns promptly;
/// it is also safe for a still-live mount. The parent is canonicalized so the
/// dead mount itself is never stat-ed.
fn force_unmount(mount_point: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        if !omnifs_nfs::mount_is_active(mount_point) {
            return;
        }
        if let Some(canonical) = mount_point
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .and_then(|parent| mount_point.file_name().map(|leaf| parent.join(leaf)))
        {
            let _ = Command::new("sudo")
                .args(["-n", "umount", "-f"])
                .arg(&canonical)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::OsStr;
        let mp = mount_point.as_os_str();
        let _ = Command::new("fusermount")
            .args([OsStr::new("-uz"), mp])
            .status();
        let _ = Command::new("umount").arg(mp).status();
    }
    #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
    {
        if omnifs_nfs::mount_is_active(mount_point) {
            let _ = Command::new("umount").arg("-f").arg(mount_point).status();
        }
    }
}

/// Run `omnifs status --json` against `home`, with the daemon-address override
/// and token stripped so resolution goes through the workspace's record.
fn run_status(home: &std::path::Path, mount_point: &std::path::Path) -> Output {
    Command::new(omnifs_bin())
        .args(["status", "--json"])
        .env("OMNIFS_HOME", home)
        .env("OMNIFS_MOUNT_POINT", mount_point)
        .env_remove("OMNIFS_DAEMON_ADDR")
        .env_remove("OMNIFS_CONTROL_TOKEN")
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
            "status --json must produce valid JSON: {error}\nstdout: {}\nstderr: {}",
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
    let out_a = run_status(daemon_a.home.path(), &daemon_a.mount_point);
    assert_eq!(
        exit_code(&out_a),
        0,
        "status for home A must exit 0\nstderr: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    let json_a = status_json(&out_a);
    assert_eq!(json_a["runtime"]["state"], "running");
    let pid_a = json_a["runtime"]["pid"].as_u64().expect("A pid");
    assert_eq!(
        pid_a,
        u64::from(daemon_a.pid()),
        "status A must report A's pid"
    );
    assert_eq!(
        json_a["runtime"]["mount_point"].as_str().unwrap_or(""),
        daemon_a.mount_point.to_str().unwrap(),
        "status A must name A's mount point"
    );

    let out_b = run_status(daemon_b.home.path(), &daemon_b.mount_point);
    assert_eq!(exit_code(&out_b), 0, "status for home B must exit 0");
    let json_b = status_json(&out_b);
    let pid_b = json_b["runtime"]["pid"].as_u64().expect("B pid");
    assert_eq!(
        pid_b,
        u64::from(daemon_b.pid()),
        "status B must report B's pid"
    );
    assert_eq!(
        json_b["runtime"]["mount_point"].as_str().unwrap_or(""),
        daemon_b.mount_point.to_str().unwrap(),
        "status B must name B's mount point"
    );

    assert_ne!(pid_a, pid_b, "the two daemons must be distinct processes");

    // A fresh home with no runtime record never dials a default address.
    //
    // `omnifs status` is informational and exits 0 whether or not a daemon is
    // running (locked by the CLI lifecycle suite's scenario_1). The property
    // that matters here is that a home with no record reports `not_running` and
    // never dials A or B: with no record and no override, resolution short
    // circuits to absent, so it can only ever report its own (missing) daemon.
    let fresh = hermetic_home();
    let out_fresh = run_status(fresh.home.path(), &fresh.mount_point);
    assert_eq!(
        exit_code(&out_fresh),
        0,
        "status for a home with no record must exit 0 (informational)\nstderr: {}",
        String::from_utf8_lossy(&out_fresh.stderr)
    );
    assert_eq!(
        status_json(&out_fresh)["runtime"]["state"],
        "not_running",
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
    daemon_a.detach_mount();

    // A's record still points at its now-dead socket. Resolving through it, the
    // status probe hits a refused socket, removes the stale record, and reports
    // the daemon as not running.
    let out_dead = run_status(daemon_a.home.path(), &daemon_a.mount_point);
    assert_eq!(
        exit_code(&out_dead),
        0,
        "status for the killed home A must still exit 0 (informational)\nstderr: {}",
        String::from_utf8_lossy(&out_dead.stderr)
    );
    assert_eq!(
        status_json(&out_dead)["runtime"]["state"],
        "not_running",
        "the killed home A must report not_running"
    );
    assert!(
        !daemon_a.record_path().exists(),
        "the stale runtime record must be cleaned up after dialing a refused socket"
    );

    // Home B still answers correctly.
    let out_b2 = run_status(daemon_b.home.path(), &daemon_b.mount_point);
    assert_eq!(
        exit_code(&out_b2),
        0,
        "home B must still answer after A is gone"
    );
    assert_eq!(
        status_json(&out_b2)["runtime"]["pid"].as_u64(),
        Some(u64::from(daemon_b.pid())),
    );

    // A graceful SIGTERM removes home B's runtime record.
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
        "a gracefully stopped daemon must remove its runtime record"
    );

    // Drop guards force-unmount both mount points.
    drop(daemon_a);
    drop(daemon_b);
}
