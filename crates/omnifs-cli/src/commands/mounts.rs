//! `omnifs mounts rm <name>` — remove a mount config and (by default) its
//! stored credential.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use omnifs_core::MountName;
use omnifs_creds::{CredentialStore, FileStore};
use std::io::Write as _;
use std::path::Path;

use crate::credential_target::CredentialTarget;
use crate::session::MountConfig;
use crate::workspace::Workspace;
use omnifs_home::WorkspaceLayout;

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
        match workspace
            .catalog()
            .resolve_mount_spec(mount.config.clone(), false)
        {
            Ok(resolved) => {
                let provider = resolved.provider_name.clone();
                let auth = crate::auth::AuthReadiness::from_config(&resolved, &store)
                    .terminal_row()
                    .summary;
                anstream::println!("{name}  {}  {auth}", crate::style::dim(provider));
            },
            Err(error) => anstream::println!("{name}  {}", crate::style::error(error)),
        }
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
    let catalog = workspace.catalog();
    let mounts = workspace.mounts()?;
    let name =
        MountName::new(name.to_owned()).with_context(|| format!("invalid mount name `{name}`"))?;

    let mount = mounts.iter().find(|m| m.name == name).ok_or_else(|| {
        missing_mount_error(&layout.config_file, &mounts, name.as_str()).unwrap_err()
    })?;
    let config_path = mount.source.clone();
    let config = mount.config.clone();
    let resolved = catalog
        .resolve_mount_spec(config, false)
        .with_context(|| format!("resolve mount config for `{name}`"))?;
    let credential_target = CredentialTarget::for_mount(&resolved);

    if !force {
        confirm(
            name.as_str(),
            &config_path,
            &credential_target,
            keep_credentials,
        )?;
    }

    let store = FileStore::new(&layout.credentials_file);
    delete_credentials(&store, &credential_target, keep_credentials, name.as_str())?;

    std::fs::remove_file(&config_path)
        .with_context(|| format!("remove {}", config_path.display()))?;
    anstream::println!(
        "Removed mount `{name}` ({})",
        WorkspaceLayout::display(&config_path)
    );

    match workspace.daemon().reconcile_if_running().await {
        Ok(Some(_)) => {
            anstream::println!("✓ Unloaded from the running daemon");
        },
        Ok(None) => {},
        Err(error) => {
            anstream::eprintln!(
                "Mount config removed, but unloading it from the running daemon failed: {error:#}"
            );
        },
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
        CredentialTarget::External(source) => {
            anstream::println!(
                "  • leave the externally-configured credential ({source}) unchanged"
            );
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

pub(crate) fn delete_credentials(
    store: &dyn CredentialStore,
    target: &CredentialTarget,
    keep_credentials: bool,
    name: &str,
) -> anyhow::Result<()> {
    if keep_credentials {
        return Ok(());
    }
    // Credential delete happens before the mount-file delete so that on
    // store failure we don't orphan the mount config.
    target.delete_from(store, name)
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
    use omnifs_core::CredentialId;
    use omnifs_creds::{CredentialEntry, MemoryStore};
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
    #[test]
    fn delete_credentials_deletes_internal_key() {
        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        let entry = CredentialEntry::static_token(
            SecretString::from("secret".to_owned()),
            OffsetDateTime::now_utc(),
        );
        store.put(&key, &entry).unwrap();

        let target = CredentialTarget::Internal(key.clone());
        delete_credentials(&store, &target, false, "github").unwrap();

        assert!(store.get(&key).unwrap().is_none());
    }
}
