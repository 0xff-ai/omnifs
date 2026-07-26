//! Docker-hosted FUSE filesystem acceptance.
//!
//! `omnifs fs attach --name itest-docker` attaches a separate, credential-free container to a
//! host-native daemon's shared namespace over TCP and renders kernel FUSE
//! inside it. This suite proves the container behaves like a real mount for
//! the standard toolbox (the `fuse-docker` conformance column, reusing the
//! shared matrix machinery from `omnifs_itest::matrix` rather than forking
//! it), plus the surrounding lifecycle, timing, and security guarantees:
//! `omnifs fs {create,attach,detach,ls}`, explicit filesystem teardown before
//! `omnifs down`, a cold-start budget, cross-mount byte identity,
//! kill/reattach behavior, and
//! the no-credentials contract.
//!
//! Gated on `OMNIFS_ACCEPTANCE_LIVE`, matching every other live-mount lane.
//! Requires Docker, the `omnifs-filesystem:*dev*` image (`just filesystem-image`,
//! or `OMNIFS_FILESYSTEM_IMAGE` naming a different local tag), and a platform
//! that can serve a host-native mount. Missing any of those is a skip, not a
//! failure, so a contributor without Docker still sees the suite pass.

#![cfg(not(target_os = "wasi"))]

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use omnifs_itest::matrix::{self, Exec};
use omnifs_itest::{live, provider_artifact_dir};
use omnifs_workspace::daemon_record::DaemonRecord;
use tempfile::TempDir;

/// Scratch dir inside the filesystem container for the matrix's copy/archive
/// rows. Distinct container per test, so no collision with
/// `filesystem_matrix`'s own `DOCKER_SCRATCH` in a different container.
const DOCKER_SCRATCH: &str = "/tmp/omnifs-matrix";

/// The Docker label `fs_container.rs` stamps on every filesystem
/// container with the workspace's config dir. Discovering containers by this
/// label (rather than recomputing the launcher's naming hash) keeps the
/// naming rule owned in one place.
const HOME_LABEL: &str = "ai.0xff.omnifs.home";

fn acceptance_gated() -> bool {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run the fuse-docker acceptance gate");
        return false;
    }
    true
}

