//! `omnifs logs` — tail daemon output for whichever backend is running.
//!
//! Backend-aware: the Docker backend streams the container's logs; the
//! host-native backend logs to `<cache_dir>/daemon.log`, so there is no
//! container to attach to and `logs` tails the file directly.

use std::path::Path;
use std::process::Command;

use anyhow::Context as _;
use clap::Args;

use crate::error::{ExitCode, WithExitCode};
use crate::launch_backend::DockerTarget;
use crate::launch_record::LaunchRecord;
use crate::runtime::Runtime;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct LogsArgs {
    /// Container name (Docker backend only).
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    #[arg(short = 'f', long)]
    pub follow: bool,
}

impl LogsArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let paths = workspace.layout();
        let record = LaunchRecord::read(&paths.config_dir)?;

        // Host-native backend: no container exists; the daemon logs to a host
        // file, so tail that instead of telling the user to start Docker.
        if let Some(record) = &record
            && record.container_name().is_none()
        {
            return tail_native_log(&paths.cache_dir.join("daemon.log"), self.follow);
        }

        // Docker backend: an explicit `--container-name` wins, then the recorded
        // container, then the resolved default.
        let config = workspace.config()?;
        let container_name = self.container_name.clone().or_else(|| {
            record
                .as_ref()
                .and_then(LaunchRecord::container_name)
                .map(str::to_owned)
        });
        let target = DockerTarget::resolve(container_name, None, &config)?;
        let runtime = Runtime::connect_ready(&target, "omnifs logs")
            .await
            .with_exit_code(ExitCode::DaemonUnavailable)?;
        let container_name = target.container_name().clone();

        if self.follow {
            runtime.exec_follow_log(&container_name).await
        } else {
            runtime.container_logs(&container_name, None).await
        }
    }
}

/// Tail the host-native daemon log file. `--follow` streams new lines (and
/// survives rotation via `tail -F`); otherwise it prints the whole file.
fn tail_native_log(log_path: &Path, follow: bool) -> anyhow::Result<()> {
    if !log_path.exists() {
        anyhow::bail!(
            "no daemon log at {}; the host-native daemon may not be running (start it with `omnifs up`)",
            log_path.display()
        );
    }
    let mut cmd = Command::new("tail");
    if follow {
        cmd.arg("-F").arg("-n").arg("100");
    } else {
        cmd.arg("-n").arg("+1");
    }
    cmd.arg(log_path);
    // A clean exit or a signal stop (Ctrl-C on `-F`) both return the user; only
    // a failure to spawn `tail` is an error worth surfacing.
    cmd.status()
        .with_context(|| format!("tail {}", log_path.display()))?;
    Ok(())
}
