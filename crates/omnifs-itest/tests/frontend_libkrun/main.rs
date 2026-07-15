//! The libkrun environment end to end: `omnifs frontend enable fuse --runtime libkrun` boots
//! the mkosi guest image under libkrun on Apple Silicon macOS, attaches the
//! guest's `omnifs-thin fuse` runner to a host-native daemon's shared namespace
//! over vsock, and serves `/omnifs` inside the guest. This suite proves the
//! guest mount behaves like a real filesystem for the standard toolbox (the
//! `fuse-libkrun` conformance column, reusing the shared matrix machinery
//! from `omnifs_itest::matrix`, exactly as `tests/frontend_docker` does for
//! the Docker-hosted frontend), plus lifecycle and teardown cleanliness.
//!
//! LOCAL-ONLY, never CI: GitHub-hosted macOS runners cannot nest
//! virtualization, so this suite can never boot libkrun there. It is gated on
//! **both** `cfg(target_os = "macos")` and the `OMNIFS_ACCEPTANCE_LIVE`
//! opt-in env var (the same convention the live NFS/Docker-frontend lanes
//! use), and prints a loud `skip:` line rather than silently passing when
//! either is absent. See `docs/contracts/60-build-validation.md` for the
//! exact command and the "why no CI" rationale, and `just libkrun-conformance`
//! for the wrapped invocation.
//!
//! Every row's matrix execution goes over ssh-over-vsock via the real
//! `omnifs shell -- <cmd>` CLI path (`matrix::Exec::SshLibkrun`), the same
//! command construction `LibkrunRunner::shell_command` builds for interactive
//! `omnifs shell`. One ssh connection per row (mirroring `frontend_docker`,
//! which is also one `docker exec` per row, not batched): a single libkrun
//! guest is fast enough over vsock+socat that batching bought nothing
//! measurable in a live run (see the report for the wall-clock total).
//!
//! Serializes against every other live-mount lane (NFS, wire, Docker-hosted
//! frontend) through the one cross-process lock this crate owns
//! (`omnifs_itest::live::nfs_serial_lock`), named for its original NFS-only
//! use but reused here as-is per this crate's "do not invent a second lock"
//! rule: nextest runs each integration-test binary as its own process, so an
//! in-process mutex cannot serialize across binaries, and a libkrun guest's
//! own vsock ports (fixed per-launch socket paths under `<config_dir>/libkrun/`)
//! would otherwise race a concurrent live lane the same way a second NFS mount
//! would.
//!
//! Never interrupt a running instance of this suite: like the live NFS lanes,
//! an interrupted run can leave a libkrun process or a host-native mount
//! orphaned for the next run to trip over.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use omnifs_itest::matrix::{self, Exec};
use omnifs_itest::{live, provider_artifact_dir};
use omnifs_workspace::daemon_record::DaemonRecord;
use tempfile::TempDir;

/// Scratch dir inside the libkrun guest for the matrix's copy/archive rows.
/// Distinct path namespace from `frontend_docker`'s `DOCKER_SCRATCH`, though
/// nothing would collide even if they matched: this is a different guest.
const GUEST_SCRATCH: &str = "/tmp/omnifs-matrix";

const ENV_GUEST_IMAGE: &str = "OMNIFS_GUEST_IMAGE";

fn acceptance_gated() -> bool {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run the libkrun acceptance gate");
        return false;
    }
    true
}

