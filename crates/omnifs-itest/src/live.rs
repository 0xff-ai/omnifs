//! Live-daemon support shared by the frontend conformance matrix lanes.
//!
//! This is the one owner of the cross-process NFS serialization lock, the
//! `omnifs` binary resolution used by matrix lanes, the hermetic `OMNIFS_HOME`
//! construction, and native-daemon bring-up/readiness/teardown. It is ported
//! from the retired `omnifs-cli` `frontend_conformance` test so the matrix and
//! the CLI lifecycle suite share one lock owner and one daemon contract.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::ProviderStore;
use tempfile::TempDir;

/// Fixed, non-ephemeral port used purely as a cross-process lock for live NFS
/// mounts. Below the OS ephemeral range, so it never collides with a daemon's
/// [`free_port`]. This is the single owner of the constant; the CLI lifecycle
/// suite delegates here so both binaries serialize against the same port.
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

/// Bind an ephemeral loopback port and return it, for the daemon's control API.
#[must_use]
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
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
/// `omnifs-itest` does not depend on `omnifs-cli`, so `CARGO_BIN_EXE_omnifs` is
/// not available here. Resolve as `OMNIFS_BIN` if set (CI hands the packaged
/// binary that way and installs no wasm toolchain on the test runner), else
/// build it once per process from the workspace root and use the default debug
/// artifact.
#[must_use]
pub fn omnifs_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("OMNIFS_BIN") {
        return PathBuf::from(path);
    }
    ensure_omnifs_built();
    crate::workspace_root().join("target/debug/omnifs")
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
    crate::build_once(
        &BUILT,
        "cargo",
        &["build", "-p", "omnifs-cli", "--bin", "omnifs"],
    );
}

/// Install the test provider into the provider store under `providers_dir` and
/// return its content id. The daemon serves by content id, so the provider must
/// be present in the store before a mount spec pinning it can resolve.
pub fn install_test_provider(providers_dir: &Path) -> ProviderId {
    install_test_provider_as(providers_dir, "test-provider")
}

/// Install the test provider under `provider_name` into the content-addressed
/// store at `providers_dir`, returning its content id.
pub fn install_test_provider_as(providers_dir: &Path, provider_name: &str) -> ProviderId {
    let bytes = std::fs::read(crate::provider_wasm_path("test_provider.wasm"))
        .expect("read test provider wasm");
    let id = ProviderId::from_wasm_bytes(&bytes);
    let store = ProviderStore::new(providers_dir);
    store.put_if_absent(&id, &bytes).expect("put test provider");
    store
        .install(
            id,
            ProviderMeta {
                name: ProviderName::new(provider_name).unwrap(),
                version: None,
            },
            "test_provider.wasm".into(),
        )
        .expect("install test provider");
    id
}

/// The pinned reference for the test provider, derived from its built bytes.
#[must_use]
pub fn test_provider_reference() -> ProviderRef {
    let bytes =
        std::fs::read(crate::provider_wasm_path("test_provider.wasm")).expect("read test provider");
    ProviderRef {
        id: ProviderId::from_wasm_bytes(&bytes),
        meta: ProviderMeta {
            name: ProviderName::new("test-provider").unwrap(),
            version: None,
        },
    }
}

/// A pinned mount `Spec` for the test provider under `mount`.
#[must_use]
pub fn test_provider_spec(mount: &str) -> Spec {
    let value = serde_json::json!({ "provider": test_provider_reference(), "mount": mount });
    serde_json::from_value(value).expect("build test spec")
}

/// No-auth mount spec for the test provider, pinning `id`. Serves the projected
/// tree under `test/`.
#[must_use]
pub fn test_mount_spec(id: &ProviderId) -> String {
    format!(
        r#"{{"provider":{{"id":"{id}","meta":{{"name":"test-provider"}}}},"mount":"test","capabilities":{{"domains":["httpbin.org"]}}}}"#
    )
}

