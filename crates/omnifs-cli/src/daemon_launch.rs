//! Host-native daemon process launch.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::process::Command;

use crate::client::DaemonClient;
use crate::error::{ExitCode, WithExitCode};
use crate::process::ProcessRole;

pub(crate) async fn launch(
    daemon: &DaemonClient,
    metrics_enabled: bool,
    mount_revision: &omnifs_workspace::mounts::Revision,
    mount_snapshot: &Path,
    offline: bool,
    readiness_timeout: Duration,
) -> Result<()> {
    let log_path = daemon.log_file();
    let cache_dir = log_path
        .parent()
        .context("daemon log path has no cache directory")?;
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("create cache dir {}", cache_dir.display()))?;

    let binary = std::env::current_exe().context("resolve the omnifs executable")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("clone daemon log handle {}", log_path.display()))?;

    let mut command = Command::new(&binary);
    command
        .arg("daemon")
        .arg("--mount-revision")
        .arg(mount_revision.to_string())
        .arg("--mount-snapshot")
        .arg(mount_snapshot)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if offline {
        command.arg("--offline");
    }

    if std::env::var_os("RUST_LOG").is_none() {
        command.env("RUST_LOG", ProcessRole::Daemon.default_log_level());
    }
    if !metrics_enabled {
        command.env(omnifs_workspace::metrics::ENV_SWITCH, "0");
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn omnifs daemon ({})", binary.display()))?;
    let child_pid = child
        .id()
        .context("spawned omnifs daemon has no process identity")?;
    let ready = tokio::time::timeout(readiness_timeout, async {
        loop {
            if let Some(status) = child.try_wait().context("poll daemon child status")? {
                let cause = log_cause_suffix(&log_path);
                // Daemon-shaped so the top-level human error block
                // quotes the log tail inline instead of only pointing at
                // `omnifs logs`; this message no longer repeats that pointer.
                return Err(anyhow::anyhow!(
                    "omnifs daemon exited before the mount became ready ({status}){cause}"
                ))
                .with_exit_code(ExitCode::DaemonUnavailable);
            }
            if daemon.ready().await
                && let Ok(status) = daemon.status().await
            {
                let record = daemon.record().context("read daemon readiness record")?;
                anyhow::ensure!(
                    record.as_ref().is_some_and(|record| {
                        status.pid == child_pid
                            && status.offline == offline
                            && record.pid == child_pid
                            && record.instance_id == status.instance_id
                            && record.mount_revision == *mount_revision
                            && record.offline == offline
                    }),
                    "daemon readiness did not match spawned pid {child_pid}, revision {mount_revision}, and offline={offline}"
                );
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    match ready {
        Ok(Ok(())) => {
            drop(child);
            Ok(())
        },
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error)
        },
        Err(_) => {
            let cause = log_cause_suffix(&log_path);
            let _ = child.kill().await;
            Err(anyhow::anyhow!(
                "omnifs daemon did not become ready within {}s{cause}",
                readiness_timeout.as_secs()
            ))
            .with_exit_code(ExitCode::DaemonUnavailable)
        },
    }
}

fn last_log_line(log_path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(log_path).ok()?;
    contents
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::to_owned)
}

fn log_cause_suffix(log_path: &Path) -> String {
    last_log_line(log_path).map_or_else(String::new, |line| format!(": {line}"))
}
