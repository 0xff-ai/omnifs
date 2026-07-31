//! Live-daemon support shared by the filesystem conformance matrix lanes.
//!
//! This is the one owner of the cross-process NFS serialization lock, the
//! `omnifs` binary resolution used by matrix lanes, the hermetic `OMNIFS_HOME`
//! construction, and native-daemon bring-up/readiness/teardown. The matrix and
//! CLI lifecycle suite share this lock owner and daemon contract.

use std::ffi::OsString;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper_util::rt::TokioIo;
use omnifs_api::grpc::{self, wire};
use omnifs_api::{
    ApplyResourcesRequest, AttachmentDefinition, CONTROL_REQUEST_TIMEOUT_SECS,
    CONTROL_STREAM_PAYLOAD_MAX_BYTES, DaemonInfo, DaemonStatus, MountResourceDefinition,
    ProgressEventKind, ProgressTarget, ProviderDefinition, ProviderImportReceipt,
    ResourceDeclarations, ResourceDefinition,
};
use omnifs_core::{
    AttachmentProtocol, AttachmentRuntime, AttachmentSpec, MutationId, ProviderId, ResourceKind,
    ResourceName,
};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::runtime::{Builder, Runtime};
use tonic::Request;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

type ControlClient = wire::control_client::ControlClient<Channel>;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS);
const PROVIDER_CHUNK_BYTES: usize = CONTROL_STREAM_PAYLOAD_MAX_BYTES;

/// A unique-enough resource-apply id for a test fixture.
fn next_mutation_id() -> MutationId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&counter.to_be_bytes());
    MutationId::from_bytes(bytes)
}

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
/// Product lifecycle uses hidden `omnifs run-fs`; a few protocol conformance
/// lanes spawn the slim binary to isolate the wire boundary. Their operational
/// records still live under daemon state so these tests cannot recreate the
/// retired client-owned filesystem tree.
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
    let profile = state_dir.ancestors().nth(4).unwrap_or(state_dir);
    let control = profile.join(".r").join(format!("{}.sock", &instance[16..]));
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

/// A hermetic profile root and empty filesystem mount point.
pub struct HermeticHome {
    pub home: TempDir,
    pub mount_point: PathBuf,
}

/// Build a hermetic profile root and create the filesystem mount point.
#[must_use]
pub fn hermetic_home() -> HermeticHome {
    // macOS exposes a long per-user TMPDIR. Nested runtime and control socket
    // names can exceed sockaddr_un::sun_path there, so live lanes keep the
    // path string under the short `/tmp` alias.
    #[cfg(target_os = "macos")]
    let home = tempfile::tempdir_in("/tmp").expect("home tempdir");
    #[cfg(not(target_os = "macos"))]
    let home = tempfile::tempdir().expect("home tempdir");
    let mount_point = home.path().join("mnt");
    std::fs::create_dir_all(&mount_point).expect("mount point");

    HermeticHome { home, mount_point }
}

/// Build the hidden-daemon arguments for a direct test launch.
#[must_use]
pub fn daemon_args(_home: &Path) -> Vec<OsString> {
    vec![OsString::from("daemon")]
}

/// A running `omnifs daemon` and explicit local filesystem runner with the test
/// provider mounted, torn down on drop.
pub struct NativeDaemon {
    daemon: Child,
    pub mount_point: PathBuf,
    home: TempDir,
    /// Cross-process NFS serialization lock, held for the lane's lifetime.
    /// `None` when the caller holds the lock externally (the perf lane spans two
    /// sequential lanes under one lock, so no per-lane bring-up owns its own).
    _nfs_lock: Option<TcpListener>,
}

