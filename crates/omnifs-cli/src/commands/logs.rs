//! `omnifs logs` — tail the daemon's log file. Content on stdout
//! is the daemon log verbatim, never restructured; the one narration line
//! (following vs. down, and the empty state) is stderr.

use std::path::Path;
use std::process::Command;

use anyhow::Context as _;
use clap::Args;

use crate::ui::output::Output;
use crate::ui::style::{self, Stream};
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct LogsArgs {
    #[arg(short = 'f', long)]
    pub follow: bool,
}

/// Default tail length: last 50 lines when not following.
const DEFAULT_TAIL_LINES: usize = 50;

impl LogsArgs {
    pub fn run(self, output: &Output) -> anyhow::Result<()> {
        if output.is_structured() {
            anyhow::bail!("logs is a passthrough command and only supports human output")
        }
        let workspace = Workspace::resolve()?;
        let log_path = workspace.daemon().log_file();
        if !log_path.exists() {
            output.narrate("No daemon log yet. It's written on first `omnifs up`.");
            return Ok(());
        }
        let running = daemon_is_running(&workspace);
        output.narrate(style::dim(
            header_line(&log_path, self.follow, running),
            Stream::Stderr,
        ));
        tail_log(&log_path, self.follow && running)
    }
}

/// Whether the daemon that owns this log is currently alive, best-effort: a
/// live process for the recorded pid. A false positive/negative here only
/// affects the narration header, never the log content itself.
fn daemon_is_running(workspace: &Workspace) -> bool {
    workspace
        .daemon()
        .record()
        .ok()
        .flatten()
        .is_some_and(|record| crate::process::is_alive(record.pid))
}

/// The one stderr header line. Pure so the exact wording is
/// testable without a live daemon.
fn header_line(log_path: &Path, follow: bool, running: bool) -> String {
    if !running {
        "daemon is not running; showing its last log".to_owned()
    } else if follow {
        format!(
            "tailing {}  (^C to stop)",
            omnifs_workspace::display(log_path)
        )
    } else {
        format!(
            "showing the last {DEFAULT_TAIL_LINES} lines of {}",
            omnifs_workspace::display(log_path)
        )
    }
}

/// Delegate both static and followed output to the platform `tail`, inheriting
/// stdout so Omnifs never decodes or rewrites log bytes.
fn tail_log(log_path: &Path, follow: bool) -> anyhow::Result<()> {
    let mut command = Command::new("tail");
    if follow {
        command.arg("-F");
    }
    let operation = if follow { "follow" } else { "read" };
    let status = command
        .arg("-n")
        .arg(DEFAULT_TAIL_LINES.to_string())
        .arg(log_path)
        .status()
        .with_context(|| format!("{operation} daemon log {}", log_path.display()))?;
    anyhow::ensure!(status.success(), "tail exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_line_names_following_down_and_the_default_case() {
        let path = Path::new("/home/u/.omnifs/cache/daemon.log");

        assert_eq!(
            header_line(path, false, false),
            "daemon is not running; showing its last log"
        );
        // A down daemon wins over `-f`: nothing new is coming, so the
        // "tailing" framing would be misleading.
        assert_eq!(
            header_line(path, true, false),
            "daemon is not running; showing its last log"
        );
        assert!(header_line(path, true, true).starts_with("tailing "));
        assert!(header_line(path, true, true).ends_with("  (^C to stop)"));
        assert_eq!(
            header_line(path, false, true),
            format!(
                "showing the last {DEFAULT_TAIL_LINES} lines of {}",
                omnifs_workspace::display(path)
            )
        );
    }
}