/// Every precondition this suite needs beyond the live-acceptance gate: an
/// Apple Silicon host (the guest image is arm64-only), the test provider
/// artifact, `libkrun` and `socat` on `PATH` (the driver and its ssh bridge
/// respectively — mirrors `LibkrunRunner::ensure_libkrun_available`/
/// `ensure_socat_available`, duplicated here as a probe rather than imported
/// since neither is a public `omnifs-cli` API), and the locally built guest
/// image. Returns the resolved guest image path, or `None` (skip, message
/// already printed) when any precondition is missing.
fn preconditions() -> Option<PathBuf> {
    if std::env::consts::ARCH != "aarch64" {
        eprintln!(
            "skip: libkrun guest image is arm64-only, this host is {}",
            std::env::consts::ARCH
        );
        return None;
    }
    let test_wasm = provider_artifact_dir().join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just build providers`)",
            test_wasm.display()
        );
        return None;
    }
    if !command_reachable("krunkit", &["--version"]) {
        eprintln!(
            "skip: the krunkit executable is not on PATH (`brew tap slp/krun && brew install krunkit`)"
        );
        return None;
    }
    if !command_reachable("socat", &["-V"]) {
        eprintln!("skip: socat not on PATH (`brew install socat`)");
        return None;
    }
    let image = guest_image_path();
    if !image.is_file() {
        eprintln!(
            "skip: guest image missing at {} (run `just guest-image`)",
            image.display()
        );
        return None;
    }
    Some(image)
}

fn command_reachable(program: &str, probe_args: &[&str]) -> bool {
    Command::new(program)
        .args(probe_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Resolve the guest image the same way `libkrun_runner::resolve_guest_image`
/// does for its default case: an explicit `OMNIFS_GUEST_IMAGE` override, else
/// `just guest-image`'s default output path resolved against the workspace
/// root (not the current working directory: a test binary's cwd is its own
/// crate directory, not the workspace root the CLI assumes when run from a
/// contributor's shell, so this suite resolves the absolute path itself and
/// hands it to every CLI invocation via `OMNIFS_GUEST_IMAGE` rather than
/// relying on cwd-relative resolution matching by accident).
fn guest_image_path() -> PathBuf {
    if let Some(path) = std::env::var_os(ENV_GUEST_IMAGE) {
        return PathBuf::from(path);
    }
    workspace_root().join("target/guest-image/omnifs-guest.raw")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

// ===========================================================================
// Fixture: a hermetic workspace driving the real `omnifs` CLI end to end.
// ===========================================================================

/// Drives the real `omnifs` binary against a hermetic `OMNIFS_HOME`, exactly
/// as a contributor would: `up`, explicit host and libkrun frontend enable,
/// `frontend ls`, explicit frontend disable, `down`. No test touches the user's real
/// `~/.omnifs` or default ports.
struct Fixture {
    home: TempDir,
    mount_point: PathBuf,
    guest_image: PathBuf,
    daemon_pid: Option<u32>,
}

impl Fixture {
    fn new(guest_image: PathBuf) -> Self {
        let live::HermeticHome { home, mount_point } = live::hermetic_home();
        Self {
            home,
            mount_point,
            guest_image,
            daemon_pid: None,
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn libkrun_dir(&self) -> PathBuf {
        self.home_path().join("libkrun")
    }

    fn record_path(&self) -> PathBuf {
        self.home_path().join("daemon.json")
    }

    fn record(&self) -> Option<DaemonRecord> {
        DaemonRecord::read(&self.record_path()).ok().flatten()
    }

    fn daemon_pid_from_record(&self) -> Option<u32> {
        Some(self.record()?.pid)
    }

    /// The libkrun guest's own pid, read from its pidfile if present.
    fn libkrun_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.libkrun_dir().join("libkrun.pid"))
            .ok()
            .and_then(|contents| contents.trim().parse().ok())
    }

    /// Run a CLI subcommand with the hermetic env, including the guest image
    /// override so libkrun frontend enable never falls back to a
    /// cwd-relative default.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(live::omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env(ENV_GUEST_IMAGE, &self.guest_image)
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    /// Bring up a host-native daemon, explicitly enable its host frontend, and
    /// wait for it to serve the test-provider tree. Panics on a real failure: every
    /// environment gap (missing wasm, unmountable platform) was already
    /// checked by [`preconditions`] before the fixture was built.
    fn up_native(&mut self) {
        let out = self.run(&["up"]);
        assert!(
            out.status.success(),
            "omnifs up failed (exit {})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        self.daemon_pid = self.daemon_pid_from_record();

        let location = self.mount_point.to_str().expect("mount point utf8");
        let out = self.run(&[
            "frontend",
            "enable",
            "nfs",
            "--runtime",
            "host",
            "--location",
            location,
        ]);
        assert!(
            out.status.success(),
            "host NFS frontend enable failed (exit {})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let message = self.mount_point.join("test/hello/message");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !message.is_file() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared within 30s after `omnifs up`",
                message.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn frontend_enable(&self) -> Output {
        self.run(&["frontend", "enable", "fuse", "--runtime", "libkrun"])
    }

    /// Assert libkrun frontend enable succeeded; on failure, dump the libkrun serial
    /// console log first (the fixture Drop removes the whole `libkrun/` dir,
    /// so this is the only window to capture why the guest never served),
    /// then panic with the CLI's own output.
    fn assert_frontend_enable_ok(&self, out: &Output, context: &str) {
        if out.status.success() {
            return;
        }
        let serial = self.libkrun_dir().join("serial.log");
        if let Ok(log) = std::fs::read_to_string(&serial) {
            let tail: String = log
                .lines()
                .rev()
                .take(80)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            eprintln!("--- {} (tail) ---\n{tail}\n---", serial.display());
        }
        panic!(
            "omnifs frontend enable fuse --runtime libkrun failed ({context}, exit {})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    fn frontend_status(&self) -> Output {
        self.run(&["frontend", "ls"])
    }

    fn down(&self) -> Output {
        self.run(&["down"])
    }

    /// Every artifact `LibkrunRunner::launch` can lay down under
    /// `<config_dir>/libkrun/`. Used both to prove teardown removed them and,
    /// before that, to prove frontend enable created them.
    fn libkrun_artifacts(&self) -> Vec<PathBuf> {
        let dir = self.libkrun_dir();
        [
            "libkrun.pid",
            "seed.iso",
            "ssh.sock",
            "ready.sock",
            "restful.sock",
            "serial.log",
        ]
        .into_iter()
        .map(|name| dir.join(name))
        .filter(|path| path.exists())
        .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(pid) = self.libkrun_pid() {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
        if let Some(pid) = self.daemon_pid.or_else(|| self.daemon_pid_from_record()) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
        force_unmount(&self.mount_point);
    }
}

/// Clear a possibly-dead-server mount without blocking. Mirrors
/// `frontend_docker`'s own `force_unmount`: on macOS `sudo -n umount -f`
/// clears a dead-server NFS mount instantly, where `diskutil unmount force`
/// blocks in an uninterruptible NFS syscall. A no-op when nothing is mounted
/// (the common case after explicit frontend disable).
fn force_unmount(mount_point: &Path) {
    let unmount_once = || {
        let Some(canonical) = mount_point
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .and_then(|parent| mount_point.file_name().map(|leaf| parent.join(leaf)))
        else {
            return;
        };
        let _ = Command::new("sudo")
            .args(["-n", "umount", "-f"])
            .arg(&canonical)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    };

    let deadline = Instant::now() + Duration::from_secs(15);
    while omnifs_nfs::mount_is_active(mount_point) {
        unmount_once();
        if !omnifs_nfs::mount_is_active(mount_point) || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// `omnifs shell -- cat /omnifs/<mount>/hello/message` returns exact fixture
/// bytes for every configured mount.
fn assert_serves(fixture: &Fixture) {
    for root in ["test", "test2"] {
        let guest_path = format!("/omnifs/{root}/hello/message");
        let out = fixture.run(&["shell", "--", "cat", &guest_path]);
        assert!(
            out.status.success(),
            "omnifs shell -- cat {guest_path} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "Hello, world!");
    }
}

// ===========================================================================
// Cold start: libkrun-run-to-served-mount, recorded not gated
// ===========================================================================

/// A libkrun microVM boots a real kernel to multi-user systemd before its
/// frontend runner can even attach — categorically slower and more
/// host-load-dependent than a container start, and observed locally in the
/// 4-15s range rather than `fuse-docker`'s sub-5s container budget. A fixed
/// wall-clock gate at that range would be flaky across developer machines
/// (thermal throttling, concurrent VMs, a cold libkrun binary page-in), so
/// this metric is recorded for trend-watching but never asserted against; a
/// budget regression is a design conversation, not a test failure.
#[derive(serde::Serialize)]
struct ColdStart {
    version: u32,
    generated_at: String,
    metric: &'static str,
    duration_ms: u64,
}

fn record_cold_start(duration: Duration) {
    let report = ColdStart {
        version: 1,
        generated_at: now_rfc3339(),
        metric: "libkrun-boot-to-served-mount",
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
    };
    let path = matrix::scorecard_dir().join("cold-start-fuse-libkrun.json");
    let json = serde_json::to_string_pretty(&report).expect("serialize cold-start report");
    std::fs::write(&path, json).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    eprintln!(
        "cold-start-fuse-libkrun: {} ({} ms, recorded not gated)",
        path.display(),
        report.duration_ms,
    );
}

fn now_rfc3339() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

// ===========================================================================
// Test 1: lifecycle, cold start, the matrix, teardown
// ===========================================================================

#[test]
fn libkrun_lifecycle_and_matrix() {
    if !acceptance_gated() {
        return;
    }
    let Some(guest_image) = preconditions() else {
        return;
    };

    // Serialize against every other live-mount lane (NFS, wire, Docker-hosted
    // frontend): held for this test's whole lifetime.
    let _nfs_lock = live::nfs_serial_lock();
    let mut fixture = Fixture::new(guest_image);
    fixture.up_native();

    // Cold start: the daemon is already warm, so the timed span isolates
    // libkrun-boot-to-served-mount latency from daemon bring-up cost.
    let started = Instant::now();
    let up_out = fixture.frontend_enable();
    let elapsed = started.elapsed();
    fixture.assert_frontend_enable_ok(&up_out, "cold start");
    record_cold_start(elapsed);

    // `frontend ls` is truthful.
    let status_out = fixture.frontend_status();
    assert!(status_out.status.success());
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.contains("libkrun") && status_text.contains("attached"),
        "frontend ls must report the libkrun guest attached: {status_text}"
    );

    assert_serves(&fixture);

    // The libkrun runner's own launch-time lockdown audit
    // (`assert_libkrun_locked_down`) already proves the device set from
    // inside `omnifs-cli`; this suite's job is the guest-visible conformance
    // contract, not re-proving that audit from outside.

    let mkdir_out = fixture.run(&["shell", "--", "mkdir", "-p", GUEST_SCRATCH]);
    assert!(
        mkdir_out.status.success(),
        "omnifs shell -- mkdir -p {GUEST_SCRATCH} failed: {}",
        String::from_utf8_lossy(&mkdir_out.stderr)
    );

    // The fuse-libkrun matrix column, through the shared row/executor
    // machinery, over ssh-over-vsock via the real `omnifs shell -- <cmd>`
    // path.
    let exec = Exec::SshLibkrun {
        omnifs_bin: live::omnifs_bin(),
        home: fixture.home_path().to_path_buf(),
        root: "/omnifs/test".to_string(),
        scratch: GUEST_SCRATCH.to_string(),
    };
    let scorecard = matrix::run_column(&exec, &matrix::FUSE_LIBKRUN_FRONTEND, matrix::ROWS);
    let scorecard_path = matrix::write_scorecard(&scorecard);
    eprintln!("scorecard: {}", scorecard_path.display());
    eprintln!(
        "\n{}",
        matrix::render_table(std::slice::from_ref(&scorecard))
    );
    let mismatches = matrix::mismatches(&scorecard);
    assert!(
        mismatches.is_empty(),
        "fuse-libkrun column has {} expectation mismatch(es):\n  {}",
        mismatches.len(),
        mismatches.join("\n  ")
    );

    // Frontend runners have independent lifecycles. Disable the libkrun and
    // host runners explicitly, then stop the daemon with `omnifs down`.
    // The host-native NFS mount can be transiently busy at shutdown because
    // macOS spawns indexer handles like mds/mdworker against a fresh mount.
    let libkrun_disabled = fixture.run(&["frontend", "disable", "fuse", "--runtime", "libkrun"]);
    assert!(
        libkrun_disabled.status.success(),
        "disabling libkrun frontend before down failed (exit {})\nstdout: {}\nstderr: {}",
        libkrun_disabled.status,
        String::from_utf8_lossy(&libkrun_disabled.stdout),
        String::from_utf8_lossy(&libkrun_disabled.stderr),
    );
    let host_location = fixture.mount_point.to_str().expect("mount point utf8");
    let host_disabled = fixture.run(&[
        "frontend",
        "disable",
        "nfs",
        "--runtime",
        "host",
        "--location",
        host_location,
    ]);
    assert!(
        host_disabled.status.success(),
        "disabling host frontend before down failed (exit {})\nstdout: {}\nstderr: {}",
        host_disabled.status,
        String::from_utf8_lossy(&host_disabled.stdout),
        String::from_utf8_lossy(&host_disabled.stderr),
    );

    let mut down_out = fixture.down();
    for _ in 0..3 {
        if down_out.status.success() {
            break;
        }
        let stderr = String::from_utf8_lossy(&down_out.stderr).to_string();
        if !stderr.contains("still mounted") {
            break;
        }
        eprintln!("omnifs down: mount transiently busy, retrying: {stderr}");
        std::thread::sleep(Duration::from_secs(2));
        down_out = fixture.down();
    }
    assert!(
        down_out.status.success(),
        "omnifs down failed (exit {})\nstdout: {}\nstderr: {}",
        down_out.status,
        String::from_utf8_lossy(&down_out.stdout),
        String::from_utf8_lossy(&down_out.stderr),
    );

    // Teardown cleanliness: no leftover libkrun process, pidfile, or sockets
    // (the pidfile check subsumes "no leftover process": a live pid with no
    // pidfile would be unobservable, but `tear_down` always removes the
    // pidfile after the process is confirmed gone, never before).
    let leftover = fixture.libkrun_artifacts();
    assert!(
        leftover.is_empty(),
        "frontend disable must remove every libkrun artifact before omnifs down, found: {leftover:?}"
    );
}
