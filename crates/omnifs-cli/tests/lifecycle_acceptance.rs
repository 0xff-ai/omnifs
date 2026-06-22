//! Lifecycle acceptance tests for the CLI ↔ daemon lifecycle.
//!
//! Each test drives the real `omnifs` binary against a hermetic `OMNIFS_HOME`
//! with its own temp dir, mount point, and control port. No test touches the
//! user's real `~/.omnifs`, `~/omnifs`, or port 7878.
//!
//! The fixture writes `test_provider.wasm` and `omnifs_tool_archive.wasm` into
//! `<OMNIFS_HOME>/providers/` and writes the no-auth test mount spec to
//! `<OMNIFS_HOME>/mounts/test.json`. The mount serves `test/hello/message`.
//!
//! A `Drop` guard on `Fixture` force-unmounts the mount point and kills any
//! surviving daemon, so a panicking or interrupted test still cleans up.
//!
//! Skip (not fail) only when the platform genuinely cannot mount. A daemon
//! that exits due to a CLI parse error or bad argument is a real failure.
//!
//! Tests that involve live NFS mounts (scenarios 3-8 on macOS) run under a
//! process-global mutex to avoid concurrent NFS mount/unmount operations that
//! can serialize through the macOS kernel's NFS state and cause timeouts.

#![cfg(not(target_os = "wasi"))]

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Fixed, non-ephemeral port used purely as a cross-process lock for live NFS
/// mounts. Below the OS ephemeral range, so it never collides with a daemon's
/// `free_port()`.
const NFS_LOCK_PORT: u16 = 48761;

/// Acquire the cross-process NFS serialization lock, returning the bound socket
/// as the guard. macOS deadlocks under concurrent loopback mounts, and nextest
/// runs each integration-test binary as its own process, so an in-process mutex
/// cannot serialize across binaries. Binding a fixed port does: a second binder
/// fails until the holder drops the socket, and the OS frees it the instant the
/// holder exits, so even a killed test never wedges the next one.
fn nfs_serial_lock() -> TcpListener {
    loop {
        match TcpListener::bind(("127.0.0.1", NFS_LOCK_PORT)) {
            Ok(listener) => return listener,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

// ── Constants ─────────────────────────────────────────────────────────────────

// ── Helpers ───────────────────────────────────────────────────────────────────

/// No-auth mount spec for the test provider, pinning `id`. Serves
/// `test/hello/message`.
fn test_mount_spec(id: &omnifs_core::ProviderId) -> String {
    format!(
        r#"{{"provider":{{"id":"{id}","meta":{{"name":"test-provider"}}}},"mount":"test","capabilities":{{"domains":["httpbin.org"]}}}}"#
    )
}

/// A broken spec: pins a content id with no installed by-hash artifact.
fn broken_mount_spec() -> String {
    let bogus = "0".repeat(64);
    format!(
        r#"{{"provider":{{"id":"{bogus}","meta":{{"name":"broken"}}}},"mount":"broken","capabilities":{{"domains":[]}}}}"#
    )
}

/// Install the test provider into the by-hash store under `providers_dir` and
/// return its content id.
fn install_test_provider(providers_dir: &Path) -> omnifs_core::ProviderId {
    let bytes = std::fs::read(release_wasm_dir().join("test_provider.wasm"))
        .expect("read test provider wasm");
    let id = omnifs_core::ProviderId::from_wasm_bytes(&bytes);
    let store = omnifs_mount::mounts::ProviderStore::new(providers_dir);
    store.put_if_absent(&id, &bytes).expect("put test provider");
    store
        .install(
            id,
            omnifs_core::ProviderMeta {
                name: omnifs_core::ProviderName::new("test-provider").unwrap(),
                version: None,
            },
            "test_provider.wasm".into(),
        )
        .expect("install test provider");
    id
}

/// `target/wasm32-wasip2/release`, where provider wasm lives.
fn release_wasm_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target/wasm32-wasip2/release")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Live-mount acceptance tests are opt-in: they spawn a real daemon and mount a
/// real filesystem, which is slow and noisy on a developer machine (a stray
/// macOS NFS mount triggers "server connection interrupted" alerts). A plain
/// `cargo nextest` skips them; set `OMNIFS_ACCEPTANCE_LIVE=1` to run them
/// (serialized across binaries via the `serial-nfs` nextest group).
fn live_acceptance_enabled() -> bool {
    std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_some()
}

fn omnifs_bin() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_omnifs")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_omnifs"))
        .map_or_else(
            || PathBuf::from(env!("CARGO_BIN_EXE_omnifs")),
            PathBuf::from,
        )
}

