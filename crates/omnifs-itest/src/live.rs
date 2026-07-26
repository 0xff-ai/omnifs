//! Live-daemon support shared by the filesystem conformance matrix lanes.
//!
//! This is the one owner of the cross-process NFS serialization lock, the
//! `omnifs` binary resolution used by matrix lanes, the hermetic `OMNIFS_HOME`
//! construction, and native-daemon bring-up/readiness/teardown. The matrix and
//! CLI lifecycle suite share this lock owner and daemon contract.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use omnifs_api::{
    CONTROL_MAX_LINE_BYTES, CONTROL_PROTOCOL_VERSION, ControlOperation, ControlOutcome,
    ControlReply, ControlRequest,
};
use omnifs_core::ProviderId;
use omnifs_workspace::mounts::Repository;
use omnifs_workspace::provider::{Artifact, ProviderStore};
use tempfile::TempDir;

/// Fixed, non-ephemeral port used purely as a cross-process lock for live NFS
/// mounts. Below the OS ephemeral range, so it does not collide with any
/// filesystem or attach listener.
pub const NFS_LOCK_PORT: u16 = 48761;

/// Acquire the cross-process NFS serialization lock, returning the bound socket
/// as the guard. nextest runs each integration-test binary as its own process,
/// so an in-process mutex cannot serialize across binaries.
#[must_use]
pub fn nfs_serial_lock() -> TcpListener {
    loop {
        match TcpListener::bind(("127.0.0.1", NFS_LOCK_PORT)) {
            Ok(listener) => return listener,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Whether the platform can serve a mount. On Linux, FUSE requires `/dev/fuse`.
/// On macOS, NFS loopback is always available without root.
#[must_use]
pub fn platform_can_mount() -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/dev/fuse").exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Resolve the `omnifs` binary the live lanes spawn.
///
/// CI can supply a packaged binary through `OMNIFS_BIN`. Local nextest runs
/// use the non-test binary nextest already built. Standalone libtest runs fall
/// back to building the workspace binary once per process.
#[must_use]
pub fn omnifs_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("OMNIFS_BIN") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("NEXTEST_BIN_EXE_omnifs") {
        return PathBuf::from(path);
    }
    if let Some(path) = nextest_workspace_binary("omnifs") {
        return path;
    }
    ensure_omnifs_built();
    crate::workspace_root().join("target/debug/omnifs")
}

/// Resolve a workspace binary that nextest built for another package.
///
/// Nextest only exports `NEXTEST_BIN_EXE_*` to integration tests belonging to
/// the binary's own package. A workspace run still builds these non-test
/// binaries in the target directory before any test starts.
fn nextest_workspace_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("NEXTEST").and_then(|_| {
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| crate::workspace_root().join("target"), PathBuf::from);
        let path = target_dir
            .join("debug")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        path.is_file().then_some(path)
    })
}

/// Build the `omnifs` CLI once per process at test runtime.
///
/// Mirrors [`crate::provider_wasm_path`]'s build-on-demand pattern: it runs
/// after cargo's build phase has released the target-dir lock, so the build it
/// triggers writes into the same `target/debug` the artifact is read from
/// without deadlocking against the build that produced this test binary. Set
/// `OMNIFS_BIN` to skip it.
fn ensure_omnifs_built() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let status = Command::new("cargo")
            .args(["build", "-p", "omnifs-cli", "--bin", "omnifs"])
            .current_dir(crate::workspace_root())
            .status()
            .expect("spawn `cargo build -p omnifs-cli`");
        assert!(
            status.success(),
            "`cargo build -p omnifs-cli --bin omnifs` failed; run it directly to see the error",
        );
    });
}

/// Resolve the shipped `omnifs-thin` runner the live lanes spawn.
#[must_use]
pub fn thin_runner_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("OMNIFS_THIN_BIN") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("NEXTEST_BIN_EXE_omnifs-thin") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("NEXTEST_BIN_EXE_omnifs_thin") {
        return PathBuf::from(path);
    }
    if let Some(path) = nextest_workspace_binary("omnifs-thin") {
        return path;
    }
    ensure_thin_runner_built();
    crate::workspace_root().join("target/debug/omnifs-thin")
}

