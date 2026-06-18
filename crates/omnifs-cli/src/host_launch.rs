//! Host-native daemon launch for `omnifs up` on macOS.
//!
//! Spawns `omnifsd` as a detached child serving the NFS frontend and
//! self-mounting the loopback export, then waits for the control API to
//! report readiness. The daemon must outlive the CLI process: `omnifs up`
//! returns while the mount stays serving, exactly as the Docker path leaves
//! a container running.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, Result};
use tokio::process::{Child, Command};

use crate::client::DaemonClient;

/// A spawned host-native `omnifsd` child plus its log path.
pub(crate) struct HostDaemon {
    child: Child,
    log_path: PathBuf,
}

impl HostDaemon {
    /// Spawn a detached `omnifsd` serving the NFS frontend at `mount_point`.
    ///
    /// The binary is resolved in order: `OMNIFSD_BIN`, a sibling `omnifsd`
    /// next to the running CLI executable, then bare `omnifsd` on `PATH`.
    pub(crate) fn spawn(config_dir: &Path, cache_dir: &Path, mount_point: &Path) -> Result<Self> {
        std::fs::create_dir_all(mount_point)
            .with_context(|| format!("create mount point {}", mount_point.display()))?;
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("create cache dir {}", cache_dir.display()))?;

        let binary = resolve_omnifsd_bin();
        let log_path = cache_dir.join("omnifsd.log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("create daemon log {}", log_path.display()))?;
        let log_err = log
            .try_clone()
            .with_context(|| format!("clone daemon log handle {}", log_path.display()))?;

        let listen = format!("127.0.0.1:{}", omnifs_api::DEFAULT_PORT);
        let mut command = Command::new(&binary);
        command
            .arg("--frontend")
            .arg("nfs")
            .arg("--mount-point")
            .arg(mount_point)
            .arg("--config-dir")
            .arg(config_dir)
            .arg("--cache-dir")
            .arg(cache_dir)
            .arg("--listen")
            .arg(&listen)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));

        // Put the daemon in its own process group so it is not signalled when
        // the CLI or its shell exits. A new group leader is enough to survive the
        // CLI returning; tokio's Child does not kill on drop, so detach() can
        // simply forget it. setsid is unnecessary here: the CLI never signals
        // its own process group, and the new group already detaches the daemon
        // from CLI-targeted SIGINT/SIGTERM delivery.
        #[cfg(unix)]
        command.process_group(0);

        let child = command
            .spawn()
            .with_context(|| format!("spawn omnifsd ({})", binary.display()))?;

        Ok(Self { child, log_path })
    }

    /// Block until the daemon reports the filesystem is serving.
    ///
    /// Polls `GET /v1/ready` up to ~30s. Fails fast if the child exits before
    /// readiness, surfacing the tail of the daemon log either way.
    pub(crate) async fn wait_ready(&mut self) -> Result<()> {
        for _ in 0..60 {
            if DaemonClient::new().ready().await {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait().context("poll omnifsd child status")? {
                anyhow::bail!(
                    "omnifsd exited before the mount became ready ({status})\n{}",
                    self.log_tail()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        anyhow::bail!(
            "omnifsd did not become ready within 30s\n{}",
            self.log_tail()
        );
    }

    /// Consume self and leave the daemon running after `omnifs up` returns.
    ///
    /// `tokio::process::Child` does not kill the process on drop, so forgetting
    /// the handle is exactly the right move: the spawned daemon keeps serving
    /// the mount independently of the CLI process.
    pub(crate) fn detach(self) {
        std::mem::forget(self.child);
    }

    /// Best-effort terminate the daemon (used on the `up` error path).
    pub(crate) async fn kill(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    /// Read the last few KiB of the daemon log for error context.
    fn log_tail(&self) -> String {
        const TAIL: usize = 4096;
        match std::fs::read(&self.log_path) {
            Ok(bytes) => {
                let start = bytes.len().saturating_sub(TAIL);
                format!(
                    "--- {} (tail) ---\n{}",
                    self.log_path.display(),
                    String::from_utf8_lossy(&bytes[start..])
                )
            },
            Err(error) => format!("(could not read {}: {error})", self.log_path.display()),
        }
    }
}

/// Resolve the `omnifsd` binary path: `OMNIFSD_BIN`, then a sibling of the
/// running CLI executable, then bare `omnifsd` resolved through `PATH`.
fn resolve_omnifsd_bin() -> PathBuf {
    if let Some(explicit) = std::env::var_os("OMNIFSD_BIN") {
        return PathBuf::from(explicit);
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join("omnifsd");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("omnifsd")
}