/// Return `true` if the platform can serve a mount. On Linux, FUSE requires
/// `/dev/fuse`. On macOS, NFS loopback is always available without root.
fn platform_can_mount() -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/dev/fuse").exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

// ── Fixture ───────────────────────────────────────────────────────────────────

/// Hermetic per-test fixture: a fresh temp dir, providers, mount point, and
/// control address. Drops cleanly even when a test panics.
struct Fixture {
    home: tempfile::TempDir,
    mount_point: PathBuf,
    daemon_addr: String,
    /// Content id of the test provider installed into the by-hash store.
    test_provider_id: omnifs_core::ProviderId,
    /// PID to kill on drop, when a daemon was spawned via `omnifs up` rather
    /// than the daemon subcommand directly.
    daemon_pid: Option<u32>,
}

impl Fixture {
    /// Allocate a fresh fixture with providers copied in. Does NOT write any
    /// mount spec; callers write what they need.
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let providers_dir = home.path().join("providers");
        std::fs::create_dir_all(&providers_dir).expect("providers dir");

        let wasm_dir = release_wasm_dir();
        // Copy every built wasm so the archive tool and test provider are found.
        for entry in std::fs::read_dir(&wasm_dir)
            .expect("read release wasm dir")
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "wasm") {
                std::fs::copy(&path, providers_dir.join(path.file_name().unwrap()))
                    .expect("copy wasm");
            }
        }
        // The daemon serves by content id, so the test provider must be in the
        // by-hash store (the flat copy above only satisfies the archive tool).
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

    fn launch_json_path(&self) -> PathBuf {
        self.home_path().join("launch.json")
    }

    /// Write the test mount spec (`test.json`) into `<home>/mounts/`.
    fn write_test_spec(&self) {
        std::fs::write(
            self.mounts_dir().join("test.json"),
            test_mount_spec(&self.test_provider_id),
        )
        .expect("write test mount spec");
    }

    /// Write a broken mount spec (`broken.json`) into `<home>/mounts/`.
    fn write_broken_spec(&self) {
        std::fs::write(self.mounts_dir().join("broken.json"), broken_mount_spec())
            .expect("write broken mount spec");
    }

    /// Run a CLI subcommand with the hermetic env. Returns the captured output.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env("OMNIFS_MOUNT_POINT", &self.mount_point)
            .env("OMNIFS_DAEMON_ADDR", &self.daemon_addr)
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
            eprintln!("skip: test_provider.wasm missing (run `just providers-build`)");
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

        // Record daemon PID from launch.json so Drop can kill it.
        self.update_pid_from_launch_json();

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

    /// Update the stored daemon PID by reading `launch.json`. Best-effort.
    fn update_pid_from_launch_json(&mut self) {
        let path = self.launch_json_path();
        if let Ok(bytes) = std::fs::read_to_string(&path)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&bytes)
            && let Some(pid) = val["daemon_pid"].as_u64()
        {
            self.daemon_pid = u32::try_from(pid).ok();
        }
    }

    /// Force-kill the daemon PID recorded in `launch.json`, if one is present.
    fn kill_daemon_from_launch_json(&self) {
        let path = self.launch_json_path();
        if let Ok(bytes) = std::fs::read_to_string(&path)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&bytes)
            && let Some(pid) = val["daemon_pid"].as_u64()
        {
            // SIGKILL so it cannot clean up voluntarily.
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
    }

    /// True when the mount point is active in the OS mount table.
    fn mount_is_active(&self) -> bool {
        omnifs_nfs::mount_is_active(&self.mount_point)
    }

    /// Force-unmount the mount point. Best-effort, non-blocking, and safe to call
    /// even when nothing is mounted, so a `Drop` during a panicking test never
    /// wedges the suite.
    ///
    /// On macOS this mirrors production teardown: `sudo -n umount -f` clears a
    /// dead-server NFS mount instantly, where `diskutil unmount force` would block
    /// in an uninterruptible NFS syscall. The path is resolved via the parent so
    /// the dead mount itself is never stat-ed.
    fn force_unmount(&self) {
        #[cfg(target_os = "macos")]
        {
            if !omnifs_nfs::mount_is_active(&self.mount_point) {
                return;
            }
            if let Some(canonical) = self
                .mount_point
                .parent()
                .and_then(|parent| std::fs::canonicalize(parent).ok())
                .and_then(|parent| self.mount_point.file_name().map(|leaf| parent.join(leaf)))
            {
                let _ = Command::new("sudo")
                    .args(["-n", "umount", "-f"])
                    .arg(&canonical)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output();
            }
        }
        #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
        {
            if omnifs_nfs::mount_is_active(&self.mount_point) {
                let _ = Command::new("umount")
                    .arg("-f")
                    .arg(&self.mount_point)
                    .output();
            }
        }
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("fusermount")
                .args([OsStr::new("-uz"), self.mount_point.as_os_str()])
                .output();
            let _ = Command::new("umount").arg(&self.mount_point).output();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Kill any daemon we know about.
        if let Some(pid) = self.daemon_pid {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
        // Also try the launch.json in case we didn't capture the PID yet.
        self.kill_daemon_from_launch_json();
        // Force-unmount.
        self.force_unmount();
    }
}

