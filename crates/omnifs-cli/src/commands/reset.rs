//! `omnifs reset`: nuke every mount config (and, by default, the stored
//! credentials they reference) and tear down the running daemon.
//!
//! Bulk equivalent of `omnifs mounts rm` over every mount, plus
//! `omnifs down`. Reuses the credential-target logic so external-source
//! credentials (`token_file` / `token_env`) are left untouched.
//!
//! Backend-transparent: probes the control port then falls back to the launch
//! record, never branches on `[system].runtime`.

use anyhow::Context;
use clap::Args;
use omnifs_auth::{CredentialService, OAuthClient};
use omnifs_workspace::mounts::Registry;

use crate::commands::mounts::delete_credentials;
use crate::credential_target::CredentialTarget;
use crate::daemon_teardown::DaemonTeardown;
use crate::workspace::{MountRemovalTarget, Workspace};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::layout::WorkspaceLayout;
use std::io::IsTerminal as _;
use std::sync::Arc;

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
        let workspace = Workspace::resolve()?;
        let layout = workspace.layout();
        let targets = workspace.reset_removal_targets()?;

        if targets.is_empty() {
            anstream::eprintln!("No mount configs found in {}.", layout.mounts_dir.display());
        }
        print_preview(&targets, keep_credentials);

        if !yes {
            // Without a terminal there is no one to answer; fail fast naming the
            // skip flag instead of surfacing inquire's raw NotATTY error.
            if !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "cannot confirm reset on a non-interactive stdin; pass -y to skip confirmation"
                );
            }
            let proceed = inquire::Confirm::new("Proceed?")
                .with_default(false)
                .prompt()
                .map_err(crate::ui::from_inquire)?;
            if !proceed {
                anstream::eprintln!("Aborted.");
                return Ok(());
            }
        }

        // Mount specs are deleted through the running daemon when it is ready
        // (live converge per mount); the trailing teardown no longer owns spec
        // mutation. Credential deletes go through the CredentialService so
        // OAuth credentials are revoked upstream first.
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(&layout.credentials_file));
        let service = CredentialService::new(store, OAuthClient::new()?);
        for target in &targets {
            let credential = if keep_credentials {
                target.credential.clone()
            } else {
                target
                    .config
                    .as_ref()
                    .map(|spec| {
                        crate::auth::MountAuth::from_spec(workspace.catalog(), spec.clone())
                            .register_revocation(&service)
                    })
                    .transpose()?
                    .unwrap_or_else(|| target.credential.clone())
            };
            delete_credentials(&service, &credential, keep_credentials, &target.name).await?;
            let daemon_delete = match workspace.daemon().delete_mount_if_ready(&target.name).await {
                Ok(report) => report,
                Err(error) => {
                    anstream::eprintln!(
                        "Running daemon could not remove mount `{}`: {error:#}",
                        target.name
                    );
                    None
                },
            };
            if daemon_delete.is_none() {
                let name = omnifs_workspace::mounts::Name::new(target.name.clone())
                    .with_context(|| format!("invalid mount name `{}`", target.name))?;
                Registry::load(&layout.mounts_dir)?
                    .remove(&name)
                    .with_context(|| format!("remove {}", target.path.display()))?;
            }
            anstream::eprintln!("Removed mount `{}`", target.name);
        }

        // Best-effort: a non-running daemon or an absent launch record is not a
        // reset failure. Mount specs were already deleted through the daemon
        // when it was ready, so teardown no longer owns spec mutation.
        DaemonTeardown::new(&workspace).reset_best_effort().await;

        if !targets.is_empty() {
            anstream::eprintln!();
            anstream::eprintln!("✓ Reset complete.");
        }
        crate::telemetry::maybe_print_health_nudge(&workspace).await;
        Ok(())
    }
}

fn print_preview(targets: &[MountRemovalTarget], keep_credentials: bool) {
    anstream::eprintln!("This will:");
    for target in targets {
        anstream::eprintln!("  • delete {}", WorkspaceLayout::display(&target.path));
        match &target.credential {
            CredentialTarget::Internal(_) if !keep_credentials => {
                for key in target.credential.keys() {
                    anstream::eprintln!("      and credential `{}`", key.storage_key());
                }
            },
            CredentialTarget::Internal(_) => {
                anstream::eprintln!("      (keeping credentials, --keep-credentials)");
            },
            CredentialTarget::None => {},
        }
    }
    anstream::eprintln!("  • stop the running daemon (if any)");
}
