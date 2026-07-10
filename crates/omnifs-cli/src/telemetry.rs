//! CLI-side dogfood telemetry.
//!
//! Records one line per CLI invocation to the workspace-local `cli.jsonl`. The
//! writer, privacy contract, and record schema live in
//! [`omnifs_workspace::telemetry`]; this is the thin CLI adapter that resolves the
//! workspace and reads the `[telemetry] enabled` off-switch.
//!
//! It is called from every real exit point (main's return path, and the two
//! subcommands that `std::process::exit` themselves), so it must be
//! self-contained and never fail or block: a broken workspace or config simply
//! skips the record.

use crate::config::Config;
use crate::workspace::Workspace;
use omnifs_api::CredentialHealth;
use omnifs_workspace::creds::FileStore;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Record a completed CLI invocation. `cmd` is the top-level subcommand and
/// `exit` the process exit code. Best-effort; the internal `daemon` subcommand
/// is excluded by its callers (it records `daemon.jsonl` instead).
pub(crate) fn record_cli_exit(cmd: &str, exit: i32) {
    let Ok(layout) = omnifs_workspace::layout::WorkspaceLayout::resolve() else {
        return;
    };
    // A malformed config disables telemetry rather than guessing: telemetry is
    // never allowed to surface an error, and off is the safe default.
    let enabled = Config::load(&layout.config_file).is_ok_and(|config| config.telemetry_enabled());
    omnifs_workspace::telemetry::TelemetrySink::new(&layout.config_dir, enabled)
        .cli_event(cmd, exit);
}

pub(crate) async fn maybe_print_health_nudge(workspace: &Workspace) {
    let path = workspace
        .layout()
        .config_dir
        .join("telemetry")
        .join("last-nudge");
    if !nudge_due(&path) {
        return;
    }
    let Some(line) = health_nudge(workspace).await else {
        return;
    };
    anstream::eprintln!("{line}");
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
    if workspace.daemon().ready().await
        && let Ok(Some(status)) = workspace.daemon().compatible_status_optional().await
    {
        for mount in &status.mounts {
            if let Some(health) = mount.auth_health
                && health.needs_attention()
            {
                return Some(format!(
                    "mount `{}` has {}; run `omnifs mounts reauth {}`",
                    mount.mount,
                    credential_health_noun(health),
                    mount.mount
                ));
            }
        }
        if let Some(failure) = status.failed.first() {
            // Load failures are not necessarily auth problems; doctor's live
            // section names the cause and the right fix per mount.
            return Some(format!(
                "mount `{}` failed to load; run `omnifs doctor` for details",
                failure.mount
            ));
        }
    }

    let mounts = workspace.mounts().ok()?;
    let store = FileStore::new(&workspace.layout().credentials_file);
    let statuses =
        crate::mount_report::scan_user_mount_configs(workspace.catalog(), &mounts, &store);
    for status in statuses {
        match status {
            crate::status::UserMountStatus::Ready(mount) => match mount.auth.terminal_row().kind {
                crate::auth::AuthTerminalKind::Missing => {
                    return Some(format!(
                        "mount `{}` has a missing credential; run `omnifs mounts reauth {}`",
                        mount.mount, mount.mount
                    ));
                },
                crate::auth::AuthTerminalKind::Error => {
                    return Some(format!(
                        "mount `{}` has a credential error; run `omnifs mounts reauth {}`",
                        mount.mount, mount.mount
                    ));
                },
                crate::auth::AuthTerminalKind::None | crate::auth::AuthTerminalKind::Ready => {},
            },
            crate::status::UserMountStatus::Invalid { .. } => {},
        }
    }
    None
}

fn credential_health_noun(health: CredentialHealth) -> &'static str {
    match health {
        CredentialHealth::Ready => "a ready credential",
        CredentialHealth::ExpiringSoon => "an expiring credential",
        CredentialHealth::Expired => "an expired credential",
        CredentialHealth::RefreshFailed => "a credential refresh failure",
        CredentialHealth::NeedsConsent => "a credential needing consent",
        CredentialHealth::Missing => "a missing credential",
        CredentialHealth::StaticUnvalidated => "an unvalidated static credential",
    }
}