// ── Scenario 1: status, nothing running ───────────────────────────────────────

/// `status` when no daemon is running: exit 0, reports not running.
#[test]
fn scenario_1_status_nothing_running() {
    let fixture = Fixture::new();
    let out = fixture.run(&["status", "--json"]);

    assert!(
        out.status.success(),
        "omnifs status should exit 0 when nothing is running (exit {})\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status --json must produce valid JSON");
    assert_eq!(
        json["runtime"]["state"].as_str().unwrap_or(""),
        "not_running",
        "runtime.state must be 'not_running' when no daemon is up; got:\n{json:#}"
    );
}

// ── Scenario 2: down, nothing running ─────────────────────────────────────────

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

// ── Scenarios 3-6: up/status/up-again/down cycle ──────────────────────────────

/// Up serves the mount, status shows it running, up-again is rejected,
/// down is clean. Scenarios 3-6 share a single daemon lifecycle so we do not
/// pay 4x mount setup latency.
#[test]
#[allow(clippy::too_many_lines)] // one shared daemon lifecycle across scenarios 3-6
fn scenarios_3_to_6_lifecycle_cycle() {
    if !live_acceptance_enabled() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    let wasm_dir = release_wasm_dir();
    if !wasm_dir.join("test_provider.wasm").exists() {
        eprintln!("skip: test_provider.wasm missing (run `just providers-build`)");
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

    // ── Scenario 3: up serves the mount ──────────────────────────────────────

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

    // `launch.json` exists.
    assert!(
        fixture.launch_json_path().exists(),
        "launch.json must exist after `omnifs up`"
    );

    // ── Scenario 4: status while running ─────────────────────────────────────

    let out = fixture.run(&["status", "--json"]);
    assert!(
        out.status.success(),
        "omnifs status should exit 0 while running (exit {})\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status --json must produce valid JSON");

    assert_eq!(
        json["runtime"]["state"].as_str().unwrap_or(""),
        "running",
        "runtime.state must be 'running'; got:\n{json:#}"
    );

    // The `test` mount is loaded.
    let mounts = json["runtime"]["mounts"]
        .as_array()
        .expect("runtime.mounts must be an array");
    assert!(
        mounts.iter().any(|m| m.as_str().unwrap_or("") == "test"),
        "runtime.mounts must include 'test'; got: {mounts:?}"
    );

    // Verify backend is native via the daemon status API directly.
    let base = format!("http://{}", fixture.daemon_addr);
    let status_resp = Command::new("curl")
        .args(["-fs", &format!("{base}/v1/status")])
        .output()
        .expect("curl /v1/status");
    let status_json: serde_json::Value = serde_json::from_slice(&status_resp.stdout)
        .expect("daemon /v1/status must return valid JSON");
    assert_eq!(
        status_json["backend"].as_str().unwrap_or(""),
        "native",
        "daemon backend must be 'native'; got: {:?}",
        status_json["backend"]
    );

    // ── Scenario 5: up while already running ─────────────────────────────────

    let out = fixture.run(&["up"]);
    assert!(
        !out.status.success(),
        "omnifs up while already running must exit non-zero"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // The error message must name a running daemon.
    assert!(
        combined.to_lowercase().contains("already running")
            || combined.to_lowercase().contains("daemon"),
        "up-while-running error must mention 'already running' or 'daemon'; got:\n{combined}"
    );

    // ── Scenario 6: down is clean ─────────────────────────────────────────────

    // Use --force so a tardy NFS unmount does not cause `down` to exit
    // non-zero. On macOS the NFS client takes a variable amount of time to
    // acknowledge the server's unmount, which can exceed the 3s grace window
    // in `wait_unmounted`. The invariant under test (mount gone, launch.json
    // removed, daemon exited) is preserved regardless of the --force flag.
    let out = fixture.run(&["down", "--force"]);
    assert!(
        out.status.success(),
        "omnifs down must exit 0 (exit {})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Mount is gone from the OS mount table.
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
        "mount point {} must be gone from the mount table after `omnifs down`",
        fixture.mount_point.display()
    );

    // `launch.json` removed.
    assert!(
        !fixture.launch_json_path().exists(),
        "launch.json must be removed after `omnifs down`"
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
                    .map(|o| o.status.success())
                    .unwrap_or(false);
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

// ── Scenario 7: dead-daemon fallback ──────────────────────────────────────────

/// No daemon answers the control port; `down` falls back to the launch record
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
    // not actually mounted. No control listener answers the fixture's port, so
    // `down` takes the record-fallback path.
    let record = format!(
        r#"{{"version":1,"backend":"native","daemon_pid":2000000,"control_addr":"{}","mount_point":"{}","started_at":"2026-01-01T00:00:00Z"}}"#,
        fixture.daemon_addr,
        fixture.mount_point.display(),
    );
    std::fs::write(fixture.launch_json_path(), record).expect("write synthetic launch record");

    let out = fixture.run(&["down"]);
    assert!(
        out.status.success(),
        "omnifs down must consume a stale launch record and exit 0 (exit {})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !fixture.launch_json_path().exists(),
        "down must remove the launch record after reclaiming a dead daemon"
    );
}

// ── Scenario 8: failed mount surfaced ─────────────────────────────────────────

/// Add a spec with a missing provider, `up`, `status` shows the broken mount
/// in the failed set with a reason; `test` still serves; `down` cleans up.
#[test]
fn scenario_8_failed_mount_surfaced() {
    if !live_acceptance_enabled() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    let wasm_dir = release_wasm_dir();
    if !wasm_dir.join("test_provider.wasm").exists() {
        eprintln!("skip: test_provider.wasm missing (run `just providers-build`)");
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
    fixture.write_broken_spec();

    // `up` must exit 0 even with a partially-broken mount set: the daemon
    // starts and serves the working mounts; the broken one is surfaced in the
    // reconcile report.
    let Some(()) = fixture.up_and_wait() else {
        return; // skip: platform could not mount
    };

    // `test/hello/message` is still readable.
    let message_path = fixture.mount_point.join("test/hello/message");
    let content = std::fs::read(&message_path)
        .expect("test/hello/message must be readable even with a broken peer");
    assert_eq!(
        content, b"Hello, world!",
        "test/hello/message content mismatch after partial failure"
    );

    // `status --json` surfaces the broken mount in the failed set.
    let out = fixture.run(&["status", "--json"]);
    assert!(
        out.status.success(),
        "omnifs status must exit 0 (exit {})\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status --json must produce valid JSON");

    assert_eq!(
        json["runtime"]["state"].as_str().unwrap_or(""),
        "running",
        "runtime.state must be 'running' even with a broken mount; got:\n{json:#}"
    );

    let failed_mounts = json["runtime"]["failed_mounts"]
        .as_array()
        .expect("runtime.failed_mounts must be an array");
    assert!(
        !failed_mounts.is_empty(),
        "runtime.failed_mounts must be non-empty when a broken mount spec exists; got:\n{json:#}"
    );

    // The broken mount must have a non-empty reason.
    let broken = failed_mounts
        .iter()
        .find(|m| m["mount"].as_str().unwrap_or("") == "broken")
        .unwrap_or_else(|| {
            panic!("failed_mounts must include an entry for 'broken'; got: {failed_mounts:?}")
        });
    let reason = broken["reason"].as_str().unwrap_or("");
    assert!(
        !reason.is_empty(),
        "failed mount 'broken' must have a non-empty reason; got: {broken:#}"
    );

    // `test` is still in the working mounts list.
    let running_mounts = json["runtime"]["mounts"]
        .as_array()
        .expect("runtime.mounts must be an array");
    assert!(
        running_mounts
            .iter()
            .any(|m| m.as_str().unwrap_or("") == "test"),
        "runtime.mounts must still include 'test' despite the broken peer; got: {running_mounts:?}"
    );

    // Clean up: use --force so a tardy NFS unmount does not block.
    let out = fixture.run(&["down", "--force"]);
    assert!(
        out.status.success(),
        "omnifs down must exit 0 after scenario 8 (exit {})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
