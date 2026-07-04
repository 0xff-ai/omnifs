//! Shared helpers for integration tests.

// env mutation helpers use unsafe set_var/remove_var (Rust 2024), allowed here
// because we hold ENV_LOCK across every mutation/restore pair.
#![allow(unsafe_code)]
#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

// Guard for env-mutating tests: env is process-global, so all tests that touch
// it must hold this lock.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Set environment variables for the duration of `f`, then restore previous values.
pub fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let saved: Vec<(&str, Option<String>)> = vars
        .iter()
        .map(|(key, _)| (*key, std::env::var(*key).ok()))
        .collect();

    // SAFETY: ENV_LOCK is held for the entire duration of this call.
    // No other thread mutates the environment concurrently.
    for (key, value) in vars {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    f();

    // SAFETY: ENV_LOCK is still held; restoring the saved values is subject
    // to the same serialization guarantee as the writes above.
    for (key, original) in &saved {
        match original {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}

/// `target/wasm32-wasip2/release`, where provider wasm lives.
pub fn release_wasm_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target/wasm32-wasip2/release")
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

pub fn omnifs_bin() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_omnifs")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_omnifs"))
        .map_or_else(
            || PathBuf::from(env!("CARGO_BIN_EXE_omnifs")),
            PathBuf::from,
        )
}

pub fn live_acceptance_enabled() -> bool {
    std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_some()
}

/// Return `true` if the platform can serve a mount. On Linux, FUSE requires
/// `/dev/fuse`. On macOS, NFS loopback is always available without root.
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

/// Fixed, non-ephemeral port used purely as a cross-process lock for live NFS
/// mounts. Below the OS ephemeral range, so it never collides with a daemon's
/// `free_port()`.
const NFS_LOCK_PORT: u16 = 48761;

/// Acquire the cross-process NFS serialization lock, returning the bound socket
/// as the guard. nextest runs each integration-test binary as its own process,
/// so an in-process mutex cannot serialize across binaries.
pub fn nfs_serial_lock() -> TcpListener {
    loop {
        match TcpListener::bind(("127.0.0.1", NFS_LOCK_PORT)) {
            Ok(listener) => return listener,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Install the test provider into the provider store under `providers_dir` and
/// return its content id.
pub fn install_test_provider(providers_dir: &Path) -> omnifs_workspace::ids::ProviderId {
    let bytes = std::fs::read(release_wasm_dir().join("test_provider.wasm"))
        .expect("read test provider wasm");
    let id = omnifs_workspace::ids::ProviderId::from_wasm_bytes(&bytes);
    let store = omnifs_workspace::provider::ProviderStore::new(providers_dir);
    store.put_if_absent(&id, &bytes).expect("put test provider");
    store
        .install(
            id,
            omnifs_workspace::ids::ProviderMeta {
                name: omnifs_workspace::ids::ProviderName::new("test-provider").unwrap(),
                version: None,
            },
            "test_provider.wasm".into(),
        )
        .expect("install test provider");
    id
}

/// No-auth mount spec for the test provider, pinning `id`. Serves
/// `test/hello/message`.
pub fn test_mount_spec(id: &omnifs_workspace::ids::ProviderId) -> String {
    format!(
        r#"{{"provider":{{"id":"{id}","meta":{{"name":"test-provider"}}}},"mount":"test","capabilities":{{"domains":["httpbin.org"]}}}}"#
    )
}
