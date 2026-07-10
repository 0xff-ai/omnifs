//! The krunkit readiness beacon.
//!
//! The krunkit guest's FUSE frontend runner has no way to observe its own mount
//! from outside the guest, so once its mount is live it dials host vsock on a
//! well-known port and writes a single `ready\n` line; the krunkit backend's
//! `mount_ready` (`crates/omnifs-cli/src/krunkit_backend.rs`) listens for that
//! line instead of polling the guest. Non-fatal end to end: the FUSE mount is
//! served either way, so a timed-out wait or a failed dial only logs a warning.

#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use tokio::runtime::Handle;
#[cfg(target_os = "linux")]
use tracing::{info, warn};

/// How often [`wait_until_mounted`] polls for the mount point becoming a
/// distinct filesystem from its parent.
#[cfg(target_os = "linux")]
const MOUNT_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Bound on [`wait_until_mounted`]'s wait: if the mount never comes up in this
/// window, the caller's own mount error path has almost certainly already
/// fired and the process is exiting anyway, so this just stops polling rather
/// than leaking the task forever.
#[cfg(target_os = "linux")]
const MOUNT_POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// Failure resolving the readiness-beacon vsock port from
/// `OMNIFS_READY_VSOCK_PORT`, before any connection is attempted.
#[derive(Debug, thiserror::Error)]
pub enum ReadyPortError {
    #[error("{env} `{value}` is not a valid port")]
    Invalid {
        env: &'static str,
        value: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("{env} is set but vsock is only available on Linux (the krunkit guest)")]
    UnsupportedPlatform { env: &'static str },
}

/// Parse `OMNIFS_READY_VSOCK_PORT` if set (only the krunkit guest's seed ever
/// sets it). Absence is not an error — today's behavior for every other
/// runner — but presence on a non-Linux target is: only the Linux krunkit
/// guest can dial vsock at all.
pub fn resolve_ready_vsock_port() -> Result<Option<u32>, ReadyPortError> {
    ready_vsock_port_from_env(std::env::var(omnifs_api::OMNIFS_READY_VSOCK_PORT_ENV).ok())
}

/// The env-driven half of [`resolve_ready_vsock_port`], pulled out as a pure
/// function of its one input so the parse/platform-check logic is
/// unit-testable without mutating process environment.
fn ready_vsock_port_from_env(value: Option<String>) -> Result<Option<u32>, ReadyPortError> {
    let Some(value) = value else {
        return Ok(None);
    };
    #[cfg(not(target_os = "linux"))]
    {
        let _ = value;
        Err(ReadyPortError::UnsupportedPlatform {
            env: omnifs_api::OMNIFS_READY_VSOCK_PORT_ENV,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let port: u32 = value.parse().map_err(|source| ReadyPortError::Invalid {
            env: omnifs_api::OMNIFS_READY_VSOCK_PORT_ENV,
            value,
            source,
        })?;
        Ok(Some(port))
    }
}

/// Spawn the krunkit readiness beacon: wait for `mount_point` to become a
/// live mount, then dial host vsock on `port` and write a single `ready\n`
/// line.
#[cfg(target_os = "linux")]
pub fn spawn_ready_signal(rt: &Handle, mount_point: PathBuf, port: u32) {
    rt.spawn(async move {
        if !wait_until_mounted(&mount_point).await {
            warn!(
                mount = %mount_point.display(),
                "timed out waiting for the mount to become live; readiness signal not sent"
            );
            return;
        }
        match signal_guest_ready(port).await {
            Ok(()) => info!(port, "sent the krunkit readiness signal"),
            Err(error) => warn!(%error, port, "failed to send the krunkit readiness signal"),
        }
    });
}

/// Poll until `path` is a distinct filesystem from its parent (i.e. is
/// mounted), or [`MOUNT_POLL_TIMEOUT`] elapses. A generic "is this a mount
/// point" check rather than a FUSE-specific one: by construction only the
/// frontend this process itself mounts will ever change `path`'s device.
#[cfg(target_os = "linux")]
async fn wait_until_mounted(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Some(parent) = path.parent() else {
        return false;
    };
    let deadline = tokio::time::Instant::now() + MOUNT_POLL_TIMEOUT;
    loop {
        if let (Ok(mount), Ok(parent_meta)) = (std::fs::metadata(path), std::fs::metadata(parent))
            && mount.dev() != parent_meta.dev()
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(MOUNT_POLL_INTERVAL).await;
    }
}

/// A vsock dial or write fault while sending the readiness signal. Only ever
/// logged (the FUSE mount is served either way), so it stays private to this
/// module.
#[cfg(target_os = "linux")]
#[derive(Debug, thiserror::Error)]
enum ReadySignalError {
    #[error("dial host vsock port {port} for the readiness signal: {source}")]
    Dial { port: u32, source: std::io::Error },
    #[error("write readiness line: {0}")]
    Write(#[source] std::io::Error),
}

/// Dial host vsock (`VMADDR_CID_HOST`) on `port` and write `ready\n`. Mirrors
/// the attach client's vsock dial ([`crate::AttachTarget::Vsock`]'s connect):
/// same CID, same crate.
#[cfg(target_os = "linux")]
async fn signal_guest_ready(port: u32) -> Result<(), ReadySignalError> {
    use tokio::io::AsyncWriteExt as _;
    let addr = tokio_vsock::VsockAddr::new(tokio_vsock::VMADDR_CID_HOST, port);
    let mut stream = tokio_vsock::VsockStream::connect(addr)
        .await
        .map_err(|source| ReadySignalError::Dial { port, source })?;
    stream
        .write_all(b"ready\n")
        .await
        .map_err(ReadySignalError::Write)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_vsock_port_absent_is_none() {
        assert!(ready_vsock_port_from_env(None).unwrap().is_none());
    }

    #[test]
    fn ready_vsock_port_rejects_a_non_numeric_value() {
        ready_vsock_port_from_env(Some("not-a-port".to_string()))
            .expect_err("a non-numeric port must fail");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ready_vsock_port_parses_on_linux() {
        let port = ready_vsock_port_from_env(Some("1025".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(port, 1025);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn ready_vsock_port_is_rejected_off_linux() {
        let error = ready_vsock_port_from_env(Some("1025".to_string())).unwrap_err();
        assert!(error.to_string().contains("only available on Linux"));
    }
}
