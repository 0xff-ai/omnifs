//! `omnifs reset`: nuke every mount config (and, by default, the stored
//! credentials they reference) and tear down the running daemon.
//!
//! Bulk equivalent of `omnifs mounts rm` over every mount, plus
//! `omnifs down`. Reuses the credential-target logic so external-source
//! credentials (`token_file` / `token_env`) are left untouched.
//!
//! Backend-transparent: probes the control port then falls back to the launch
//! record, never branches on `[system].runtime`.

use std::fs;
use std::path::Path;

use anyhow::Context;
use clap::Args;

use crate::catalog::MountRemovalTarget;
use crate::client::DaemonClient;
use crate::commands::mounts::delete_credentials;
use crate::credential_target::CredentialTarget;
use crate::launch_backend::LaunchBackend;
use crate::launch_record::{LaunchRecord, backend_from_daemon};
use crate::paths::Paths;
use crate::workspace::Workspace;
use omnifs_creds::FileStore;

#[derive(Args, Debug, Clone, Default)]
pub struct ResetArgs {
    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Keep stored credentials; only delete mount configs and the daemon.
    #[arg(long)]
    pub keep_credentials: bool,
}

impl ResetArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let ResetArgs {
            yes,
            keep_credentials,
        } = self;
        let workspace = Workspace::resolve_default()?;
        let paths = workspace.paths();
        let targets = workspace.catalog().reset_removal_targets()?;

        if targets.is_empty() {
            anstream::println!("No mount configs found in {}.", paths.mounts_dir.display());
        }
        print_preview(&targets, keep_credentials);

        if !yes {
            let proceed = inquire::Confirm::new("Proceed?")
                .with_default(false)
                .prompt()
                .map_err(|error| anyhow::anyhow!("confirm prompt: {error}"))?;
            if !proceed {
                anstream::println!("Aborted.");
                return Ok(());
            }
        }

        // Tear down the daemon first so a daemon writing files won't race the
        // credential or mount-config delete. Best-effort: a non-running daemon
        // or an absent launch record is not a reset failure.
        teardown_daemon(workspace.daemon(), paths).await;

        let store = FileStore::new(&paths.credentials_file);
        for target in &targets {
            delete_credentials(&store, &target.credential, keep_credentials, &target.name)?;
            fs::remove_file(&target.path)
                .with_context(|| format!("remove {}", target.path.display()))?;
            anstream::println!("Removed mount `{}`", target.name);
        }
        if !targets.is_empty() {
            anstream::println!();
            anstream::println!("✓ Reset complete.");
        }
        Ok(())
    }
}

fn print_preview(targets: &[MountRemovalTarget], keep_credentials: bool) {
    anstream::println!("This will:");
    for target in targets {
        anstream::println!("  • delete {}", Paths::display(&target.path));
        match &target.credential {
            CredentialTarget::Internal(_) if !keep_credentials => {
                for key in target.credential.keys() {
                    anstream::println!("      and credential `{}`", key.storage_key());
                }
            },
            CredentialTarget::Internal(_) => {
                anstream::println!("      (keeping credentials, --keep-credentials)");
            },
            CredentialTarget::External(source) => {
                anstream::println!("      (external credential {source} unchanged)");
            },
            CredentialTarget::None => {},
        }
    }
    anstream::println!("  • stop the running daemon (if any)");
}

/// Best-effort daemon teardown using the same resolution order as `omnifs
/// down`: probe the control port, fall back to the launch record.
async fn teardown_daemon(client: &DaemonClient, paths: &crate::paths::Paths) {
    let config_dir = &paths.config_dir;
    let nfs_state_dir = paths.nfs_state_dir();

    // Try live daemon first.
    let backend = match resolve_backend(client, config_dir).await {
        Ok(Some(backend)) => backend,
        Ok(None) => {
            anstream::println!("⚠  No running daemon found; skipping daemon teardown");
            return;
        },
        Err(error) => {
            anstream::eprintln!("⚠  Could not identify daemon backend: {error:#}");
            return;
        },
    };

    // Graceful shutdown first.
    let mount_point = match client.shutdown().await {
        Ok(Some(report)) => {
            anstream::println!("✓ Daemon stopped");
            Some(report.mount_point)
        },
        Ok(None) => {
            anstream::println!("No daemon answered shutdown; sweeping…");
            None
        },
        Err(error) => {
            anstream::eprintln!("⚠  Daemon shutdown call failed: {error:#}");
            None
        },
    };

    // Backend-specific reclaim.
    if let Err(error) = backend
        .reclaim(mount_point.as_deref(), &nfs_state_dir)
        .await
    {
        anstream::eprintln!("⚠  Backend reclaim failed: {error:#}");
    }

    let _ = LaunchRecord::remove(config_dir);
}

/// Identify the backend from the live daemon or the launch record.
async fn resolve_backend(
    client: &DaemonClient,
    config_dir: &Path,
) -> anyhow::Result<Option<LaunchBackend>> {
    // Probe the live daemon; on any error fall through to the launch record.
    if let Ok(status) = client.status().await {
        let backend = backend_from_daemon(status.backend, config_dir)?;
        return Ok(Some(backend));
    }

    // Fall back to launch record.
    if let Some(record) = LaunchRecord::read(config_dir)? {
        return Ok(Some(record.into_backend()?));
    }

    Ok(None)
}
