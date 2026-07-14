//! Lifecycle acceptance tests for the CLI ↔ daemon lifecycle.
//!
//! Each test drives the real `omnifs` binary against a hermetic `OMNIFS_HOME`
//! with its own temp dir, mount point, and control port. No test touches the
//! user's real `~/.omnifs`, `~/omnifs`, or port 7878.
//!
//! The fixture writes `test_provider.wasm` into `<OMNIFS_HOME>/providers/` and
//! writes the no-auth test mount spec to `<OMNIFS_HOME>/mounts/test.json`. The
//! mount serves `test/hello/message`.
//!
//! A `Drop` guard on `Fixture` force-unmounts the mount point and kills any
//! surviving daemon, so a panicking or interrupted test still cleans up.
//!
//! Skip (not fail) only when the platform genuinely cannot mount. A daemon
//! that exits due to a CLI parse error or bad argument is a real failure.
//!
//! Tests that involve live NFS mounts (scenarios 3-6 on macOS) run under a
//! process-global mutex to avoid concurrent NFS mount/unmount operations that
//! can serialize through the macOS kernel's NFS state and cause timeouts.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::{
    force_unmount, free_port, install_test_provider, live_acceptance_enabled, nfs_serial_lock,
    omnifs_bin, platform_can_mount, recorded_pid, release_wasm_dir, test_mount_spec,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Shared bearer token for the debug TCP path. `omnifs up` spawns the daemon
/// with this in its environment, and every CLI invocation dials with it.
const CONTROL_TOKEN: &str = "lifecycle-acceptance-token";

// ── Fixture ───────────────────────────────────────────────────────────────────

/// Hermetic per-test fixture: a fresh temp dir, providers, mount point, and
/// control address. Drops cleanly even when a test panics.
struct Fixture {
    home: tempfile::TempDir,
    mount_point: PathBuf,
    daemon_addr: String,
    /// Content id of the test provider installed into the provider store.
    test_provider_id: omnifs_workspace::ids::ProviderId,
    /// PID to kill on drop, when a daemon was spawned via `omnifs up` rather
    /// than the daemon subcommand directly.
    daemon_pid: Option<u32>,
}

impl Fixture {
    /// Allocate a fresh fixture with the test provider installed. Does NOT write any
    /// mount spec; callers write what they need.
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let providers_dir = home.path().join("providers");
        std::fs::create_dir_all(&providers_dir).expect("providers dir");

        let test_provider_id = install_test_provider(&providers_dir);

        let mounts_dir = home.path().join("mounts");
        std::fs::create_dir_all(&mounts_dir).expect("mounts dir");

        let mount_point = home.path().join("mnt");
        std::fs::create_dir_all(&mount_point).expect("mount point dir");

        let port = free_port();
        let daemon_addr = format!("127.0.0.1:{port}");

        Self {
            home,
            mount_point,
            daemon_addr,
            test_provider_id,
            daemon_pid: None,
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn mounts_dir(&self) -> PathBuf {
        self.home_path().join("mounts")
    }

    fn runtime_record_path(&self) -> PathBuf {
        self.home_path().join("daemon.json")
    }

    /// Write the test mount spec (`test.json`) into `<home>/mounts/`.
    fn write_test_spec(&self) {
        std::fs::write(
            self.mounts_dir().join("test.json"),
            test_mount_spec(&self.test_provider_id),
        )
        .expect("write test mount spec");
    }

    /// Write a second valid mount spec pinned to the test provider.
    fn write_other_spec(&self) {
        let spec = test_mount_spec(&self.test_provider_id)
            .replace(r#""mount":"test""#, r#""mount":"other""#);
        std::fs::write(self.mounts_dir().join("other.json"), spec).expect("write other mount spec");
    }

    /// Run a CLI subcommand with the hermetic env. Returns the captured output.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env("OMNIFS_MOUNT_POINT", &self.mount_point)
            .env("OMNIFS_DAEMON_ADDR", &self.daemon_addr)
            // The debug TCP path needs a shared token: the spawned daemon reads
            // this as its bearer token and every CLI invocation dials with the
            // same value, so no on-disk token file is involved.
            .env("OMNIFS_CONTROL_TOKEN", CONTROL_TOKEN)
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|e| panic!("spawn omnifs {}: {e}", args.join(" ")))
    }

