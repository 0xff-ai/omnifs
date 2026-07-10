//! `omnifs logs` — tail the daemon's log file.

use std::path::Path;
use std::process::Command;

use anyhow::Context as _;
use clap::Args;

use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct LogsArgs {
    #[arg(short = 'f', long)]
    pub follow: bool,
}

impl LogsArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let paths = workspace.layout();
        tail_native_log(&paths.cache_dir.join("daemon.log"), self.follow)
    }
}

/// Tail the host-native daemon log file. `--follow` streams new lines (and
/// survives rotation via `tail -F`); otherwise it prints the whole file.
fn tail_native_log(log_path: &Path, follow: bool) -> anyhow::Result<()> {
    if !log_path.exists() {
        anyhow::bail!(
            "no daemon log at {}; the daemon may not be running (start it with `omnifs up`)",
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