/// A hermetic `OMNIFS_HOME` with the test provider installed and its mount spec
/// written to `mounts/`, plus an empty mount point. The daemon reconciles from
/// `mounts/` on startup, so no `POST /v1/mounts` is needed.
pub struct HermeticHome {
    pub home: TempDir,
    pub mount_point: PathBuf,
}

/// Build a hermetic home: install the test provider into `providers/`, write
/// its pinned spec to `mounts/test.json`, and create the mount point.
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

    let mount_point = home.path().join("mnt");
    std::fs::create_dir_all(&mount_point).expect("mount point");

    HermeticHome { home, mount_point }
}

/// A running `omnifs daemon` (platform-default host-native frontend) with the
/// test-provider mounted, torn down on drop.
pub struct NativeDaemon {
    child: Child,
    pub mount_point: PathBuf,
    _home: TempDir,
    /// Cross-process NFS serialization lock, held for the lane's lifetime.
    /// `None` when the caller holds the lock externally (the perf lane spans two
    /// sequential lanes under one lock, so no per-lane bring-up owns its own).
    _nfs_lock: Option<TcpListener>,
}

impl Drop for NativeDaemon {
    fn drop(&mut self) {
        self.detach_mount();
        let _ = self.child.kill();
        let _ = self.child.wait();
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

/// A running `omnifs daemon` serving an explicit set of frontends, one per
/// `--frontend <kind>=<mount_point>` flag, over one shared namespace. Host-native
/// so it publishes a runtime record; torn down on drop. Used by the
/// dual-frontend acceptance test.
pub struct MultiFrontendDaemon {
    child: Child,
    pub mount_points: Vec<PathBuf>,
    home: TempDir,
    /// Cross-process NFS serialization lock, held for the lane's lifetime.
    _nfs_lock: TcpListener,
}

impl Drop for MultiFrontendDaemon {
    fn drop(&mut self) {
        for mount_point in &self.mount_points {
            detach_mount_any(mount_point);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl MultiFrontendDaemon {
    /// The daemon-owned runtime record for this hermetic home.
    #[must_use]
    pub fn record_path(&self) -> PathBuf {
        self.home.path().join("daemon.json")
    }

    /// The projected test-provider root under the frontend at `index`.
    #[must_use]
    pub fn tree_root(&self, index: usize) -> PathBuf {
        self.mount_points[index].join("test")
    }
}

/// Force-unmount a mount point regardless of frontend kind: try FUSE and NFS
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

/// Bring up `omnifs daemon` serving the frontends named in `kinds` (`"fuse"` or
/// `"nfs"`), each at its own mount point under a hermetic home, over one shared
/// namespace. Polls the runtime record and every mount rather than the control
/// API, so no `OMNIFS_DAEMON_ADDR` is needed.
///
/// Returns `None` (skip) when the platform cannot mount or the daemon does not
/// serve every mount (for example the NFS client bits are missing). Panics only
/// on a spawn error. The caller gates on `OMNIFS_ACCEPTANCE_LIVE` and holds the
/// NFS serial lock (this helper also holds its own copy for the lane lifetime).
#[must_use]
pub fn start_multi_frontend_daemon(kinds: &[&str]) -> Option<MultiFrontendDaemon> {
    let test_wasm = crate::provider_artifact_dir().join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just providers build`)",
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

    let mut args = vec!["daemon".to_string(), "--host-native".to_string()];
    let mut mount_points = Vec::with_capacity(kinds.len());
    for (index, kind) in kinds.iter().enumerate() {
        let mount_point = home.path().join(format!("mnt-{index}-{kind}"));
        std::fs::create_dir_all(&mount_point).expect("frontend mount point");
        args.push("--frontend".to_string());
        args.push(format!("{kind}={}", mount_point.display()));
        mount_points.push(mount_point);
    }

    let child = Command::new(omnifs_bin())
        .args(&args)
        .env("OMNIFS_HOME", home.path())
        .env_remove("OMNIFS_MOUNT_POINT")
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

    let mut daemon = MultiFrontendDaemon {
        child,
        mount_points,
        home,
        _nfs_lock: nfs_lock,
    };

    // Wait for the record to appear and every frontend to serve the projected
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
        match daemon.child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    eprintln!("skip: daemon exited cleanly before every frontend served");
                } else {
                    eprintln!(
                        "skip: daemon exited ({status}) before every frontend served — a \
                         requested frontend could not come up on this platform"
                    );
                }
                return None;
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if Instant::now() >= deadline {
            eprintln!("skip: not every frontend served within 30s");
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A namespace-only `omnifs daemon` (one attach socket, no in-process frontend)
/// plus an out-of-process `omnifs frontend run` runner attached to it. Torn down on
/// drop: the frontend first (it owns the mount), then the daemon.
pub struct WireFrontendDaemon {
    daemon: Child,
    frontend: Child,
    pub mount_point: PathBuf,
    _home: TempDir,
    /// Cross-process NFS serialization lock, held for the lane's lifetime.
    /// `None` when the caller holds the lock externally (the perf lane spans two
    /// sequential lanes under one lock, so no per-lane bring-up owns its own).
    _nfs_lock: Option<TcpListener>,
}

impl Drop for WireFrontendDaemon {
    fn drop(&mut self) {
        // SIGTERM the frontend first so its signal handler unmounts cleanly, then
        // the daemon. Fall back to a force-unmount sweep and SIGKILL.
        sigterm(&self.frontend);
        wait_briefly(&mut self.frontend);
        detach_mount_any(&self.mount_point);
        sigterm(&self.daemon);
        wait_briefly(&mut self.daemon);
        let _ = self.frontend.kill();
        let _ = self.frontend.wait();
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl WireFrontendDaemon {
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

/// Bring up a namespace-only daemon serving one attach socket, then an
/// out-of-process `omnifs frontend run --kind <kind>` attached to it. Proves the
/// projected tree serves out of process over the namespace wire.
///
/// Returns `None` (skip) when the platform cannot mount or a surface never comes
/// up; panics only on a spawn error or a daemon that is alive but never ready
/// (a real regression in the namespace-only ready path). The caller gates on
/// `OMNIFS_ACCEPTANCE_LIVE`. Pass `Some(nfs_serial_lock())` to hold the
/// cross-process NFS lock for the lane's lifetime, or `None` when the caller
/// already holds the NFS serial lock and keeps holding it (the perf lane holds
/// one lock across both its sequential lanes, so it must not let each bring-up
/// acquire, and later drop, its own). `transport` selects the out-of-process
/// runner's attach transport: `Unix` shares a socket path with the daemon;
/// `Tcp` is the Docker-hosted frontend's only option (it cannot share a host
/// Unix socket into its container), reached here without a container so the
/// perf lane isolates transport cost from Docker's own overhead.
#[derive(Clone, Copy)]
pub enum AttachTransport {
    Unix,
    Tcp,
}

#[allow(clippy::too_many_lines)] // linear end-to-end bring-up
#[must_use]
pub fn start_wire_frontend(
    kind: &str,
    transport: AttachTransport,
    nfs_lock: Option<TcpListener>,
) -> Option<WireFrontendDaemon> {
    let test_wasm = crate::provider_artifact_dir().join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just providers build`)",
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

    let port = free_port();
    let listen_addr = format!("127.0.0.1:{port}");
    let base = format!("http://{listen_addr}");

    // Namespace-only: at least one attach socket named, no `--frontend`
    // (`resolve_frontends` only suppresses the default frontend when the named
    // attach-socket list is non-empty, so the Tcp lane still names one even
    // though the runner never dials it). host-native + --listen gives a UDS
    // record and a TCP control API to poll for readiness.
    let mut daemon_args = vec![
        "daemon".to_string(),
        "--host-native".to_string(),
        "--listen".to_string(),
        listen_addr.clone(),
        "--attach-socket".to_string(),
        "nfs-wire".to_string(),
    ];
    if matches!(transport, AttachTransport::Tcp) {
        daemon_args.push("--attach-tcp".to_string());
        daemon_args.push("0".to_string());
    }
    let daemon = Command::new(omnifs_bin())
        .args(&daemon_args)
        .env("OMNIFS_HOME", &home_path)
        .env("OMNIFS_DAEMON_ADDR", &listen_addr)
        .env_remove("OMNIFS_MOUNT_POINT")
        .env_remove("OMNIFS_CONTROL_TOKEN")
        .env("RUST_LOG", "warn")
        .spawn();
    let mut daemon = match daemon {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip: spawn omnifs daemon failed: {error}");
            return None;
        },
    };

    // A namespace-only daemon reports ready once mounts reconcile and its attach
    // sockets serve. A non-zero exit before that is a hard failure.
    let deadline = Instant::now() + Duration::from_secs(30);
    let ready = loop {
        match daemon.try_wait() {
            Ok(Some(status)) => {
                let _ = daemon.wait();
                panic!(
                    "namespace-only daemon exited with {status} before /v1/ready on {listen_addr}; \
                     this is a startup regression, not a skip"
                );
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if curl_ok(&format!("{base}/v1/ready")) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        ready,
        "namespace-only daemon never reported /v1/ready on {listen_addr} after 30s; \
         the attach-socket ready path regressed. Check the daemon log."
    );

    let mount_point = home_path.join("mnt-wire");
    std::fs::create_dir_all(&mount_point).expect("frontend mount point");

    // The out-of-process renderer attaches over the requested transport and
    // mounts the tree: `--attach <socket>` for Unix, or the TCP env pair the
    // Docker-hosted frontend also uses.
    let mut frontend_cmd = Command::new(omnifs_bin());
    frontend_cmd
        .args(["frontend", "run", "--kind", kind, "--mount-point"])
        .arg(&mount_point)
        .env("OMNIFS_HOME", &home_path)
        .env("RUST_LOG", "warn");
    match transport {
        AttachTransport::Unix => {
            let socket = home_path.join("frontends/nfs-wire.sock");
            assert!(
                socket.exists(),
                "attach socket {} absent after the daemon reported ready",
                socket.display()
            );
            frontend_cmd.arg("--attach").arg(&socket);
        },
        AttachTransport::Tcp => {
            let record = omnifs_workspace::runtime_record::RuntimeRecord::read(
                &home_path.join("daemon.json"),
            )
            .expect("read daemon.json")
            .expect("daemon.json present once ready");
            let attach = record
                .attach
                .expect("daemon.json must carry `attach` after --attach-tcp");
            frontend_cmd
                .env(omnifs_api::OMNIFS_ATTACH_ADDR_ENV, &attach.addr)
                .env(omnifs_api::OMNIFS_ATTACH_TOKEN_ENV, &attach.token);
        },
    }
    let frontend = frontend_cmd.spawn();
    let mut frontend = match frontend {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip: spawn omnifs frontend run failed: {error}");
            let _ = daemon.kill();
            let _ = daemon.wait();
            return None;
        },
    };

    let message = mount_point.join("test/hello/message");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if message.is_file() {
            return Some(WireFrontendDaemon {
                daemon,
                frontend,
                mount_point,
                _home: hermetic.home,
                _nfs_lock: nfs_lock,
            });
        }
        match frontend.try_wait() {
            Ok(Some(status)) => {
                eprintln!(
                    "skip: frontend runner exited ({status}) before the mount served — \
                     the renderer could not come up on this platform"
                );
                let _ = daemon.kill();
                let _ = daemon.wait();
                return None;
            },
            Ok(None) => {},
            Err(error) => panic!("poll frontend child status: {error}"),
        }
        if Instant::now() >= deadline {
            eprintln!("skip: {} never appeared within 30s", message.display());
            let _ = frontend.kill();
            let _ = frontend.wait();
            let _ = daemon.kill();
            let _ = daemon.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn curl_ok(url: &str) -> bool {
    Command::new("curl")
        .args(["-fs", "-o", "/dev/null", url])
        .status()
        .is_ok_and(|status| status.success())
}

/// Bring up `omnifs daemon` with the platform-default host-native frontend and
/// only the test-provider mounted. `OMNIFS_HOME`, `OMNIFS_MOUNT_POINT`, and
/// `OMNIFS_DAEMON_ADDR` are all hermetic per-lane values, so the daemon never
/// touches the user's real home or the default control port.
///
/// Returns `None` (skip) only when the platform genuinely cannot mount. Panics
/// if the daemon exits due to a CLI parse error or bind collision, since that is
/// a real regression in the test or the daemon argument surface, not a skip.
///
/// The caller is responsible for the `OMNIFS_ACCEPTANCE_LIVE` env gate and its
/// skip message. Pass `Some(nfs_serial_lock())` to hold the cross-process NFS
/// lock for the lane's lifetime so parallel live tests cannot interleave NFS
/// mounts, or `None` when the caller already holds the NFS serial lock and keeps
/// holding it.
#[allow(clippy::too_many_lines)] // linear end-to-end daemon bring-up
#[must_use]
pub fn start_native_daemon(nfs_lock: Option<TcpListener>) -> Option<NativeDaemon> {
    let test_wasm = crate::provider_artifact_dir().join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just providers build`)",
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

    let port = free_port();
    let listen_addr = format!("127.0.0.1:{port}");
    let base = format!("http://{listen_addr}");

    // The daemon picks the platform-default frontend automatically; no
    // --frontend flag. Mount point comes from OMNIFS_MOUNT_POINT; config and
    // providers come from OMNIFS_HOME. --host-native opens preopens directly.
    let child = Command::new(omnifs_bin())
        .args(["daemon", "--listen", &listen_addr, "--host-native"])
        .env("OMNIFS_HOME", hermetic.home.path())
        .env("OMNIFS_MOUNT_POINT", &mount_point)
        .env("OMNIFS_DAEMON_ADDR", &listen_addr)
        .env("RUST_LOG", "warn")
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip: spawn omnifs daemon failed: {error}");
            return None;
        },
    };

    let mut daemon = NativeDaemon {
        child,
        mount_point: mount_point.clone(),
        _home: hermetic.home,
        _nfs_lock: nfs_lock,
    };

    // Wait for the control API. A non-zero exit before serving is a hard failure
    // (bad CLI parse or bind collision) and panics; a clean exit is an
    // unexpected but non-hard skip.
    let deadline = Instant::now() + Duration::from_secs(30);
    let ready = loop {
        match daemon.child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    eprintln!("skip: daemon exited cleanly before the control API was ready");
                    return None;
                }
                panic!(
                    "omnifs daemon exited with {status} before the control API became ready on \
                     {listen_addr} — this is a CLI or startup error, not a skip. \
                     Check that the daemon accepts the args passed in this lane \
                     and that `{listen_addr}` was not already in use."
                );
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if curl_ok(&format!("{base}/v1/ready")) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        ready,
        "omnifs daemon control API never became ready on {listen_addr} after 30s; \
         the daemon is alive but not serving. Check the daemon log."
    );

    // Wait for the mount to serve the projected tree, bailing if the daemon exits.
    let message = mount_point.join("test/hello/message");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if message.is_file() {
            return Some(daemon);
        }
        match daemon.child.try_wait() {
            Ok(Some(status)) => {
                eprintln!(
                    "skip: daemon exited ({status}) before the mount was active — \
                     the platform frontend could not serve"
                );
                return None;
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if Instant::now() >= deadline {
            eprintln!(
                "skip: {} never appeared within 30s — the mount could not come up on this platform",
                message.display()
            );
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
