//! `omnifs reset`: nuke every mount config (and, by default, the stored
//! credentials they reference) and tear down the running container.
//!
//! Bulk equivalent of `omnifs mounts rm` over every mount, plus
//! `omnifs down`. Reuses the credential-target logic so external-source
//! credentials (`token_file` / `token_env`) are left untouched.

use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use omnifs_host::config::InstanceConfig;

use crate::app_context::AppContext;
use crate::catalog::ProviderCatalog;
use crate::commands::mounts::delete_credentials;
use crate::container_name::ContainerName;
use crate::credential_target::CredentialTarget;
use crate::paths::Paths;
use crate::runtime::Runtime;
use crate::session::{self, CredsBackend, ENV_CONTAINER_NAME};

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
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
}

struct MountTarget {
    name: String,
    path: PathBuf,
    credential: CredentialTarget,
}

impl ResetArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let ctx = AppContext::resolve_default()?;
        let paths = ctx.paths();
        let config = ctx.config();
        let container_name = self
            .container_name
            .clone()
            .or_else(|| session::env_string(ENV_CONTAINER_NAME))
            .or(config.container_name.clone())
            .unwrap_or_else(|| session::CONTAINER_NAME.to_string());
        let targets = scan_mounts(ctx.catalog())?;

        if targets.is_empty() {
            anstream::println!("No mount configs found in {}.", paths.mounts_dir.display());
        }
        print_preview(&targets, self.keep_credentials, &container_name);

        if !self.yes {
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
                self.keep_credentials,
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

fn scan_mounts(catalog: &ProviderCatalog) -> anyhow::Result<Vec<MountTarget>> {
    let mut out = Vec::new();
    for path in catalog.mount_config_paths()? {
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        // An unparsable mount is still removable — the user is trying to
        // nuke everything, including broken state.
        let credential = match InstanceConfig::from_file(&path) {
            Ok(config) => match catalog.into_effective_mount(config, false) {
                Ok(effective) => CredentialTarget::for_mount(&effective),
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "unresolvable mount config; will remove the file but cannot drop credentials"
                    );
                    CredentialTarget::None
                },
            },
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "unparsable mount config; will remove the file but cannot drop credentials"
                );
                CredentialTarget::None
            },
        };
        out.push(MountTarget {
            name,
            path,
            credential,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn print_preview(targets: &[MountTarget], keep_credentials: bool, container_name: &str) {
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

async fn teardown_container(container_name: &str) {
    match Runtime::connect_docker() {
        Ok(runtime) => match ContainerName::new(container_name.to_owned()) {
            Ok(container_name) => match runtime.remove_existing(&container_name).await {
                Ok(()) => anstream::println!("✓ Container `{container_name}` removed"),
                Err(error) => {
                    anstream::println!("⚠  Could not remove container `{container_name}`: {error}");
                },
            },
            Err(error) => {
                anstream::println!("⚠  Invalid container name `{container_name}`: {error}");
            },
        },
        Err(error) => {
            anstream::println!("⚠  Docker not reachable; skipping container teardown ({error})");
        },
    }
}
