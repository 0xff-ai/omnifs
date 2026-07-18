//! Shared helpers for integration tests.

// env mutation helpers use unsafe set_var/remove_var (Rust 2024), allowed here
// because we hold ENV_LOCK across every mutation/restore pair.
#![allow(unsafe_code)]
#![allow(dead_code)]

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

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

/// Acquire the cross-process NFS serialization lock. The port constant and the
/// bind loop have one owner in `omnifs-itest`, shared with the frontend
/// conformance matrix so both binaries serialize against the same port.
pub fn nfs_serial_lock() -> TcpListener {
    omnifs_itest::live::nfs_serial_lock()
}

/// The native daemon pid recorded in `<home>/daemon.json`, if present.
pub fn recorded_pid(home: &Path) -> Option<u32> {
    let bytes = std::fs::read_to_string(home.join("daemon.json")).ok()?;
    let record = serde_json::from_str::<serde_json::Value>(&bytes).ok()?;
    u32::try_from(record["pid"].as_u64()?).ok()
}

/// Best-effort force-unmount for a test mount. Safe during panic cleanup and
/// when nothing is mounted.
pub fn force_unmount(mount_point: &Path) {
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
                .output();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("fusermount")
            .args([OsStr::new("-uz"), mount_point.as_os_str()])
            .output();
        let _ = Command::new("umount").arg(mount_point).output();
    }
    #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
    {
        if omnifs_nfs::mount_is_active(mount_point) {
            let _ = Command::new("umount").arg("-f").arg(mount_point).output();
        }
    }
}

/// Install the test provider into the provider store under `providers_dir` and
/// return its content id.
pub fn install_test_provider(providers_dir: &Path) -> omnifs_workspace::ids::ProviderId {
    let artifact = omnifs_workspace::provider::Artifact::from_file(
        release_wasm_dir().join("test_provider.wasm"),
    )
    .expect("parse test provider wasm");
    let id = artifact.id();
    let store = omnifs_workspace::provider::ProviderStore::new(providers_dir);
    store.retain(&artifact).expect("retain test provider");
    id
}

/// No-auth mount spec for the test provider, pinning `id`. Serves
/// `test/hello/message`.
pub fn test_mount_spec(id: &omnifs_workspace::ids::ProviderId) -> String {
    format!(r#"{{"provider":{{"id":"{id}","meta":{{"name":"test-provider"}}}},"mount":"test"}}"#)
}

/// Install a synthetic fixture provider carrying only a
/// `omnifs.provider-metadata.v1` custom section built from `manifest` (no
/// real wasm component), returning its content id. `mount add` resolves auth
/// and config from this section alone, so a fixture never needs a real
/// provider crate to exercise auth-flow behavior. Duplicated from
/// `omnifs-cli`'s own private `test_support::wasm_with_provider_metadata`:
/// integration tests build as a separate crate and cannot reach `#[cfg(test)]`
/// items inside the library.
pub fn install_fixture_provider(
    providers_dir: &Path,
    manifest: &serde_json::Value,
) -> omnifs_workspace::ids::ProviderId {
    let id = manifest["id"].as_str().expect("manifest carries an id");
    let file = format!("omnifs_provider_{id}.wasm");
    let wasm = wasm_with_provider_metadata(manifest);
    let artifact = omnifs_workspace::provider::Artifact::from_bytes(file, wasm)
        .expect("parse fixture provider");
    let content_id = artifact.id();
    let store = omnifs_workspace::provider::ProviderStore::new(providers_dir);
    store.retain(&artifact).expect("retain fixture provider");
    content_id
}

fn wasm_with_provider_metadata(manifest: &serde_json::Value) -> Vec<u8> {
    let data = serde_json::to_vec(manifest).expect("serialize fixture manifest");
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    let mut section = Vec::new();
    let section_name = omnifs_workspace::provider::PROVIDER_METADATA_SECTION_NAME;
    push_uleb(section_name.len(), &mut section);
    section.extend_from_slice(section_name.as_bytes());
    section.extend_from_slice(&data);
    wasm.push(0);
    push_uleb(section.len(), &mut wasm);
    wasm.extend(section);
    wasm
}

fn push_uleb(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let mut byte = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}