/// Every precondition this suite needs beyond the live-acceptance gate: the
/// test provider artifact, a mountable platform, Docker, and the filesystem dev
/// image. Returns the image ref to use, or `None` (skip, with a message
/// already printed) when any precondition is missing.
fn preconditions() -> Option<String> {
    let test_wasm = provider_artifact_dir().join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just build providers`)",
            test_wasm.display()
        );
        return None;
    }
    if !live::platform_can_mount() {
        eprintln!("skip: platform cannot mount (no /dev/fuse)");
        return None;
    }
    if !docker_reachable() {
        eprintln!("skip: Docker not reachable");
        return None;
    }
    let Some(image) = filesystem_image() else {
        eprintln!(
            "skip: no local `omnifs-filesystem:*dev*` image found; run `just filesystem-image`"
        );
        return None;
    };
    Some(image)
}

fn docker_reachable() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Resolve the filesystem image to launch: an explicit `OMNIFS_FILESYSTEM_IMAGE`
/// override, else the floating `omnifs-filesystem:dev` tag, else the newest
/// local `omnifs-filesystem` image whose tag contains `dev` (CI tags per-arch
/// images as `docker-<arch>-<sha>`, so this also matches those once pulled
/// and retagged by the caller).
fn filesystem_image() -> Option<String> {
    if let Ok(image) = std::env::var("OMNIFS_FILESYSTEM_IMAGE") {
        return Some(image);
    }
    if docker_output(&["image", "inspect", "omnifs-filesystem:dev"]).is_some() {
        return Some("omnifs-filesystem:dev".to_string());
    }
    let listing = docker_output(&[
        "images",
        "omnifs-filesystem",
        "--format",
        "{{.Repository}}:{{.Tag}}",
    ])?;
    listing
        .lines()
        .map(str::trim)
        .find(|tag| tag.contains("dev") && !tag.ends_with(":<none>"))
        .map(str::to_string)
}

fn docker_output(args: &[&str]) -> Option<String> {
    let output = Command::new("docker").args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

// ===========================================================================
// Fixture: a hermetic workspace driving the real `omnifs` CLI end to end.
// ===========================================================================

/// Drives the real `omnifs` binary against a hermetic `OMNIFS_HOME`, exactly
/// as a contributor would: `up`, `fs create/attach/detach/ls`, `down`. No test
/// touches the user's real `~/.omnifs` or default ports.
struct Fixture {
    home: TempDir,
    mount_point: PathBuf,
    filesystem_image: String,
    daemon_pid: Option<u32>,
}

impl Fixture {
    fn new(filesystem_image: String) -> Self {
        let live::HermeticHome { home, mount_point } = live::hermetic_home();
        Self {
            home,
            mount_point,
            filesystem_image,
            daemon_pid: None,
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
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

    /// Run a CLI subcommand with the hermetic env, including the filesystem
    /// image override so Docker attach never reaches for a registry.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(live::omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env("OMNIFS_FILESYSTEM_IMAGE", &self.filesystem_image)
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    /// Bring up a host-native daemon, explicitly attach its host filesystem,
    /// and wait for it to serve the test-provider tree. Panics on a real failure: every
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

        let filesystem = if cfg!(target_os = "linux") {
            "fuse"
        } else {
            "nfs"
        };
        let location = self.mount_point.to_str().expect("mount point utf8");
        let out = self.run(&[
            "fs",
            "create",
            "--name",
            "itest-host",
            "--protocol",
            filesystem,
            "--runtime",
            "host",
            "--location",
            location,
        ]);
        assert!(
            out.status.success(),
            "host filesystem create failed (exit {})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let out = self.run(&["fs", "attach", "--name", "itest-host"]);
        assert!(
            out.status.success(),
            "host filesystem attach failed (exit {})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let out = self.run(&[
            "fs",
            "create",
            "--name",
            "itest-docker",
            "--protocol",
            "fuse",
            "--runtime",
            "docker",
        ]);
        assert!(
            out.status.success(),
            "Docker filesystem create failed (exit {})\nstdout: {}\nstderr: {}",
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

    fn filesystem_attach(&self) -> Output {
        self.run(&["fs", "attach", "--name", "itest-docker", "--output", "json"])
    }

    /// Assert Docker filesystem attach succeeded; on failure, dump the runner logs of
    /// every labeled container first (the fixture Drop removes them, so this
    /// is the only window to capture why the mount never served), then panic
    /// with the CLI's own output.
    fn assert_filesystem_attach_ok(&self, out: &Output, context: &str) {
        if out.status.success() {
            assert!(
                out.stderr.is_empty(),
                "structured Docker filesystem attach leaked stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        for name in self.containers() {
            let logs = docker_output(&["logs", "--tail", "60", &name]).unwrap_or_default();
            eprintln!("--- docker logs {name} (tail) ---\n{logs}\n---");
        }
        panic!(
            "omnifs fs attach --name itest-docker failed ({context}, exit {})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    fn filesystem_status(&self) -> Output {
        self.run(&["fs", "ls"])
    }

    fn down(&self) -> Output {
        self.run(&["down"])
    }

    /// Every filesystem container labeled for this fixture's home. Discovery by
    /// label (not by recomputing the launcher's naming hash) keeps the naming
    /// rule owned solely by `fs_container.rs`.
    fn containers(&self) -> Vec<String> {
        let filter = format!("label={HOME_LABEL}={}", self.home_path().display());
        docker_output(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"])
            .map(|out| out.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The fixture's single configured Docker filesystem. Product workspaces
    /// may have more than one when their IDs differ.
    fn container_name(&self) -> String {
        let names = self.containers();
        assert_eq!(
            names.len(),
            1,
            "expected exactly one filesystem container for this workspace, found {names:?}"
        );
        names.into_iter().next().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for name in self.containers() {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        }
        if let Some(pid) = self.daemon_pid.or_else(|| self.daemon_pid_from_record()) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
        force_unmount(&self.mount_point);
    }
}

/// Clear a possibly-dead-server mount without blocking. Mirrors
/// `lifecycle_acceptance.rs`'s teardown: on macOS `sudo -n umount -f` clears
/// a dead-server NFS mount instantly, where `diskutil unmount force` blocks
/// in an uninterruptible NFS syscall (observed hanging this suite's Drop);
/// the path is resolved via the parent so the dead mount itself is never
/// stat-ed. A no-op when nothing is mounted (the common case after explicit
/// filesystem detach). Retries briefly: a just-killed server can leave the mount
/// transiently busy before the kernel gives up on it.
fn force_unmount(mount_point: &Path) {
    #[cfg(target_os = "linux")]
    let unmount_once = || {
        let _ = Command::new("fusermount")
            .arg("-uz")
            .arg(mount_point)
            .output();
        let _ = Command::new("umount").arg("-f").arg(mount_point).output();
    };
    #[cfg(not(target_os = "linux"))]
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

// ===========================================================================
// Container inspection helpers
// ===========================================================================

fn docker_json(container: &str, format: &str) -> serde_json::Value {
    let out = docker_output(&["inspect", "-f", format, container])
        .unwrap_or_else(|| panic!("docker inspect {container} -f {format}"));
    serde_json::from_str(&out)
        .unwrap_or_else(|error| panic!("parse docker inspect output `{out}`: {error}"))
}

fn container_id(name: &str) -> String {
    docker_output(&["inspect", "-f", "{{.Id}}", name]).expect("docker inspect .Id")
}

fn container_state(name: &str) -> Option<String> {
    docker_output(&["inspect", "-f", "{{.State.Status}}", name])
}

fn wait_for_container_state(name: &str, want: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if container_state(name).as_deref() == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "container `{name}` never reached state `{want}` within {timeout:?} (last seen: {:?})",
            container_state(name)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_pid_gone(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .is_ok_and(|output| output.status.success());
        if !alive {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pid {pid} still alive after {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `docker exec <container> cat /omnifs/<mount>/hello/message` returns exact
/// fixture bytes for every configured mount.
fn assert_serves(container: &str) {
    for root in ["test", "test2"] {
        let guest_path = format!("/omnifs/{root}/hello/message");
        let out = Command::new("docker")
            .args(["exec", container, "cat", &guest_path])
            .output()
            .unwrap_or_else(|error| panic!("docker exec cat {guest_path}: {error}"));
        assert!(
            out.status.success(),
            "docker exec cat {guest_path} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "Hello, world!");
    }
}

// ===========================================================================
// (f) No-credentials structural contract
// ===========================================================================

/// The fail-closed contract `assert_locked_down`
/// (`crates/omnifs-cli/src/fs_container.rs`) enforces at launch time,
/// re-checked here from the outside with plain `docker inspect`/`docker exec`:
/// no mounts of any kind, an env set of exactly the two attach vars plus the
/// image's own defaults, and no guest home or credentials file anywhere in the
/// container (it never receives `OMNIFS_HOME`, so neither can exist).
fn assert_no_credentials_contract(container: &str) {
    let mounts = docker_json(container, "{{json .Mounts}}");
    assert!(
        mounts.as_array().is_some_and(Vec::is_empty),
        "filesystem container must have no mounts: {mounts}"
    );

    let env_value = docker_json(container, "{{json .Config.Env}}");
    let env: Vec<String> = env_value
        .as_array()
        .expect("env is a JSON array")
        .iter()
        .map(|entry| entry.as_str().expect("env entry is a string").to_string())
        .collect();
    let names: Vec<&str> = env
        .iter()
        .filter_map(|entry| entry.split_once('=').map(|(name, _)| name))
        .collect();
    assert!(
        names.contains(&"OMNIFS_ATTACH_ADDR"),
        "missing OMNIFS_ATTACH_ADDR: {env:?}"
    );
    for name in &names {
        assert!(
            matches!(*name, "OMNIFS_ATTACH_ADDR" | "PATH" | "HOME"),
            "unexpected env var `{name}` leaked into the credential-free filesystem container: {env:?}"
        );
    }

    // The guest home path is a Docker-boundary literal (AGENTS.md); the
    // container never receives OMNIFS_HOME, so neither it nor the
    // credentials file under it can exist.
    for path in ["/root/.omnifs", "/root/.omnifs/credentials.json"] {
        let status = Command::new("docker")
            .args(["exec", container, "test", "-e", path])
            .status();
        assert!(
            status.is_ok_and(|status| !status.success()),
            "`{path}` must not exist inside the credential-free filesystem container"
        );
    }
}

// ===========================================================================
// (d) Byte identity: container mount vs. the concurrent host mount
// ===========================================================================

fn read_prefix(path: &Path, limit: usize) -> Vec<u8> {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
    let mut buf = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut buf)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    buf
}

/// Read the same slice through the container via `dd`, the same tool the
/// matrix's `read-128k` row uses, so the same mechanism proves the same bytes
/// on both sides of the comparison.
fn docker_read_prefix(container: &str, guest_path: &str, limit: usize) -> Vec<u8> {
    let count = (limit / (128 * 1024)).max(1);
    let out = Command::new("docker")
        .args([
            "exec",
            container,
            "dd",
            &format!("if={guest_path}"),
            "bs=131072",
            &format!("count={count}"),
        ])
        .output()
        .unwrap_or_else(|error| panic!("docker exec dd {guest_path}: {error}"));
    assert!(
        out.status.success(),
        "docker exec dd {guest_path} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn assert_byte_identity(host_path: &Path, container: &str, guest_path: &str, limit: usize) {
    let host_bytes = read_prefix(host_path, limit);
    let container_bytes = docker_read_prefix(container, guest_path, limit);
    assert!(
        !host_bytes.is_empty(),
        "expected non-empty host bytes from {}",
        host_path.display()
    );
    assert_eq!(
        host_bytes.len(),
        container_bytes.len(),
        "byte length mismatch host vs container for {guest_path}"
    );
    assert_eq!(
        host_bytes, container_bytes,
        "byte identity failed host vs container for {guest_path}"
    );
}

// ===========================================================================
// (c) Cold start: docker-run-to-served-mount
// ===========================================================================

/// Gate: Docker filesystem attach must go from container start to a served mount
/// in under 15s. Timed with the daemon already warm (a prior `up_native` call),
/// so the span isolates container start cost from daemon bring-up. Sized for
/// shared CI runners (measured ~7.5s on GitHub-hosted Linux); the scorecard
/// JSON records the exact duration either way, so drift stays observable.
const COLD_START_BUDGET_MS: u64 = 15_000;

#[derive(serde::Serialize)]
struct ColdStart {
    version: u32,
    generated_at: String,
    metric: &'static str,
    duration_ms: u64,
    budget_ms: u64,
}

fn record_cold_start(duration: Duration) {
    let report = ColdStart {
        version: 1,
        generated_at: now_rfc3339(),
        metric: "docker-run-to-served-mount",
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        budget_ms: COLD_START_BUDGET_MS,
    };
    let path = matrix::scorecard_dir().join("cold-start-fuse-docker.json");
    let json = serde_json::to_string_pretty(&report).expect("serialize cold-start report");
    std::fs::write(&path, json).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    eprintln!(
        "cold-start-fuse-docker: {} ({} ms, budget {} ms)",
        path.display(),
        report.duration_ms,
        report.budget_ms
    );
    assert!(
        report.duration_ms < COLD_START_BUDGET_MS,
        "docker-run-to-served-mount took {}ms, over the {}ms gate",
        report.duration_ms,
        COLD_START_BUDGET_MS
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
// Test 1: lifecycle, cold start, byte identity, no-credentials, the matrix
// ===========================================================================

#[test]
#[allow(clippy::too_many_lines)] // one continuous acceptance run; expensive bring-up amortized
fn fuse_docker_lifecycle_and_matrix() {
    if !acceptance_gated() {
        return;
    }
    let Some(image) = preconditions() else {
        return;
    };

    let _nfs_lock = live::nfs_serial_lock();
    let mut fixture = Fixture::new(image);
    fixture.up_native();

    // (c) cold start: the daemon is already warm, so the timed span isolates
    // container-run-to-served-mount latency from daemon bring-up cost.
    let started = Instant::now();
    let up_out = fixture.filesystem_attach();
    let elapsed = started.elapsed();
    fixture.assert_filesystem_attach_ok(&up_out, "cold start");
    record_cold_start(elapsed);

    // (a) `fs ls` is truthful.
    let status_out = fixture.filesystem_status();
    assert!(
        status_out.status.success(),
        "fs ls failed (exit {})\nstdout: {}\nstderr: {}",
        status_out.status,
        String::from_utf8_lossy(&status_out.stdout),
        String::from_utf8_lossy(&status_out.stderr),
    );
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.contains("attached"),
        "fs ls must report an attached Docker filesystem: {status_text}"
    );

    let container = fixture.container_name();
    assert_serves(&container);

    // (f) no-credentials contract.
    assert_no_credentials_contract(&container);

    // (d) byte identity: the container mount vs. the concurrent host-native
    // mount served by the same daemon. 13 = len("Hello, world!").
    assert_byte_identity(
        &fixture.mount_point.join("test/hello/message"),
        &container,
        "/omnifs/test/hello/message",
        13,
    );
    assert_byte_identity(
        &fixture.mount_point.join("test/hello/large-ranged"),
        &container,
        "/omnifs/test/hello/large-ranged",
        256 * 1024,
    );
    assert_byte_identity(
        &fixture.mount_point.join("test2/hello/message"),
        &container,
        "/omnifs/test2/hello/message",
        13,
    );

    let _ = Command::new("docker")
        .args(["exec", &container, "mkdir", "-p", DOCKER_SCRATCH])
        .output();

    // (b) the fuse-docker matrix column, through the shared row/executor
    // machinery: every read row must pass, including tar and tail-f-growing
    // (the debian filesystem image ships GNU coreutils); write rows stay
    // expected-fail (no write path yet). The recursive grep excludes the
    // provider's deliberately invalid `checkout` tree-ref fixture while still
    // traversing the paged comments collection and live stream leaf.
    let exec = Exec::DockerExec {
        container: container.clone(),
        root: "/omnifs/test".to_string(),
        scratch: DOCKER_SCRATCH.to_string(),
    };
    let scorecard = matrix::run_column(&exec, &matrix::FUSE_DOCKER_FILESYSTEM, matrix::ROWS);
    let scorecard_path = matrix::write_scorecard(&scorecard);
    eprintln!("scorecard: {}", scorecard_path.display());
    eprintln!(
        "\n{}",
        matrix::render_table(std::slice::from_ref(&scorecard))
    );
    let mismatches = matrix::mismatches(&scorecard);
    assert!(
        mismatches.is_empty(),
        "fuse-docker column has {} expectation mismatch(es):\n  {}",
        mismatches.len(),
        mismatches.join("\n  ")
    );

    // Detaching Docker leaves the host filesystem serving every mount; attaching
    // it again restores the same whole-namespace view.
    let detached = fixture.run(&["fs", "detach", "--name", "itest-docker"]);
    assert!(
        detached.status.success(),
        "detaching Docker filesystem failed (exit {})\nstdout: {}\nstderr: {}",
        detached.status,
        String::from_utf8_lossy(&detached.stdout),
        String::from_utf8_lossy(&detached.stderr),
    );
    for root in ["test", "test2"] {
        assert!(
            fixture
                .mount_point
                .join(root)
                .join("hello/message")
                .is_file(),
            "host filesystem lost mount {root} while Docker filesystem was detached"
        );
    }
    let reattached = fixture.filesystem_attach();
    fixture.assert_filesystem_attach_ok(&reattached, "reattach after detach");
    assert_serves(&fixture.container_name());

    // Filesystem runners have independent lifecycles. Detach Docker and the
    // host runner explicitly, then stop the daemon with `omnifs down`.
    let detached = fixture.run(&["fs", "detach", "--name", "itest-docker"]);
    assert!(
        detached.status.success(),
        "detaching Docker filesystem before down failed (exit {})\nstdout: {}\nstderr: {}",
        detached.status,
        String::from_utf8_lossy(&detached.stdout),
        String::from_utf8_lossy(&detached.stderr),
    );
    let detached = fixture.run(&["fs", "detach", "--name", "itest-host"]);
    assert!(
        detached.status.success(),
        "detaching host filesystem before down failed (exit {})\nstdout: {}\nstderr: {}",
        detached.status,
        String::from_utf8_lossy(&detached.stdout),
        String::from_utf8_lossy(&detached.stderr),
    );
    let down_out = fixture.down();
    assert!(
        down_out.status.success(),
        "omnifs down failed (exit {})\nstdout: {}\nstderr: {}",
        down_out.status,
        String::from_utf8_lossy(&down_out.stdout),
        String::from_utf8_lossy(&down_out.stderr),
    );
    assert!(
        fixture.containers().is_empty(),
        "filesystem detach must remove the filesystem container before omnifs down"
    );
}

// ===========================================================================
// Test 2: kill-and-reattach FUSE semantics
// ===========================================================================

/// (e) Two failure modes, both real, deliberately kept apart:
///
/// 1. **Kill the container.** The FUSE mount lives entirely inside the
///    container's own mount namespace, so killing it leaves nothing to clean
///    up host-side; the only observable effect is the container going away.
///    Docker filesystem attach again creates a fresh container that serves.
/// 2. **Kill the daemon, leaving the container alive.** The VFS wire client
///    reconnects with backoff forever (`omnifs-vfs`), and daemon startup
///    restores the predecessor TCP address and token before publishing
///    readiness. The same container therefore reattaches without replacement.
///    This test deliberately avoids a filesystem read while the daemon is
///    absent because an unreachable FUSE attach can block in the kernel.
#[test]
fn kill_and_reattach_fuse_semantics() {
    if !acceptance_gated() {
        return;
    }
    let Some(image) = preconditions() else {
        return;
    };

    let _nfs_lock = live::nfs_serial_lock();
    let mut fixture = Fixture::new(image);
    fixture.up_native();

    let up1 = fixture.filesystem_attach();
    fixture.assert_filesystem_attach_ok(&up1, "first bring-up");
    let container = fixture.container_name();
    assert_serves(&container);
    let id_1 = container_id(&container);

    // Failure mode 1: SIGKILL the container.
    let kill_status = Command::new("docker")
        .args(["kill", "-s", "KILL", &container])
        .status();
    assert!(
        kill_status.is_ok_and(|status| status.success()),
        "docker kill must succeed"
    );
    wait_for_container_state(&container, "exited", Duration::from_secs(10));

    let up2 = fixture.filesystem_attach();
    fixture.assert_filesystem_attach_ok(&up2, "after a killed container");
    let id_2 = container_id(&container);
    assert_ne!(
        id_1, id_2,
        "filesystem attach must create a genuinely fresh container, not reuse the killed one"
    );
    assert_serves(&container);

    // Failure mode 2: SIGKILL the daemon, container untouched.
    let old_pid = fixture
        .daemon_pid
        .expect("daemon pid recorded by up_native");
    let old_instance = fixture
        .record()
        .expect("daemon record present while serving")
        .instance_id;
    let _ = Command::new("kill")
        .args(["-9", &old_pid.to_string()])
        .output();
    wait_for_pid_gone(old_pid, Duration::from_secs(10));

    // The container process survives: no crash just because the daemon it
    // depends on died. Deliberately no read through its mount here (see the
    // test doc comment).
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        container_state(&container).as_deref(),
        Some("running"),
        "the filesystem container must survive a bare daemon kill without a graceful teardown"
    );

    // A bare `kill -9` (bypassing `omnifs down`'s graceful unmount) leaves the
    // native mount a zombie: a dead-server NFS/FUSE mount at the same path
    // that blocks a fresh daemon from mounting there again. This is a
    // pre-existing rough edge independent of the Docker filesystem (observed
    // manually against a plain `omnifs up` after a bare kill); the product's
    // supported recovery is to detach the stale filesystem before stopping the
    // daemon, but calling it here would also tear down the very container this
    // test is keeping alive on purpose. So this test sweeps the host mount by
    // hand: force-unmount, then brings up a fresh daemon on the same home.
    // The daemon instance changes while its approved attach authority stays
    // stable for the surviving container.
    force_unmount(&fixture.mount_point);
    fixture.up_native();
    let new_instance = fixture
        .record()
        .expect("daemon record present after restart")
        .instance_id;
    assert_ne!(
        old_instance, new_instance,
        "a restarted daemon must mint a new instance id"
    );

    // `up` publishes daemon readiness before surviving filesystems finish their
    // independent reconnect backoff. `filesystem attach` observes the live
    // container and waits for its attachment without replacing it.
    let reattached = fixture.filesystem_attach();
    fixture.assert_filesystem_attach_ok(&reattached, "after daemon restart");

    let id_3 = container_id(&container);
    assert_eq!(
        id_2, id_3,
        "daemon restart must preserve the existing filesystem container"
    );
    assert_serves(&container);
}
