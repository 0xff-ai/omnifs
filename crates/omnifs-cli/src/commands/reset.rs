//! `omnifs reset`: nuke every mount config (and, by default, the stored
//! credentials they reference) and tear down the running container.
//!
//! Bulk equivalent of `omnifs mounts rm` over every mount, plus
//! `omnifs down`. Reuses the credential-target logic so external-source
//! credentials (`token_file` / `token_env`) are left untouched.

use std::fs;

use anyhow::Context;
use clap::Args;

use crate::app_context::AppContext;
use crate::catalog::MountRemovalTarget;
use crate::commands::mounts::delete_credentials;
use crate::container_name::ContainerName;
use crate::credential_target::CredentialTarget;
use crate::paths::Paths;
use crate::runtime::Runtime;
use crate::runtime_target::RuntimeTarget;
use crate::session::CredsBackend;

#[derive(Args, Debug, Clone, Default)]
pub struct ResetArgs {
    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Keep stored credentials; only delete mount configs and the container.
    #[arg(long)]
    pub keep_credentials: bool,
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
}

impl ResetArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let ResetArgs {
            yes,
            keep_credentials,
            container_name,
        } = self;
        let ctx = AppContext::resolve_default()?;
        let paths = ctx.paths();
        let config = ctx.config();
        let container_name = RuntimeTarget::resolve_container_name(container_name, config)?;
        let targets = ctx.catalog().mount_removal_targets()?;

        if targets.is_empty() {
            anstream::println!("No mount configs found in {}.", paths.mounts_dir.display());
        }
        print_preview(&targets, keep_credentials, container_name.as_str());

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

        // Tear down the container first so a daemon writing files won't race
        // the credential or mount-config delete. Best-effort: a non-running
        // Docker (or an absent container) isn't a reset failure.
        teardown_container(&container_name).await;

        let store = CredsBackend::auto(&paths.credentials_file, false);
        for target in &targets {
            delete_credentials(
                store.as_ref(),
                &target.credential,
                keep_credentials,
                &target.name,
            )?;
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

fn print_preview(targets: &[MountRemovalTarget], keep_credentials: bool, container_name: &str) {
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
    anstream::println!("  • stop and remove container `{container_name}` (if running)");
}

async fn teardown_container(container_name: &ContainerName) {
    match Runtime::connect_docker() {
        Ok(runtime) => match runtime.remove_existing(container_name).await {
            Ok(()) => anstream::println!("✓ Container `{container_name}` removed"),
            Err(error) => {
                anstream::println!("⚠  Could not remove container `{container_name}`: {error}");
            },
        },
        Err(error) => {
            anstream::println!("⚠  Docker not reachable; skipping container teardown ({error})");
        },
    }
}
