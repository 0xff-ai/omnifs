//! `omnifs mounts rm <name>` — remove a mount config and (by default) its
//! stored credential.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use omnifs_core::MountName;
use omnifs_creds::CredentialStore;
use std::io::Write as _;
use std::path::Path;

use crate::app_context::AppContext;
use crate::catalog::ProviderCatalog;
use crate::config::ConfigFile;
use crate::credential_target::CredentialTarget;
use crate::paths::Paths;
use crate::session::CredsBackend;

#[derive(Args, Debug, Clone)]
pub struct MountsArgs {
    #[command(subcommand)]
    pub command: MountsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MountsCommand {
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
        let ctx = AppContext::resolve_default()?;
        let paths = ctx.paths();
        let catalog = ctx.catalog();
        match self.command {
            MountsCommand::Rm {
                name,
                force,
                keep_credentials,
            } => rm(paths, catalog, &name, force, keep_credentials).await,
        }
    }
}

pub async fn rm(
    paths: &Paths,
    catalog: &ProviderCatalog,
    name: &str,
    force: bool,
    keep_credentials: bool,
) -> anyhow::Result<()> {
    let name =
        MountName::new(name.to_owned()).with_context(|| format!("invalid mount name `{name}`"))?;

    let mount = catalog
        .session_mount_configs()?
        .into_iter()
        .find(|mount| mount.name == name)
        .ok_or_else(|| missing_mount_error(paths, catalog, name.as_str()).unwrap_err())?;
    let config_path = mount.source.clone();
    let inline = config_path == paths.config_file;
    let config = mount.config;
    let resolved = catalog
        .resolve_mount_spec(config, false)
        .with_context(|| format!("resolve mount config for `{name}`"))?;
    let credential_target = CredentialTarget::for_mount(&resolved);

    if !force {
        confirm(
            paths,
            name.as_str(),
            &config_path,
            &credential_target,
            keep_credentials,
        )?;
    }

    let store = CredsBackend::auto(&paths.credentials_file, false);
    delete_credentials(
        store.as_ref(),
        &credential_target,
        keep_credentials,
        name.as_str(),
    )?;

    if inline {
        let mut config = ConfigFile::load(&paths.config_file)?;
        config.remove_mount(name.as_str())?;
        config.save()?;
    } else {
        std::fs::remove_file(&config_path)
            .with_context(|| format!("remove {}", config_path.display()))?;
    }
    anstream::println!("Removed mount `{name}` ({})", Paths::display(&config_path));

    match crate::live::remove_mount(name.as_str()).await {
        Ok(crate::live::LiveApply::Applied) => {
            anstream::println!("✓ Unloaded from the running daemon");
        },
        Ok(_) => {},
        Err(error) => {
            anstream::eprintln!(
                "Mount config removed, but unloading it from the running daemon failed: {error:#}"
            );
        },
    }
    Ok(())
}

fn confirm(
    paths: &Paths,
    name: &str,
    config_path: &Path,
    target: &CredentialTarget,
    keep_credentials: bool,
) -> anyhow::Result<()> {
    anstream::println!("Remove mount `{name}`? This will:");
    anstream::println!("  • delete {}", Paths::display(config_path));
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
    let _ = paths;
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

fn missing_mount_error(paths: &Paths, catalog: &ProviderCatalog, name: &str) -> anyhow::Result<()> {
    let suggestion = catalog.closest_mount_name(name)?;
    let mut message = format!(
        "no mount config named `{name}` in {}",
        Paths::display(&paths.config_file)
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

    fn fixture_paths(root: &Path) -> Paths {
        let paths = base_fixture_paths(root);
        std::fs::create_dir_all(&paths.mounts_dir).unwrap();
        paths
    }

    fn catalog_for(paths: &Paths) -> ProviderCatalog {
        ProviderCatalog::new(&paths.mounts_dir, &paths.providers_dir)
    }

    #[tokio::test]
    async fn rejects_invalid_mount_name() {
        let tmp = TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        let catalog = catalog_for(&paths);
        let err = rm(&paths, &catalog, "../leak", true, false)
            .await
            .unwrap_err();
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