impl Drop for NativeDaemon {
    fn drop(&mut self) {
        if matches!(self.daemon.try_wait(), Ok(None)) {
            best_effort_remove_attachment(self.home.path(), "native");
        }
        self.detach_mount();
        sigterm(&self.daemon);
        wait_briefly(&mut self.daemon);
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
    attachment_names: Vec<String>,
    pub mount_points: Vec<PathBuf>,
    home: TempDir,
    /// Cross-process NFS serialization lock, held for the lane's lifetime.
    _nfs_lock: TcpListener,
}

impl Drop for MultiFilesystemDaemon {
    fn drop(&mut self) {
        if matches!(self.daemon.try_wait(), Ok(None)) {
            for name in &self.attachment_names {
                best_effort_remove_attachment(self.home.path(), name);
            }
        }
        for mount_point in &self.mount_points {
            detach_mount_any(mount_point);
        }
        sigterm(&self.daemon);
        wait_briefly(&mut self.daemon);
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl MultiFilesystemDaemon {
    /// Live daemon status for this hermetic home.
    #[must_use]
    pub fn status(&self) -> omnifs_api::DaemonStatus {
        control_status(&self.home.path().join("control.sock"))
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
    seed_test_namespace(&control_socket);

    let mut attachment_names = Vec::with_capacity(kinds.len());
    for ((index, kind), mount_point) in kinds.iter().enumerate().zip(&mount_points) {
        let id = format!("live-{index}");
        match *kind {
            "fuse" | "nfs" => ensure_host_attachment(home.path(), &id, kind, mount_point),
            other => panic!("unsupported filesystem kind `{other}`"),
        }
        attachment_names.push(id);
    }

    let mut daemon = MultiFilesystemDaemon {
        daemon,
        attachment_names,
        mount_points,
        home,
        _nfs_lock: nfs_lock,
    };

    // Wait for every daemon-owned Attachment to serve the projected tree.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let all_serving = daemon
            .mount_points
            .iter()
            .all(|mp| mp.join("test/hello/message").is_file());
        if all_serving {
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
    seed_test_namespace(&control_socket);

    let mount_point = home_path.join("mnt-wire");
    std::fs::create_dir_all(&mount_point).expect("filesystem mount point");

    // The out-of-process renderer attaches over the requested transport and
    // mounts the tree: `--attach <socket>` for Unix, or the VFS TCP env pair.
    let state_dir = home_path.join("daemon-state/runtime/attachments/wire");
    let attach_socket = home_path.join("daemon-state/local.sock");
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
            let status = control_status(&control_socket);
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

/// Import the test provider and declare the two mounts used by live
/// conformance fixtures through the resource apply path.
pub fn seed_test_namespace(socket: &Path) -> ProviderId {
    let bytes = std::fs::read(crate::provider_wasm_path("test_provider.wasm"))
        .expect("read test provider wasm");
    let provider = ProviderId::from_wasm_bytes(&bytes);
    let receipt = import_provider(socket, bytes);
    assert_eq!(receipt.provider.id, provider);

    apply_test_namespace(socket, provider);
    provider
}

pub fn control_ready(socket: &Path) -> bool {
    if !socket.exists() {
        return false;
    }
    match block_on(ready_rpc(socket.to_path_buf())) {
        Err(_) => false,
        Ok(Ok(())) => true,
        Ok(Err(status)) if status.code() == tonic::Code::FailedPrecondition => false,
        Ok(Err(status)) => panic!("malformed daemon readiness reply: {status}"),
    }
}

fn control_socket_ready(socket: &Path) -> bool {
    control_ready(socket)
}

/// Apply one daemon-owned host Attachment and wait for its revision to reach
/// the ready phase. Live acceptance fixtures use this path instead of taking
/// ownership of a runner process themselves.
fn ensure_host_attachment(home: &Path, name: &str, protocol: &str, location: &Path) {
    let protocol = protocol
        .parse::<AttachmentProtocol>()
        .unwrap_or_else(|error| panic!("invalid test Attachment protocol: {error}"));
    ensure_attachment(
        &home.join("control.sock"),
        AttachmentDefinition {
            name: ResourceName::new(name).expect("valid test Attachment name"),
            spec: AttachmentSpec::new(
                protocol,
                AttachmentRuntime::Host,
                location.to_path_buf(),
                None,
                None,
            )
            .expect("valid host Attachment spec"),
        },
    );
}

/// Add or replace one desired Attachment through the same typed apply and
/// progress protocol used by the CLI, preserving every other desired resource.
pub fn ensure_attachment(socket: &Path, definition: AttachmentDefinition) {
    update_attachment(socket, Some(definition));
}

fn best_effort_remove_attachment(home: &Path, name: &str) {
    let _ = Command::new(omnifs_bin())
        .args(["attachment", "rm", name, "--yes"])
        .env("OMNIFS_HOME", home)
        .output();
}

fn update_attachment(socket: &Path, definition: Option<AttachmentDefinition>) {
    block_on(async {
        let mut client = connect(socket.to_path_buf())
            .await
            .expect("connect to update test Attachment");
        let snapshot = client
            .get_resources(Request::new(wire::Empty {}))
            .await
            .expect("get resources before updating test Attachment")
            .into_inner();
        let snapshot = grpc::get_resources_response(&snapshot).expect("decode resource snapshot");
        let mut resources = snapshot.resources;
        if let Some(definition) = definition {
            resources.retain(|resource| {
                resource.kind() != ResourceKind::Attachment || resource.name() != &definition.name
            });
            resources.push(ResourceDefinition::Attachment(definition));
        }
        let declarations = ResourceDeclarations {
            api_version: omnifs_api::API_VERSION.to_owned(),
            resources,
        };
        apply_and_wait(&mut client, snapshot.revision, declarations).await;
    });
}

async fn apply_and_wait(
    client: &mut ControlClient,
    base_revision: omnifs_core::ResourceRevision,
    declarations: ResourceDeclarations,
) {
    let desired = declarations
        .clone()
        .normalize()
        .expect("normalize test resources");
    let response = client
        .apply_resources(grpc::to_apply_resources_request(&ApplyResourcesRequest {
            mutation_id: next_mutation_id(),
            base_revision,
            expected_desired_digest: desired.digest(),
            declarations,
            credential_material: Vec::new(),
        }))
        .await
        .expect("apply test Attachment resources")
        .into_inner();
    let receipt = grpc::apply_resources_response(&response).expect("decode apply receipt");
    let mut stream = client
        .watch_progress(grpc::to_progress_target(ProgressTarget::DesiredRevision(
            receipt.revision,
        )))
        .await
        .expect("watch test Attachment revision")
        .into_inner();
    let deadline = tokio::time::Instant::now() + Duration::from_mins(3);
    loop {
        let message = tokio::time::timeout_at(deadline, stream.message())
            .await
            .expect("test Attachment revision reaches a terminal state")
            .expect("read test Attachment progress")
            .expect("test Attachment progress stream remains open");
        match grpc::progress_event(&message)
            .expect("decode test Attachment progress")
            .event
        {
            ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot)
                if snapshot.observed_revision == Some(receipt.revision) =>
            {
                return;
            },
            ProgressEventKind::RevisionReady(revision) if revision == receipt.revision => return,
            ProgressEventKind::RevisionFailed { detail, .. } => {
                panic!("test Attachment revision failed: {detail}")
            },
            ProgressEventKind::RevisionSuperseded { replaced_by, .. } => {
                panic!("test Attachment revision was superseded by {replaced_by}")
            },
            _ => {},
        }
    }
}

pub fn control_status(socket: &Path) -> DaemonStatus {
    block_on(status_rpc(socket.to_path_buf()))
        .unwrap_or_else(|error| panic!("query daemon status over tonic control socket: {error}"))
}

pub fn control_daemon_info(socket: &Path) -> DaemonInfo {
    block_on(daemon_info_rpc(socket.to_path_buf()))
        .unwrap_or_else(|error| panic!("query daemon info over tonic control socket: {error}"))
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tonic test runtime")
    })
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    runtime().block_on(future)
}

async fn connect(socket: PathBuf) -> Result<ControlClient, tonic::transport::Error> {
    Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(service_fn(move |_| {
            let socket = socket.clone();
            async move { UnixStream::connect(socket).await.map(TokioIo::new) }
        }))
        .await
        .map(ControlClient::new)
}

async fn ready_rpc(socket: PathBuf) -> Result<Result<(), tonic::Status>, tonic::transport::Error> {
    let mut client = connect(socket).await?;
    let mut request = Request::new(wire::Empty {});
    request.set_timeout(REQUEST_TIMEOUT);
    Ok(client.ready(request).await.map(|_| ()))
}

async fn status_rpc(socket: PathBuf) -> Result<DaemonStatus, String> {
    let mut client = connect(socket).await.map_err(|error| error.to_string())?;
    let mut request = Request::new(wire::Empty {});
    request.set_timeout(REQUEST_TIMEOUT);
    let response = client
        .get_status(request)
        .await
        .map_err(|error| error.to_string())?
        .into_inner();
    let status = response
        .status
        .as_ref()
        .ok_or_else(|| "malformed status reply: missing status".to_owned())?;
    grpc::daemon_status(status).map_err(|error| format!("malformed status reply: {error}"))
}

async fn daemon_info_rpc(socket: PathBuf) -> Result<DaemonInfo, String> {
    let mut client = connect(socket).await.map_err(|error| error.to_string())?;
    let mut request = Request::new(wire::Empty {});
    request.set_timeout(REQUEST_TIMEOUT);
    let response = client
        .get_daemon_info(request)
        .await
        .map_err(|error| error.to_string())?
        .into_inner();
    let info = response
        .info
        .as_ref()
        .ok_or_else(|| "malformed daemon info reply: missing info".to_owned())?;
    grpc::daemon_info(info).map_err(|error| format!("malformed daemon info reply: {error}"))
}

/// Provider import carries no mutation identity: the daemon dedupes by
/// content digest, so this is a plain streamed upload.
fn import_provider(socket: &Path, bytes: Vec<u8>) -> ProviderImportReceipt {
    block_on(async {
        let mut client = connect(socket.to_path_buf())
            .await
            .expect("connect to daemon control socket");
        let digest = ProviderId::from_wasm_bytes(&bytes);
        let payload = Bytes::from(bytes);
        let mut items = Vec::with_capacity(payload.len().div_ceil(PROVIDER_CHUNK_BYTES) + 1);
        items.push(wire::ImportProviderRequest {
            value: Some(wire::import_provider_request::Value::Start(
                grpc::to_provider_upload_start("test_provider.wasm", payload.len() as u64, &digest),
            )),
        });
        for start in (0..payload.len()).step_by(PROVIDER_CHUNK_BYTES) {
            let end = (start + PROVIDER_CHUNK_BYTES).min(payload.len());
            items.push(wire::ImportProviderRequest {
                value: Some(wire::import_provider_request::Value::Chunk(
                    payload.slice(start..end),
                )),
            });
        }
        let mut request = Request::new(tokio_stream::iter(items));
        request.set_timeout(Duration::from_mins(3));
        let response = client
            .import_provider(request)
            .await
            .expect("provider import request");
        response
            .into_inner()
            .receipt
            .as_ref()
            .map(grpc::provider_import_receipt)
            .transpose()
            .expect("decode provider import receipt")
            .expect("provider import reply missing receipt")
    })
}

fn apply_test_namespace(socket: &Path, provider: ProviderId) {
    block_on(async {
        let mut client = connect(socket.to_path_buf())
            .await
            .expect("connect to daemon control socket");
        let provider_name = ResourceName::new("test-provider").expect("valid provider name");
        let declarations = ResourceDeclarations {
            api_version: omnifs_api::API_VERSION.to_owned(),
            resources: vec![
                ResourceDefinition::Provider(ProviderDefinition {
                    name: provider_name.clone(),
                    artifact: provider,
                }),
                ResourceDefinition::Mount(MountResourceDefinition {
                    name: ResourceName::new("test").expect("valid mount name"),
                    provider: provider_name.clone(),
                    credential: None,
                    config: serde_json::json!({}),
                    limits: None,
                }),
                ResourceDefinition::Mount(MountResourceDefinition {
                    name: ResourceName::new("test2").expect("valid mount name"),
                    provider: provider_name,
                    credential: None,
                    config: serde_json::json!({}),
                    limits: None,
                }),
            ],
        };
        let desired = declarations
            .clone()
            .normalize()
            .expect("normalize test namespace resources");
        let snapshot = client
            .get_resources(Request::new(wire::Empty {}))
            .await
            .expect("get resource snapshot")
            .into_inner()
            .snapshot
            .as_ref()
            .map(grpc::resource_snapshot)
            .transpose()
            .expect("decode resource snapshot")
            .expect("resource snapshot missing");
        let request = grpc::to_apply_resources_request(&ApplyResourcesRequest {
            mutation_id: next_mutation_id(),
            base_revision: snapshot.revision,
            expected_desired_digest: desired.digest(),
            declarations,
            credential_material: Vec::new(),
        });
        let response = client
            .apply_resources(Request::new(request))
            .await
            .expect("apply test namespace resources")
            .into_inner();
        let receipt = response
            .receipt
            .as_ref()
            .map(grpc::apply_receipt)
            .transpose()
            .expect("decode apply receipt")
            .expect("apply reply missing receipt");
        assert_eq!(receipt.desired_digest, desired.digest());
    });
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
    seed_test_namespace(&control_socket);

    #[cfg(target_os = "linux")]
    let protocol = "fuse";
    #[cfg(not(target_os = "linux"))]
    let protocol = "nfs";
    ensure_host_attachment(hermetic.home.path(), "native", protocol, &mount_point);

    let mut daemon = NativeDaemon {
        daemon,
        mount_point: mount_point.clone(),
        home: hermetic.home,
        _nfs_lock: nfs_lock,
    };

    // Wait for the mount to serve the projected tree, bailing if the daemon exits.
    let message = mount_point.join("test/hello/message");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if message.is_file() {
            return Some(daemon);
        }
        match daemon.daemon.try_wait() {
            Ok(Some(status)) => {
                eprintln!("skip: daemon exited ({status}) before the mount was active");
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
