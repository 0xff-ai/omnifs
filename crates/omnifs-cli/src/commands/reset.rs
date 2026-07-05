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

use anyhow::Context;
use clap::Args;
use omnifs_auth::{CredentialService, OAuthClient};

use crate::commands::mounts::delete_credentials;
use crate::credential_target::CredentialTarget;
use crate::daemon_teardown::DaemonTeardown;
use crate::workspace::{MountRemovalTarget, Workspace};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::layout::WorkspaceLayout;
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
            anstream::println!("No mount configs found in {}.", layout.mounts_dir.display());
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
        DaemonTeardown::new(&workspace).reset_best_effort().await;

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
                        crate::auth::mount_auth(workspace.catalog(), spec.clone())
                            .register_revocation(&service)
                    })
                    .transpose()?
                    .unwrap_or_else(|| target.credential.clone())
            };
            delete_credentials(&service, &credential, keep_credentials, &target.name).await?;
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
        anstream::println!("  • delete {}", WorkspaceLayout::display(&target.path));
        match &target.credential {
            CredentialTarget::Internal(_) if !keep_credentials => {
                for key in target.credential.keys() {
                    anstream::println!("      and credential `{}`", key.storage_key());
                }
            },
            CredentialTarget::Internal(_) => {
                anstream::println!("      (keeping credentials, --keep-credentials)");
            },
            CredentialTarget::None => {},
        }
    }
    anstream::println!("  • stop the running daemon (if any)");
}
