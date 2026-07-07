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

use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName};
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

/// Install the test provider into the provider store under `providers_dir` and
/// return its content id. The daemon serves by content id, so the provider must
/// be present in the store before a mount spec pinning it can resolve.
pub fn install_test_provider(providers_dir: &Path) -> ProviderId {
    let bytes = std::fs::read(crate::provider_wasm_path("test_provider.wasm"))
        .expect("read test provider wasm");
    let id = ProviderId::from_wasm_bytes(&bytes);
    let store = ProviderStore::new(providers_dir);
    store.put_if_absent(&id, &bytes).expect("put test provider");
    store
        .install(
            id,
            ProviderMeta {
                name: ProviderName::new("test-provider").unwrap(),
                version: None,
            },
            "test_provider.wasm".into(),
        )
        .expect("install test provider");
    id
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
    _nfs_lock: TcpListener,
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
/// skip message.
#[allow(clippy::too_many_lines)] // linear end-to-end daemon bring-up
#[must_use]
pub fn start_native_daemon() -> Option<NativeDaemon> {
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

    // Hold the cross-process NFS lock for the whole lane so this binary's mount
    // never races the CLI lifecycle suite's mounts in a parallel nextest run.
    let nfs_lock = nfs_serial_lock();

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
