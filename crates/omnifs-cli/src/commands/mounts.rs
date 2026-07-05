//! `omnifs mounts rm <name>` — remove a mount config and (by default) its
//! stored credential.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use omnifs_auth::{CredentialService, OAuthClient};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::mounts::Name as MountName;
use omnifs_workspace::mounts::Registry;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use crate::credential_target::CredentialTarget;
use crate::session::MountConfig;
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;

#[derive(Args, Debug, Clone)]
pub struct MountsArgs {
    #[command(subcommand)]
    pub command: MountsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MountsCommand {
    /// Add a mount interactively (same as `omnifs init`).
    Add(crate::commands::init::InitArgs),
    /// List configured mounts with their provider and auth state.
    Ls,
    /// Remove a mount config (and its stored credential, by default).
    Rm {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
        /// Skip the credential delete.
        #[arg(long)]
        keep_credentials: bool,
    },
}

impl MountsArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            MountsCommand::Add(args) => args.run().await,
            MountsCommand::Ls => ls(),
            MountsCommand::Rm {
                name,
                force,
                keep_credentials,
            } => {
                let workspace = Workspace::resolve()?;
                rm(&workspace, &name, force, keep_credentials).await
            },
        }
    }
}

fn ls() -> anyhow::Result<()> {
    let workspace = Workspace::resolve()?;
    let layout = workspace.layout();
    let mounts = workspace.mounts()?;
    if mounts.is_empty() {
        anstream::println!(
            "No mounts configured. Add one with `omnifs mounts add` (or `omnifs init`)."
        );
        return Ok(());
    }
    let store = FileStore::new(&layout.credentials_file);
    for mount in &mounts {
        let name = crate::style::bold(mount.name.as_str());
        let provider = mount.config.provider_name().to_string();
        let auth = crate::auth::mount_auth(workspace.catalog(), mount.config.clone())
            .readiness(&store)
            .terminal_row()
            .summary;
        anstream::println!("{name}  {}  {auth}", crate::style::dim(provider));
    }
    Ok(())
}

pub async fn rm(
    workspace: &Workspace,
    name: &str,
    force: bool,
    keep_credentials: bool,
) -> anyhow::Result<()> {
    let layout = workspace.layout();
    let mounts = workspace.mounts()?;
    let name =
        MountName::new(name.to_owned()).with_context(|| format!("invalid mount name `{name}`"))?;

    let mount = mounts.iter().find(|m| m.name == name).ok_or_else(|| {
        missing_mount_error(&layout.config_file, &mounts, name.as_str()).unwrap_err()
    })?;
    let config_path = mount.source.clone();
    let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(&layout.credentials_file));
    let service = CredentialService::new(Arc::clone(&store), OAuthClient::new()?);
    let credential_target = if keep_credentials {
        CredentialTarget::for_mount(&mount.config)
    } else {
        crate::auth::mount_auth(workspace.catalog(), mount.config.clone())
            .register_revocation(&service)?
    };

    if !force {
        confirm(
            name.as_str(),
            &config_path,
            &credential_target,
            keep_credentials,
        )?;
    }

    delete_credentials(
        &service,
        &credential_target,
        keep_credentials,
        name.as_str(),
    )
    .await?;

    let daemon_delete = match workspace
        .daemon()
        .delete_mount_if_ready(name.as_str())
        .await
    {
        Ok(report) => report,
        Err(error) => {
            anstream::eprintln!("Running daemon could not remove mount `{name}`: {error:#}");
            anstream::eprintln!("Falling back to a local mount config delete.");
            None
        },
    };
    if daemon_delete.is_none() {
        Registry::load(&layout.mounts_dir)?.remove(&name)?;
    }
    anstream::println!(
        "Removed mount `{name}` ({})",
        WorkspaceLayout::display(&config_path)
    );

    if let Some(report) = daemon_delete {
        if let Some(failure) = report.failure {
            anstream::eprintln!(
                "Mount config removed, but unloading it from the running daemon failed: {}",
                failure.reason
            );
        } else {
            anstream::println!("✓ Unloaded from the running daemon");
        }
    }
    Ok(())
}

fn confirm(
    name: &str,
    config_path: &Path,
    target: &CredentialTarget,
    keep_credentials: bool,
) -> anyhow::Result<()> {
    anstream::println!("Remove mount `{name}`? This will:");
    anstream::println!("  • delete {}", WorkspaceLayout::display(config_path));
    match target {
        CredentialTarget::Internal(_) if !keep_credentials => {
            for key in target.keys() {
                anstream::println!("  • delete the stored credential `{}`", key.storage_key());
            }
        },
        CredentialTarget::Internal(_) => {
            anstream::println!("  • keep the stored credential (--keep-credentials)");
        },
        CredentialTarget::None => {},
    }
    anstream::print!("Continue? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_lowercase();
    if !matches!(answer.as_str(), "y" | "yes") {
        bail!("aborted");
    }
    Ok(())
}

pub(crate) async fn delete_credentials(
    service: &CredentialService,
    target: &CredentialTarget,
    keep_credentials: bool,
    name: &str,
) -> anyhow::Result<()> {
    if keep_credentials {
        return Ok(());
    }
    // Credential delete happens before the mount-file delete so that on
    // store failure we don't orphan the mount config.
    for key in target.keys() {
        let outcome = service.revoke_and_delete(key).await;
        if let Some(error) = outcome.delete_error() {
            bail!("delete credential for mount `{name}`: {error}");
        }
        anstream::println!("Credential `{}`: {outcome}", key.storage_key());
    }
    Ok(())
}

fn missing_mount_error(
    config_file: &Path,
    mounts: &[MountConfig],
    name: &str,
) -> anyhow::Result<()> {
    let suggestion = crate::mount_report::closest_mount_name(mounts, name);
    let mut message = format!(
        "no mount config named `{name}` in {}",
        WorkspaceLayout::display(config_file)
    );
    if let Some(suggestion) = suggestion {
        let _ = std::fmt::Write::write_fmt(
            &mut message,
            format_args!("\nDid you mean `{suggestion}`?"),
        );
    }
    bail!(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture_paths as base_fixture_paths;
    use omnifs_workspace::authn::CredentialId;
    use omnifs_workspace::creds::{CredentialEntry, MemoryStore};
    use secrecy::SecretString;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    fn fixture_paths(root: &Path) -> WorkspaceLayout {
        let paths = base_fixture_paths(root);
        std::fs::create_dir_all(&paths.mounts_dir).unwrap();
        paths
    }

    #[tokio::test]
    async fn rejects_invalid_mount_name() {
        let tmp = TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        let workspace = Workspace::from_layout(paths);
        let err = rm(&workspace, "../leak", true, false).await.unwrap_err();
        assert!(format!("{err:#}").contains("invalid mount name"));
    }
    #[tokio::test]
    async fn delete_credentials_deletes_internal_key() {
        let store = Arc::new(MemoryStore::new());
        let key = CredentialId::new("github", "device", "default").unwrap();
        let entry = CredentialEntry::static_token(
            SecretString::from("secret".to_owned()),
            OffsetDateTime::now_utc(),
        );
        store.put(&key, &entry).unwrap();

        let target = CredentialTarget::Internal(key.clone());
        let service = CredentialService::new(store.clone(), OAuthClient::new().unwrap());
        delete_credentials(&service, &target, false, "github")
            .await
            .unwrap();

        assert!(store.get(&key).unwrap().is_none());
    }
}