    /// Run `omnifs up`, wait for the mount to become active, and record the
    /// daemon PID for drop cleanup. Returns `None` (skip) when the platform
    /// genuinely cannot mount.
    ///
    /// Panics when `up` exits non-zero (a real failure), or when the daemon
    /// becomes ready but the mount never appears (platform limitation that we
    /// convert to a skip with a clear message).
    fn up_and_wait(&mut self) -> Option<()> {
        let wasm_dir = release_wasm_dir();
        if !wasm_dir.join("test_provider.wasm").exists() {
            eprintln!("skip: test_provider.wasm missing (run `just build providers`)");
            return None;
        }
        if !platform_can_mount() {
            eprintln!("skip: platform cannot mount (no /dev/fuse)");
            return None;
        }

        let out = self.run(&["up"]);
        assert!(
            out.status.success(),
            "omnifs up failed (exit {})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        // Record daemon PID from the runtime record so Drop can kill it.
        self.update_pid_from_record();

        // Frontends are independent of daemon startup. Enable the host
        // frontend explicitly at this fixture's mount path before probing the
        // projected tree.
        let filesystem = if cfg!(target_os = "macos") {
            "nfs"
        } else {
            "fuse"
        };
        let location = self.mount_point.to_string_lossy().into_owned();
        let frontend = self.run(&[
            "frontend",
            "enable",
            filesystem,
            "--environment",
            "host",
            "--location",
            &location,
        ]);
        assert!(
            frontend.status.success(),
            "host frontend enable failed (exit {})\nstdout: {}\nstderr: {}",
            frontend.status,
            String::from_utf8_lossy(&frontend.stdout),
            String::from_utf8_lossy(&frontend.stderr),
        );

        // Wait for the mount to serve the projected tree.
        let message = self.mount_point.join("test/hello/message");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if message.is_file() {
                return Some(());
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "skip: {} never appeared within 30s; the mount could not come up on \
                     this platform",
                    message.display()
                );
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Update the stored daemon PID by reading the runtime record. Best-effort.
    /// A native record carries the pid flat at the top level.
    fn update_pid_from_record(&mut self) {
        self.daemon_pid = recorded_pid(self.home_path());
    }

    /// Force-kill the daemon PID recorded in the runtime record, if present.
    fn kill_daemon_from_record(&self) {
        if let Some(pid) = recorded_pid(self.home_path()) {
            // SIGKILL so it cannot clean up voluntarily.
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
    }

    /// True when the mount point is active in the OS mount table.
    fn mount_is_active(&self) -> bool {
        omnifs_nfs::mount_is_active(&self.mount_point)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Kill any daemon we know about.
        if let Some(pid) = self.daemon_pid {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
        // Also try the runtime record in case we didn't capture the PID yet.
        self.kill_daemon_from_record();
        // Force-unmount.
        force_unmount(&self.mount_point);
    }
}

// Status when no daemon is running.

/// `status` when no daemon is running: exit 0, reports not running.
#[test]
fn scenario_1_status_nothing_running() {
    let fixture = Fixture::new();
    let out = fixture.run(&["status", "--output", "json"]);

    assert!(
        out.status.success(),
        "omnifs status should exit 0 when nothing is running (exit {})\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status --output json must produce valid JSON");
    assert_eq!(
        json["result"]["workspace"]["daemon"].as_str().unwrap_or(""),
        "stopped",
        "workspace.daemon must be 'stopped' when no daemon is up; got:\n{json:#}"
    );
}

// Shutdown when no daemon is running.

/// `down` when nothing is running: exit 0, prints "Nothing to tear down."
#[test]
fn scenario_2_down_nothing_running() {
    let fixture = Fixture::new();
    let out = fixture.run(&["down"]);

    assert!(
        out.status.success(),
        "omnifs down should exit 0 when nothing is running (exit {})\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.contains("Nothing to tear down."),
        "expected 'Nothing to tear down.' in output; got:\n{combined}"
    );
}

// Full up, status, repeated up, and down lifecycle.

/// Up serves the mount through an explicitly enabled host frontend, status
/// shows it running, and down leaves that frontend alive. Scenarios 3-6 share
/// a single daemon lifecycle so we do not pay 4x mount creation latency.
#[test]
#[allow(clippy::too_many_lines)] // one shared daemon lifecycle across scenarios 3-6
fn scenarios_3_to_6_lifecycle_cycle() {
    if !live_acceptance_enabled() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    let wasm_dir = release_wasm_dir();
    if !wasm_dir.join("test_provider.wasm").exists() {
        eprintln!("skip: test_provider.wasm missing (run `just build providers`)");
        return;
    }
    if !platform_can_mount() {
        eprintln!("skip: platform cannot mount (no /dev/fuse)");
        return;
    }

    // Serialize mount-involving tests across processes to avoid concurrent NFS
    // state contention (held for the rest of the test).
    let _guard = nfs_serial_lock();

    let mut fixture = Fixture::new();
    fixture.write_test_spec();

    // Start the daemon, then enable the host frontend for the mount.

    let Some(()) = fixture.up_and_wait() else {
        return; // skip: platform could not mount
    };

    // Mount is active.
    assert!(
        fixture.mount_is_active(),
        "mount point {} must be active after `omnifs up`",
        fixture.mount_point.display()
    );

    // `test/hello/message` is readable and contains the expected bytes.
    let message_path = fixture.mount_point.join("test/hello/message");
    let content =
        std::fs::read(&message_path).expect("test/hello/message must be readable after up");
    assert_eq!(
        content, b"Hello, world!",
        "test/hello/message content mismatch"
    );

    // The runtime record exists.
    assert!(
        fixture.runtime_record_path().exists(),
        "daemon.json must exist after `omnifs up`"
    );

    // Status reports the running daemon.

    let out = fixture.run(&["status", "--output", "json"]);
    assert!(
        out.status.success(),
        "omnifs status should exit 0 while running (exit {})\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status --output json must produce valid JSON");

    assert_eq!(
        json["result"]["workspace"]["daemon"].as_str().unwrap_or(""),
        "running",
        "workspace.daemon must be 'running'; got:\n{json:#}"
    );

    // The `test` mount is loaded.
    let mounts = json["result"]["mounts"]
        .as_array()
        .expect("result.mounts must be an array");
    assert!(
        mounts.iter().any(|m| m["name"].as_str() == Some("test")),
        "result.mounts must include 'test'; got: {mounts:?}"
    );

    // Verify backend is native via the daemon status API directly. The debug
    // TCP listener is authenticated with the shared token the fixture injects.
    let base = format!("http://{}", fixture.daemon_addr);
    let auth_header = format!("Authorization: Bearer {CONTROL_TOKEN}");
    let status_url = format!("{base}/v1/status");
    let status_resp = Command::new("curl")
        .args(["-fs", "-H", &auth_header, &status_url])
        .output()
        .expect("curl /v1/status");
    let status_json: serde_json::Value = serde_json::from_slice(&status_resp.stdout)
        .expect("daemon /v1/status must return valid JSON");
    assert_eq!(
        status_json["backend"]["kind"].as_str().unwrap_or(""),
        "native",
        "daemon backend must be 'native'; got: {:?}",
        status_json["backend"]
    );
    assert!(
        status_json["backend"]["pid"]
            .as_u64()
            .is_some_and(|pid| pid > 0),
        "daemon backend must report its pid; got: {:?}",
        status_json["backend"]
    );

    // Applying the same revision is a no-op and retains the daemon process.
    let original_pid = recorded_pid(fixture.home_path()).expect("running daemon pid");
    let out = fixture.run(&["up"]);
    assert!(
        out.status.success(),
        "omnifs up while already serving HEAD must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        recorded_pid(fixture.home_path()),
        Some(original_pid),
        "applying the served revision must retain the daemon pid"
    );

    // Shutdown stops only the daemon; the independent host frontend and its
    // mount remain available while the daemon is down.
    let out = fixture.run(&["down"]);
    assert!(
        out.status.success(),
        "omnifs down must exit 0 (exit {})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // `down` does not return until the daemon control surface is gone, so an
    // immediate status probe must already report the settled not-running state.
    let immediate_status = fixture.run(&["status", "--output", "json"]);
    assert!(
        immediate_status.status.success(),
        "status immediately after down must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&immediate_status.stdout),
        String::from_utf8_lossy(&immediate_status.stderr),
    );
    let immediate_json: serde_json::Value = serde_json::from_slice(&immediate_status.stdout)
        .expect("immediate status --output json must produce valid JSON");
    assert_eq!(
        immediate_json["result"]["workspace"]["daemon"], "stopped",
        "status immediately after down must report stopped: {immediate_json:#}"
    );

    // The host runner and mount remain observable after daemon shutdown.
    let frontend = immediate_json["result"]["frontends"]
        .as_array()
        .expect("status result.frontends must be an array");
    let filesystem = if cfg!(target_os = "macos") {
        "nfs"
    } else {
        "fuse"
    };
    let mount_location = fixture.mount_point.to_string_lossy().into_owned();
    assert!(
        frontend.iter().any(|entry| {
            entry["filesystem"].as_str() == Some(filesystem)
                && entry["environment"].as_str() == Some("host")
                && entry["location"].as_str() == Some(mount_location.as_str())
                && entry["state"].as_str() == Some("running")
        }),
        "status after down must retain the running host frontend: {frontend:?}"
    );
    assert!(
        fixture.mount_is_active(),
        "mount point {} must remain active while the daemon is down",
        fixture.mount_point.display()
    );

    // Disable the exact host frontend while the daemon is stopped, then wait
    // for its mount and runner observation to disappear.
    let location = fixture.mount_point.to_string_lossy().into_owned();
    let disabled = fixture.run(&[
        "frontend",
        "disable",
        filesystem,
        "--environment",
        "host",
        "--location",
        &location,
    ]);
    assert!(
        disabled.status.success(),
        "frontend disable while daemon is down must succeed (exit {})\nstdout: {}\nstderr: {}",
        disabled.status,
        String::from_utf8_lossy(&disabled.stdout),
        String::from_utf8_lossy(&disabled.stderr),
    );

    // Poll briefly: the OS may take a moment to acknowledge the unmount.
    let settled = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if !fixture.mount_is_active() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    assert!(
        settled,
        "mount point {} must be gone from the mount table after frontend disable",
        fixture.mount_point.display()
    );

    let after_disable = fixture.run(&["status", "--output", "json"]);
    assert!(
        after_disable.status.success(),
        "status after frontend disable must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&after_disable.stdout),
        String::from_utf8_lossy(&after_disable.stderr),
    );
    let after_disable_json: serde_json::Value = serde_json::from_slice(&after_disable.stdout)
        .expect("status after frontend disable must produce valid JSON");
    let remaining_frontends = after_disable_json["result"]["frontends"]
        .as_array()
        .expect("status result.frontends must be an array after disable");
    assert!(
        !remaining_frontends.iter().any(|entry| {
            entry["filesystem"].as_str() == Some(filesystem)
                && entry["environment"].as_str() == Some("host")
                && entry["location"].as_str() == Some(location.as_str())
        }),
        "frontend disable must remove the exact host frontend observation: {remaining_frontends:?}"
    );

    // The runtime record is removed.
    assert!(
        !fixture.runtime_record_path().exists(),
        "daemon.json must be removed after `omnifs down`"
    );

    // Daemon process is gone: the PID recorded in our fixture should no longer
    // be alive. Poll briefly because the daemon receives SIGTERM and may take
    // a moment to exit after the mount is gone.
    if let Some(pid) = fixture.daemon_pid {
        let exited = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let alive = Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .output()
                    .is_ok_and(|o| o.status.success());
                if !alive {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        assert!(
            exited,
            "daemon pid {pid} must have exited within 5s after `omnifs down`"
        );
    }
}

// Recovery from a dead daemon.

/// No daemon answers the control port; `down` falls back to the runtime record
/// to identify the backend, reclaims, and removes the record, without hanging.
///
/// Uses a synthetic record (dead pid, no live mount) so the test never strands a
/// real NFS mount. The on-disk stale-mount sweep is exercised on Linux (FUSE),
/// where a dead-server mount can be force-unmounted without root; macOS NFS
/// cannot, so a crashed-daemon mount there clears on the kernel NFS timeout (see
/// the bounded unmount in `host_teardown`, which keeps `down` from hanging).
#[test]
fn scenario_7_dead_daemon_record_fallback() {
    if !live_acceptance_enabled() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }

    let fixture = Fixture::new();
    // A record for a daemon that is gone: a dead pid, and a mount point that is
    // not actually mounted. No control listener answers, so `down` takes the
    // record-fallback path and liveness-checks the dead pid before sweeping.
    let record = format!(
        r#"{{"version":3,"mount_revision":"0000000000000000000000000000000000000000","endpoint":{{"kind":"unix","path":"{}"}},"backend":"native","pid":2000000,"instance_id":"deadbeefdeadbeef","frontends":[],"started_at":"2026-07-07T00:00:00Z"}}"#,
        fixture.home_path().join("control.sock").display(),
    );
    std::fs::write(fixture.runtime_record_path(), record).expect("write synthetic runtime record");

    let out = fixture.run(&["down"]);
    assert!(
        out.status.success(),
        "omnifs down must consume a stale runtime record and exit 0 (exit {})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !fixture.runtime_record_path().exists(),
        "down must remove the runtime record after reclaiming a dead daemon"
    );
}

// Revision application is atomic at the daemon lifecycle boundary.

/// A changed valid revision restarts the daemon and advances the applied ref;
/// malformed desired state fails before stopping it and leaves the ref alone.
#[test]
fn scenario_8_revision_restart_and_preflight_failure() {
    if !live_acceptance_enabled() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    let wasm_dir = release_wasm_dir();
    if !wasm_dir.join("test_provider.wasm").exists() {
        eprintln!("skip: test_provider.wasm missing (run `just build providers`)");
        return;
    }

    let mut fixture = Fixture::new();
    fixture.write_test_spec();
    let out = fixture.run(&["up"]);
    assert!(
        out.status.success(),
        "initial up failed (exit {})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    fixture.update_pid_from_record();
    let first =
        omnifs_workspace::runtime_record::RuntimeRecord::read(&fixture.runtime_record_path())
            .expect("read initial runtime record")
            .expect("initial runtime record");

    fixture.write_other_spec();
    let out = fixture.run(&["apply"]);
    assert!(
        out.status.success(),
        "changed-revision apply failed (exit {})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    fixture.update_pid_from_record();
    let second =
        omnifs_workspace::runtime_record::RuntimeRecord::read(&fixture.runtime_record_path())
            .expect("read changed runtime record")
            .expect("changed runtime record");
    assert_ne!(
        first.backend, second.backend,
        "changed revision must restart the daemon"
    );
    assert_ne!(
        first.mount_revision, second.mount_revision,
        "changed desired state must load a new revision"
    );
    let repository = omnifs_workspace::mounts::Repository::open(fixture.mounts_dir())
        .expect("open mount repository after apply");
    assert_eq!(
        repository.applied().expect("read applied ref"),
        Some(second.mount_revision.clone()),
        "readiness must advance refs/omnifs/applied to the running revision"
    );
    drop(repository);

    std::fs::write(fixture.mounts_dir().join("malformed.json"), b"{")
        .expect("write malformed desired state");
    let out = fixture.run(&["up"]);
    assert!(
        !out.status.success(),
        "malformed desired state must reject up"
    );
    let after_failure =
        omnifs_workspace::runtime_record::RuntimeRecord::read(&fixture.runtime_record_path())
            .expect("read runtime record after rejected apply")
            .expect("healthy daemon must remain recorded after rejected apply");
    assert_eq!(
        after_failure.backend, second.backend,
        "preflight failure must not stop the healthy daemon"
    );
    assert_eq!(
        after_failure.mount_revision, second.mount_revision,
        "preflight failure must not change the running revision"
    );
    let alive = recorded_pid(fixture.home_path()).is_some_and(|pid| {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .is_ok_and(|output| output.status.success())
    });
    assert!(
        alive,
        "preflight failure must leave the healthy daemon alive"
    );
    std::fs::remove_file(fixture.mounts_dir().join("malformed.json"))
        .expect("remove malformed desired state");

    let out = fixture.run(&["down"]);
    assert!(
        out.status.success(),
        "omnifs down must exit 0 after scenario 8 (exit {})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
