//! Host-native daemon process launch.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::process::Command;

use crate::client::DaemonClient;
use crate::launch_backend::ProcessRole;

pub(crate) async fn launch(
    paths: &omnifs_workspace::layout::WorkspaceLayout,
    telemetry_enabled: bool,
    mount_revision: &omnifs_workspace::mounts::Revision,
    mount_snapshot: &Path,
) -> Result<()> {
    let cache_dir = &paths.cache_dir;
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("create cache dir {}", cache_dir.display()))?;

    let binary = std::env::current_exe().context("resolve the omnifs executable")?;
    let log_path = cache_dir.join("daemon.log");
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

    if std::env::var_os("RUST_LOG").is_none() {
        command.env("RUST_LOG", ProcessRole::Daemon.default_log_level());
    }
    if !telemetry_enabled {
        command.env(omnifs_workspace::telemetry::ENV_SWITCH, "0");
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn omnifs daemon ({})", binary.display()))?;
    let child_pid = child.id();
    let client = DaemonClient::for_layout(paths);
    for _ in 0..300 {
        if let Some(status) = child.try_wait().context("poll daemon child status")? {
            let cause = log_cause_suffix(&log_path);
            anyhow::bail!(
                "omnifs daemon exited before the mount became ready ({status}){cause}; run `omnifs logs` for the full daemon log"
            );
        }
        if client.ready().await {
            if let Some(pid) = child_pid {
                match client.status().await {
                    Ok(status) if status.pid == pid => {
                        drop(child);
                        return Ok(());
                    },
                    Ok(status) => {
                        let _ = child.kill().await;
                        anyhow::bail!(
                            "daemon readiness came from pid {}, not spawned pid {pid}; \
                             another omnifs daemon is already serving",
                            status.pid
                        );
                    },
                    Err(_) => {},
                }
            } else {
                drop(child);
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let cause = log_cause_suffix(&log_path);
    let _ = child.kill().await;
    anyhow::bail!(
        "omnifs daemon did not become ready within 30s{cause}; run `omnifs logs` for the full daemon log"
    )
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