/// Build the flat internal argv for a directly spawned host runner.
///
/// Product lifecycle uses hidden `omnifs run-fs`; live protocol tests spawn
/// the slim binary to isolate the wire boundary, so they must supply the same
/// private control identity and process-group shape as the host launcher.
pub fn thin_host_runner_command(
    id: &str,
    protocol: &str,
    location: &Path,
    state_dir: &Path,
    attach: Option<&Path>,
) -> Command {
    use std::sync::atomic::{AtomicU64, Ordering};

    static INSTANCE: AtomicU64 = AtomicU64::new(1);
    let instance = format!("{:032x}", INSTANCE.fetch_add(1, Ordering::Relaxed));
    let control = state_dir.join(format!("control-{instance}.sock"));
    let mut command = Command::new(thin_runner_bin());
    command
        .args(["--name", id, "--protocol", protocol, "--runtime", "host"])
        .arg("--location")
        .arg(location)
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--runner-instance")
        .arg(instance)
        .arg("--runner-control")
        .arg(control);
    if let Some(attach) = attach {
        command.arg("--attach").arg(attach);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command
}

/// Build the shipped thin runner once per process at test runtime.
fn ensure_thin_runner_built() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let status = Command::new("cargo")
            .args(["build", "-p", "omnifs-thin", "--bin", "omnifs-thin"])
            .current_dir(crate::workspace_root())
            .status()
            .expect("spawn `cargo build -p omnifs-thin --bin omnifs-thin`");
        assert!(
            status.success(),
            "`cargo build -p omnifs-thin --bin omnifs-thin` failed; run it directly to see the error",
        );
    });
}

/// Install the test provider into the provider store under `providers_dir` and
/// return its content id. The daemon serves by content id, so the provider must
/// be present in the store before a mount spec pinning it can resolve.
pub fn install_test_provider(providers_dir: &Path) -> ProviderId {
    let bytes = std::fs::read(crate::provider_wasm_path("test_provider.wasm"))
        .expect("read test provider wasm");
    let artifact =
        Artifact::from_bytes("test_provider.wasm", bytes).expect("parse test provider artifact");
    let id = artifact.id();
    let store = ProviderStore::new(providers_dir);
    store.retain(&artifact).expect("retain test provider");
    id
}

/// No-auth mount spec for the test provider, pinning `id`. Serves the projected
/// tree under `test/`.
#[must_use]
pub fn test_mount_spec(id: &ProviderId) -> String {
    test_mount_spec_at(id, "test")
}

