//! CLI-side dogfood metrics.
//!
//! Records one line per CLI invocation to the workspace-local `cli.jsonl`. The
//! writer, privacy contract, and record schema live in
//! [`omnifs_workspace::metrics`]; this is the thin CLI adapter that resolves the
//! workspace and reads the `[metrics] enabled` off-switch.
//!
//! It is called from every real exit point (main's return path, and the two
//! subcommands that `std::process::exit` themselves), so it must be
//! self-contained and never fail or block: a broken workspace or config simply
//! skips the record.

use crate::workspace::Workspace;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Record a completed CLI invocation. `cmd` is the top-level subcommand and
/// `exit` the process exit code. Best-effort; the internal `daemon` subcommand
/// is excluded by its callers (it records `daemon.jsonl` instead).
pub(crate) fn record_cli_exit(cmd: &str, exit: i32) {
    let Ok(workspace) = Workspace::resolve() else {
        return;
    };
    // A malformed config disables metrics rather than guessing: metrics is
    // never allowed to surface an error, and off is the safe default.
    let enabled = workspace.config().is_ok_and(|config| {
        config.metrics.enabled && omnifs_workspace::metrics::enabled_from_env()
    });
    workspace.metrics().sink(enabled).cli_event(cmd, exit);
}

pub(crate) async fn maybe_print_health_nudge(
    workspace: &Workspace,
    output: crate::ui::output::Output,
) {
    let path = workspace.metrics().last_nudge();
    if !nudge_due(&path) {
        return;
    }
    let Some(line) = health_nudge(workspace).await else {
        return;
    };
    // The nudge is a conversational aside; `-q` drops it.
    output.narrate(line);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let _ = std::fs::write(path, format!("{now}\n"));
}

fn nudge_due(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    modified
        .elapsed()
        .map_or(true, |elapsed| elapsed >= Duration::from_hours(24))
}

async fn health_nudge(workspace: &Workspace) -> Option<String> {
    let inventory = crate::inventory::Inventory::collect(workspace).await.ok()?;
    for mount in inventory.mounts {
        match mount.auth {
            crate::inventory::AuthState::Missing { .. } => {
                return Some(format!(
                    "mount `{}` has a missing credential; run `omnifs mount reauth {}`",
                    mount.name, mount.name
                ));
            },
            crate::inventory::AuthState::Expired { .. } => {
                return Some(format!(
                    "mount `{}` has an expired credential; run `omnifs mount reauth {}`",
                    mount.name, mount.name
                ));
            },
            crate::inventory::AuthState::Error { .. } => {
                return Some(format!(
                    "mount `{}` has a credential error; run `omnifs mount reauth {}`",
                    mount.name, mount.name
                ));
            },
            crate::inventory::AuthState::NotNeeded | crate::inventory::AuthState::Ready => {},
        }
    }
    None
}