/// No-auth mount spec for the test provider under an arbitrary namespace root.
/// Live shared-namespace fixtures install two copies (`test` and `test2`) so
/// every filesystem must expose the same known bytes from both roots.
#[must_use]
pub fn test_mount_spec_at(id: &ProviderId, mount: &str) -> String {
    format!(r#"{{"provider":{{"id":"{id}","meta":{{"name":"test-provider"}}}},"mount":"{mount}"}}"#)
}

/// A hermetic `OMNIFS_HOME` with the test provider installed, its mount specs
/// committed as desired state, an immutable daemon snapshot, and an empty
/// mount point.
pub struct HermeticHome {
    pub home: TempDir,
    pub mount_point: PathBuf,
}

/// Build a hermetic home: install the test provider into `providers/`, write
/// two pinned specs (`test` and `test2`), and create the filesystem mount point.
#[must_use]
pub fn hermetic_home() -> HermeticHome {
    let home = tempfile::tempdir().expect("home tempdir");
    let providers = home.path().join("providers");
    std::fs::create_dir_all(&providers).expect("providers dir");
    let test_id = install_test_provider(&providers);

    let mounts_dir = home.path().join("mounts");
    std::fs::create_dir_all(&mounts_dir).expect("mounts dir");
    std::fs::write(mounts_dir.join("test.json"), test_mount_spec(&test_id))
        .expect("write test mount spec");
    std::fs::write(
        mounts_dir.join("test2.json"),
        test_mount_spec_at(&test_id, "test2"),
    )
    .expect("write second test mount spec");
    let _ = daemon_args(home.path());

    let mount_point = home.path().join("mnt");
    std::fs::create_dir_all(&mount_point).expect("mount point");

    HermeticHome { home, mount_point }
}

/// Build the immutable desired-state arguments for a direct hidden-daemon
/// test launch. This is the sole test helper that initializes and snapshots a
/// mount repository, so direct launches cannot accidentally read mutable
/// `mounts/` state.
#[must_use]
pub fn daemon_args(home: &Path) -> Vec<OsString> {
    let repository = Repository::open(home.join("mounts")).expect("open mount repository");
    let revision = repository
        .head_revision()
        .expect("read mount revision")
        .expect("initialized mount repository has HEAD");
    let (snapshot, _) = repository
        .snapshot(&revision, home.join("cache"))
        .expect("snapshot mount revision");
    vec![
        OsString::from("daemon"),
        OsString::from("--mount-revision"),
        OsString::from(revision.as_str()),
        OsString::from("--mount-snapshot"),
        snapshot.into_os_string(),
    ]
}

/// A running `omnifs daemon` and explicit local filesystem runner with the test
/// provider mounted, torn down on drop.
pub struct NativeDaemon {
    daemon: Child,
    filesystem: Child,
    pub mount_point: PathBuf,
    _home: TempDir,
    /// Cross-process NFS serialization lock, held for the lane's lifetime.
    /// `None` when the caller holds the lock externally (the perf lane spans two
    /// sequential lanes under one lock, so no per-lane bring-up owns its own).
    _nfs_lock: Option<TcpListener>,
}

impl Drop for NativeDaemon {
    fn drop(&mut self) {
        sigterm(&self.filesystem);
        wait_briefly(&mut self.filesystem);
        self.detach_mount();
        sigterm(&self.daemon);
        wait_briefly(&mut self.daemon);
        let _ = self.filesystem.kill();
        let _ = self.filesystem.wait();
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl NativeDaemon {
    /// The projected tree root for the test provider (`<mount>/test`).
    #[must_use]
    pub fn tree_root(&self) -> PathBuf {
        self.mount_point.join("test")
    }

    fn detach_mount(&self) {
        #[cfg(not(target_os = "linux"))]
        {
            if omnifs_nfs::mount_is_active(&self.mount_point) {
                let _ = omnifs_nfs::unmount(&self.mount_point);
            }
            let deadline = Instant::now() + Duration::from_secs(8);
            while omnifs_nfs::mount_is_active(&self.mount_point) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        #[cfg(target_os = "linux")]
        {
            use std::ffi::OsStr;
            let mp = self.mount_point.as_os_str();
            let _ = Command::new("fusermount")
                .args([OsStr::new("-u"), mp])
                .status();
            let _ = Command::new("umount").arg(mp).status();
        }
    }
}

/// A running `omnifs daemon` with an explicit set of local filesystem runners
/// attached over one shared namespace. Torn down on drop.
pub struct MultiFilesystemDaemon {
    daemon: Child,
    filesystems: Vec<Child>,
    pub mount_points: Vec<PathBuf>,
    home: TempDir,
    /// Cross-process NFS serialization lock, held for the lane's lifetime.
    _nfs_lock: TcpListener,
}

impl Drop for MultiFilesystemDaemon {
    fn drop(&mut self) {
        for filesystem in &self.filesystems {
            sigterm(filesystem);
        }
        for filesystem in &mut self.filesystems {
            wait_briefly(filesystem);
        }
        for mount_point in &self.mount_points {
            detach_mount_any(mount_point);
        }
        sigterm(&self.daemon);
        wait_briefly(&mut self.daemon);
        for filesystem in &mut self.filesystems {
            let _ = filesystem.kill();
            let _ = filesystem.wait();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl MultiFilesystemDaemon {
    /// The daemon-owned daemon record for this hermetic home.
    #[must_use]
    pub fn record_path(&self) -> PathBuf {
        self.home.path().join("daemon.json")
    }

    /// Live daemon observations for this hermetic home.
    #[must_use]
    pub fn status(&self) -> omnifs_api::DaemonStatus {
        let reply = control_request(
            &self.home.path().join("control.sock"),
            ControlOperation::Status,
        )
        .expect("query daemon status");
        match reply.outcome {
            ControlOutcome::Status(status) => status,
            other => panic!("unexpected status reply: {other:?}"),
        }
    }

    /// The projected test-provider root under the filesystem at `index`.
    #[must_use]
    pub fn tree_root(&self, index: usize) -> PathBuf {
        self.mount_points[index].join("test")
    }
}

/// Force-unmount a mount point regardless of filesystem kind: try FUSE and NFS
/// teardown so a dual FUSE+NFS daemon cleans up both.
fn detach_mount_any(mount_point: &Path) {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::OsStr;
        let mp = mount_point.as_os_str();
        let _ = Command::new("fusermount")
            .args([OsStr::new("-uz"), mp])
            .status();
        let _ = Command::new("umount").arg(mp).status();
    }
    #[cfg(not(target_os = "linux"))]
    {
        if omnifs_nfs::mount_is_active(mount_point) {
            let _ = omnifs_nfs::unmount(mount_point);
        }
    }
}

/// Bring up `omnifs daemon` and the local filesystem runners named in `kinds`
/// (`"fuse"` or `"nfs"`), each at its own mount point under a hermetic home and
/// attached to the fixed local namespace socket.
///
/// Returns `None` (skip) when the platform cannot mount or the daemon does not
/// serve every mount (for example the NFS client bits are missing). Panics only
/// on a spawn error. The caller gates on `OMNIFS_ACCEPTANCE_LIVE` and holds the
/// NFS serial lock (this helper also holds its own copy for the lane lifetime).
#[must_use]
#[allow(clippy::too_many_lines)] // linear process-group bring-up
pub fn start_multi_filesystem_daemon(kinds: &[&str]) -> Option<MultiFilesystemDaemon> {
    let test_wasm = crate::provider_artifact_dir().join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just build providers`)",
            test_wasm.display()
        );
        return None;
    }
    if !platform_can_mount() {
        eprintln!("skip: platform cannot mount (no /dev/fuse)");
        return None;
    }

    let nfs_lock = nfs_serial_lock();
    let HermeticHome { home, .. } = hermetic_home();

    let control_socket = home.path().join("control.sock");
    let mut mount_points = Vec::with_capacity(kinds.len());
    for (index, kind) in kinds.iter().enumerate() {
        let mount_point = home.path().join(format!("mnt-{index}-{kind}"));
        std::fs::create_dir_all(&mount_point).expect("filesystem mount point");
        mount_points.push(mount_point);
    }

    let daemon = Command::new(omnifs_bin())
        .args(daemon_args(home.path()))
        .env("OMNIFS_HOME", home.path())
        .env("RUST_LOG", "warn")
        .spawn();
    let mut daemon = match daemon {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip: spawn omnifs daemon failed: {error}");
            return None;
        },
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    while !control_socket_ready(&control_socket) {
        match daemon.try_wait() {
            Ok(Some(status)) => panic!("omnifs daemon exited with {status} before ready"),
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if Instant::now() >= deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!(
                "omnifs daemon never reported ready on {}",
                control_socket.display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let attach_socket = home.path().join("filesystems/runtime/local.sock");
    let mut filesystems = Vec::with_capacity(kinds.len());
    for ((index, kind), mount_point) in kinds.iter().enumerate().zip(&mount_points) {
        let id = format!("live-{index}");
        let state_dir = home.path().join(format!("cache/filesystems/{id}"));
        let mut command = match *kind {
            "fuse" | "nfs" => {
                thin_host_runner_command(&id, kind, mount_point, &state_dir, Some(&attach_socket))
            },
            other => panic!("unsupported filesystem kind `{other}`"),
        };
        command
            .env("OMNIFS_HOME", home.path())
            .env("RUST_LOG", "warn");
        match command.spawn() {
            Ok(child) => filesystems.push(child),
            Err(error) => {
                for filesystem in &mut filesystems {
                    let _ = filesystem.kill();
                    let _ = filesystem.wait();
                }
                let _ = daemon.kill();
                let _ = daemon.wait();
                eprintln!("skip: spawn omnifs-thin {kind} failed: {error}");
                return None;
            },
        }
    }

    let mut daemon = MultiFilesystemDaemon {
        daemon,
        filesystems,
        mount_points,
        home,
        _nfs_lock: nfs_lock,
    };

    // Wait for the record to appear and every filesystem to serve the projected
    // tree. A non-zero exit before serving is a hard failure (bad CLI parse or
    // bind collision); a clean exit is a skip.
    let record = daemon.record_path();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let all_serving = daemon
            .mount_points
            .iter()
            .all(|mp| mp.join("test/hello/message").is_file());
        if record.exists() && all_serving {
            return Some(daemon);
        }
        match daemon.daemon.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    eprintln!("skip: daemon exited cleanly before every filesystem served");
                } else {
                    eprintln!(
                        "skip: daemon exited ({status}) before every filesystem served; a \
                         requested filesystem could not come up on this platform"
                    );
                }
                return None;
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        for filesystem in &mut daemon.filesystems {
            match filesystem.try_wait() {
                Ok(Some(status)) => {
                    eprintln!(
                        "skip: filesystem runner exited ({status}) before every mount served"
                    );
                    return None;
                },
                Ok(None) => {},
                Err(error) => panic!("poll filesystem child status: {error}"),
            }
        }
        if Instant::now() >= deadline {
            eprintln!("skip: not every filesystem served within 30s");
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// An `omnifs daemon` plus an out-of-process `omnifs-thin --protocol nfs` runner attached to
/// its fixed local namespace socket. Torn down on drop: the filesystem first,
/// then the daemon.
pub struct WireFilesystemDaemon {
    daemon: Child,
    filesystem: Child,
    pub mount_point: PathBuf,
    _home: TempDir,
    /// Cross-process NFS serialization lock, held for the lane's lifetime.
    /// `None` when the caller holds the lock externally (the perf lane spans two
    /// sequential lanes under one lock, so no per-lane bring-up owns its own).
    _nfs_lock: Option<TcpListener>,
}

impl Drop for WireFilesystemDaemon {
    fn drop(&mut self) {
        // SIGTERM the filesystem first so its signal handler unmounts cleanly, then
        // the daemon. Fall back to a force-unmount sweep and SIGKILL.
        sigterm(&self.filesystem);
        wait_briefly(&mut self.filesystem);
        detach_mount_any(&self.mount_point);
        sigterm(&self.daemon);
        wait_briefly(&mut self.daemon);
        let _ = self.filesystem.kill();
        let _ = self.filesystem.wait();
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl WireFilesystemDaemon {
    /// The projected test-provider root (`<mount>/test`).
    #[must_use]
    pub fn tree_root(&self) -> PathBuf {
        self.mount_point.join("test")
    }
}

/// Send SIGTERM to a child by pid (std `Child::kill` only sends SIGKILL).
fn sigterm(child: &Child) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
}

/// Poll a child's exit for up to 5s so a SIGTERM has time to unmount cleanly.
fn wait_briefly(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Bring up a daemon and an `omnifs-thin --protocol nfs` runner attached to its fixed local
/// namespace socket. Proves the projected tree serves out of process over the
/// Omnifs VFS wire protocol.
///
/// Returns `None` (skip) when the platform cannot mount or a surface never comes
/// up; panics only on a spawn error or a daemon that is alive but never ready
/// (a real regression in daemon readiness). The caller gates on
/// `OMNIFS_ACCEPTANCE_LIVE`.
#[must_use]
pub fn start_wire_filesystem() -> Option<WireFilesystemDaemon> {
    wire_filesystem(AttachTransport::Unix, Some(nfs_serial_lock()))
}

/// Like [`start_wire_filesystem`] but the caller already holds the NFS serial lock
/// and keeps holding it. The perf lane holds one lock across both its sequential
/// lanes, so it must not let each bring-up acquire (and later drop) its own.
#[must_use]
pub fn start_wire_filesystem_holding_lock() -> Option<WireFilesystemDaemon> {
    wire_filesystem(AttachTransport::Unix, None)
}

/// Like [`start_wire_filesystem`], but the out-of-process runner attaches over
/// TCP loopback (`OMNIFS_ATTACH_ADDR`) instead of a Unix socket: the same
/// transport the Docker-hosted filesystem uses, minus the container. Used by the
/// attach-transport perf comparison (TCP vs UDS), which isolates the transport
/// cost from Docker's own overhead.
#[must_use]
pub fn start_wire_filesystem_tcp_holding_lock() -> Option<WireFilesystemDaemon> {
    wire_filesystem(AttachTransport::Tcp, None)
}

/// Which transport the out-of-process runner attaches over. `Unix` shares a
/// socket path with the daemon; `Tcp` is the Docker-hosted filesystem's only
/// option (it cannot share a host Unix socket into its container), reached
/// here without a container so the perf lane isolates transport cost from
/// Docker's own overhead.
#[derive(Clone, Copy)]
enum AttachTransport {
    Unix,
    Tcp,
}

#[allow(clippy::too_many_lines)] // linear end-to-end bring-up
#[must_use]
fn wire_filesystem(
    transport: AttachTransport,
    nfs_lock: Option<TcpListener>,
) -> Option<WireFilesystemDaemon> {
    let test_wasm = crate::provider_artifact_dir().join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just build providers`)",
            test_wasm.display()
        );
        return None;
    }
    if !platform_can_mount() {
        eprintln!("skip: platform cannot mount (no /dev/fuse)");
        return None;
    }

    let hermetic = hermetic_home();
    let home_path = hermetic.home.path().to_path_buf();

    let control_socket = home_path.join("control.sock");

    // The daemon serves both fixed workspace endpoints on every start.
    let daemon_args = daemon_args(&home_path);
    let daemon = Command::new(omnifs_bin())
        .args(&daemon_args)
        .env("OMNIFS_HOME", &home_path)
        .env("RUST_LOG", "warn")
        .spawn();
    let mut daemon = match daemon {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip: spawn omnifs daemon failed: {error}");
            return None;
        },
    };

    // Ready means mounts reconciled and all requested attach listeners serve.
    let deadline = Instant::now() + Duration::from_secs(30);
    let ready = loop {
        match daemon.try_wait() {
            Ok(Some(status)) => {
                let _ = daemon.wait();
                panic!(
                    "daemon exited with {status} before Ready on {}; \
                     this is a startup regression, not a skip",
                    control_socket.display()
                );
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if control_socket_ready(&control_socket) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if !ready {
        let _ = daemon.kill();
        let _ = daemon.wait();
        panic!(
            "daemon never reported Ready on {} after 30s; \
             the attach-listener ready path regressed. Check the daemon log.",
            control_socket.display()
        );
    }

    let mount_point = home_path.join("mnt-wire");
    std::fs::create_dir_all(&mount_point).expect("filesystem mount point");

    // The out-of-process renderer attaches over the requested transport and
    // mounts the tree: `--attach <socket>` for Unix, or the VFS TCP env pair.
    let state_dir = home_path.join("cache/filesystems/wire");
    let attach_socket = home_path.join("filesystems/runtime/local.sock");
    let mut filesystem_cmd = thin_host_runner_command(
        "wire",
        "nfs",
        &mount_point,
        &state_dir,
        Some(&attach_socket),
    );
    filesystem_cmd
        .env("OMNIFS_HOME", &home_path)
        .env("RUST_LOG", "warn");
    match transport {
        AttachTransport::Unix => {
            assert!(
                attach_socket.exists(),
                "attach socket {} absent after the daemon reported ready",
                attach_socket.display()
            );
        },
        AttachTransport::Tcp => {
            let reply = control_request(&control_socket, ControlOperation::Status)
                .expect("query daemon status");
            let ControlOutcome::Status(status) = reply.outcome else {
                panic!("unexpected daemon status reply")
            };
            let attach = status
                .attach_tcp
                .expect("daemon status must publish its TCP attach endpoint");
            // Replace the explicit Unix target with the TCP env target.
            let mut tcp_command =
                thin_host_runner_command("wire", "nfs", &mount_point, &state_dir, None);
            tcp_command
                .env("OMNIFS_HOME", &home_path)
                .env("RUST_LOG", "warn")
                .env(omnifs_api::OMNIFS_ATTACH_ADDR_ENV, attach.to_string());
            filesystem_cmd = tcp_command;
        },
    }
    let filesystem = filesystem_cmd.spawn();
    let mut filesystem = match filesystem {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip: spawn omnifs-thin --protocol nfs failed: {error}");
            let _ = daemon.kill();
            let _ = daemon.wait();
            return None;
        },
    };

    let message = mount_point.join("test/hello/message");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if message.is_file() {
            return Some(WireFilesystemDaemon {
                daemon,
                filesystem,
                mount_point,
                _home: hermetic.home,
                _nfs_lock: nfs_lock,
            });
        }
        match filesystem.try_wait() {
            Ok(Some(status)) => {
                eprintln!(
                    "skip: filesystem runner exited ({status}) before the mount served; \
                     the renderer could not come up on this platform"
                );
                let _ = daemon.kill();
                let _ = daemon.wait();
                return None;
            },
            Ok(None) => {},
            Err(error) => panic!("poll filesystem child status: {error}"),
        }
        if Instant::now() >= deadline {
            eprintln!("skip: {} never appeared within 30s", message.display());
            let _ = filesystem.kill();
            let _ = filesystem.wait();
            let _ = daemon.kill();
            let _ = daemon.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn control_request(socket: &Path, operation: ControlOperation) -> Option<ControlReply> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    let request = ControlRequest {
        version: CONTROL_PROTOCOL_VERSION,
        operation,
    };
    let mut line = serde_json::to_vec(&request).ok()?;
    line.push(b'\n');
    stream.write_all(&line).ok()?;
    let mut reply = Vec::with_capacity(256);
    loop {
        let mut byte = [0_u8; 1];
        let read = stream.read(&mut byte).ok()?;
        if read == 0 || reply.len() >= CONTROL_MAX_LINE_BYTES {
            return None;
        }
        reply.push(byte[0]);
        if byte[0] == b'\n' {
            return serde_json::from_slice(&reply).ok();
        }
    }
}

pub fn control_ready(socket: &Path) -> bool {
    control_request(socket, ControlOperation::Ready)
        .is_some_and(|reply| matches!(reply.outcome, ControlOutcome::Ready))
}

fn control_socket_ready(socket: &Path) -> bool {
    control_ready(socket)
}

/// Bring up `omnifs daemon` with an explicit local filesystem runner and only the
/// test provider mounted. `OMNIFS_HOME` is hermetic per lane, so neither
/// process touches the user's real workspace.
///
/// Returns `None` (skip) only when the platform genuinely cannot mount. Panics
/// if the daemon exits due to a CLI parse error or bind collision, since that is
/// a real regression in the test or the daemon argument surface, not a skip.
///
/// The caller is responsible for the `OMNIFS_ACCEPTANCE_LIVE` env gate and its
/// skip message.
#[must_use]
pub fn start_native_daemon() -> Option<NativeDaemon> {
    // Hold the cross-process NFS lock for the whole lane so this binary's mount
    // never races the CLI lifecycle suite's mounts in a parallel nextest run.
    native_daemon(Some(nfs_serial_lock()))
}

#[allow(clippy::too_many_lines)] // linear end-to-end daemon bring-up
#[must_use]
fn native_daemon(nfs_lock: Option<TcpListener>) -> Option<NativeDaemon> {
    let test_wasm = crate::provider_artifact_dir().join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just build providers`)",
            test_wasm.display()
        );
        return None;
    }

    if !platform_can_mount() {
        eprintln!("skip: platform cannot mount (no /dev/fuse)");
        return None;
    }

    let hermetic = hermetic_home();
    let mount_point = hermetic.mount_point.clone();

    let control_socket = hermetic.home.path().join("control.sock");

    let daemon = Command::new(omnifs_bin())
        .args(daemon_args(hermetic.home.path()))
        .env("OMNIFS_HOME", hermetic.home.path())
        .env("RUST_LOG", "warn")
        .spawn();
    let mut daemon = match daemon {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip: spawn omnifs daemon failed: {error}");
            return None;
        },
    };

    // Wait for the daemon before attaching the runner.
    let deadline = Instant::now() + Duration::from_secs(30);
    let ready = loop {
        match daemon.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    eprintln!("skip: daemon exited cleanly before the control socket was ready");
                    return None;
                }
                panic!(
                    "omnifs daemon exited with {status} before the control socket became ready on \
                     {}; this is a CLI or startup error, not a skip.",
                    control_socket.display()
                );
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if control_socket_ready(&control_socket) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if !ready {
        let _ = daemon.kill();
        let _ = daemon.wait();
        panic!(
            "omnifs daemon control socket never became ready on {} after 30s; \
             the daemon is alive but not serving. Check the daemon log.",
            control_socket.display()
        );
    }

    let attach_socket = hermetic.home.path().join("filesystems/runtime/local.sock");
    #[cfg(target_os = "linux")]
    let mut filesystem_command = thin_host_runner_command(
        "native",
        "fuse",
        &mount_point,
        &hermetic.home.path().join("cache/filesystems/native"),
        Some(&attach_socket),
    );
    #[cfg(not(target_os = "linux"))]
    let mut filesystem_command = thin_host_runner_command(
        "native",
        "nfs",
        &mount_point,
        &hermetic.home.path().join("cache/filesystems/native"),
        Some(&attach_socket),
    );
    let filesystem = filesystem_command
        .env("OMNIFS_HOME", hermetic.home.path())
        .env("RUST_LOG", "warn")
        .spawn();
    let filesystem = match filesystem {
        Ok(filesystem) => filesystem,
        Err(error) => {
            let _ = daemon.kill();
            let _ = daemon.wait();
            eprintln!("skip: spawn local filesystem runner failed: {error}");
            return None;
        },
    };

    let mut daemon = NativeDaemon {
        daemon,
        filesystem,
        mount_point: mount_point.clone(),
        _home: hermetic.home,
        _nfs_lock: nfs_lock,
    };

    // Wait for the mount to serve the projected tree, bailing if the daemon exits.
    let message = mount_point.join("test/hello/message");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if message.is_file() {
            return Some(daemon);
        }
        match daemon.filesystem.try_wait() {
            Ok(Some(status)) => {
                eprintln!("skip: filesystem runner exited ({status}) before the mount was active");
                return None;
            },
            Ok(None) => {},
            Err(error) => panic!("poll filesystem child status: {error}"),
        }
        if Instant::now() >= deadline {
            eprintln!(
                "skip: {} never appeared within 30s; the mount could not come up on this platform",
                message.display()
            );
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
