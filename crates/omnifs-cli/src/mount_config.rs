//! Mount payload preparation.
//!
//! The host `OMNIFS_HOME` directory is mounted writable into the trusted
//! runtime container. Credentials stay in the resolved `credentials.json`
//! store; mount payload preparation validates that host-managed credentials
//! exist but does not copy or rewrite them into per-session files.

use anyhow::{Context, anyhow};
use omnifs_workspace::Workspace;
use omnifs_workspace::creds::CredentialStore;
use omnifs_workspace::mounts::Name as MountName;
use omnifs_workspace::mounts::Spec;
use std::path::PathBuf;

use crate::{
    auth::MountAuth,
    error::{ExitCode, WithExitCode, WithHint},
};

#[derive(Debug, Clone)]
pub(crate) struct MountConfig {
    pub(crate) name: MountName,
    pub(crate) config: Spec,
    /// Source file (informational; used for error messages).
    pub(crate) source: PathBuf,
}

impl MountConfig {
    pub(crate) fn validate_host_managed_credentials(
        &self,
        mount_auth: &MountAuth,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<()> {
        let Some(auth) = &mount_auth.spec().auth else {
            return Ok(());
        };
        let target = mount_auth
            .configured_target(auth, auth.account())
            .with_context(|| format!("resolve credential for mount `{}`", self.name))?;
        let key = target
            .primary_key()
            .expect("credential target for scheme is internal");
        let key_name = key.storage_key();
        let entry = target
            .lookup(store)
            .with_context(|| format!("fetch credential `{key_name}` for mount `{}`", self.name))?
            .ok_or_else(|| {
                anyhow!(
                    "no stored credential for `{key_name}` (mount `{}`)",
                    self.name
                )
            });
        match (auth.is_oauth(), entry) {
            (_, Ok(_)) => Ok(()),
            (true, Err(error)) => Err(error)
                .with_hint(format!(
                    "Run `omnifs mount reauth {}` to authenticate",
                    self.name
                ))
                .with_exit_code(ExitCode::AuthRequired),
            (false, Err(error)) => Err(error)
                .with_hint(format!(
                    "Run `omnifs mount reauth {}` to configure this mount's token",
                    self.name
                ))
                .with_exit_code(ExitCode::AuthRequired),
        }
    }
}

/// Convert the workspace-owned registry into the CLI payload only at the
/// command boundary that needs it.
pub(crate) fn load_mounts(workspace: &Workspace) -> anyhow::Result<Vec<MountConfig>> {
    let registry = workspace.desired_state().registry()?;
    if let Some(failure) = registry.failures().first() {
        anyhow::bail!("{}", failure.error);
    }
    Ok(registry
        .iter()
        .map(|(name, spec)| MountConfig {
            name: name.clone(),
            config: spec.clone(),
            source: registry.spec_path(name),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::authn::CredentialId;
    use omnifs_workspace::creds::{CredentialEntry, CredentialStore, MemoryStore};
    use secrecy::SecretString;
    use time::OffsetDateTime;

    use crate::test_support::{install_fixture_provider, spec_with_reference};
    use omnifs_workspace::provider::Catalog;

    fn sample_entry(value: &str) -> CredentialEntry {
        CredentialEntry::static_token(
            SecretString::from(value.to_string()),
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    fn sample_oauth_entry(value: &str) -> CredentialEntry {
        CredentialEntry::oauth(
            SecretString::from(value.to_string()),
            None,
            None,
            "bearer".to_owned(),
            vec![],
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    fn test_catalog(root: &std::path::Path) -> Catalog {
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(root);
        Catalog::open(paths.providers_dir)
    }

    /// Validate `config`'s host-managed credential. Authority belongs to daemon
    /// startup and is deliberately absent from this pre-stop preflight.
    fn preflight_and_validate(
        config: &MountConfig,
        catalog: &Catalog,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<()> {
        let mount_auth = MountAuth::from_spec(catalog, config.config.clone());
        config.validate_host_managed_credentials(&mount_auth, store)
    }

    #[test]
    fn preflight_validates_host_managed_static_token() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(tmp.path());
        std::fs::create_dir_all(&paths.providers_dir).unwrap();
        let reference = install_fixture_provider(&paths.providers_dir, "github");

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "pat", "default").unwrap();
        store.put(&key, &sample_entry("sk-12345")).unwrap();

        let config = MountConfig {
            name: MountName::try_from("github").unwrap(),
            config: spec_with_reference(
                &reference,
                r#"{ "mount": "github", "auth": {"type":"static-token","scheme":"pat"} }"#,
            ),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        preflight_and_validate(&config, &catalog, &store).unwrap();
    }

    #[test]
    fn preflight_validates_oauth_mount_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(tmp.path());
        std::fs::create_dir_all(&paths.providers_dir).unwrap();
        let reference = install_fixture_provider(&paths.providers_dir, "github");

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store.put(&key, &sample_oauth_entry("gho-access")).unwrap();

        let catalog = test_catalog(tmp.path());

        let with_scheme = MountConfig {
            name: MountName::try_from("github").unwrap(),
            config: spec_with_reference(
                &reference,
                r#"{ "mount": "github", "auth": {"type":"oauth","scheme":"device","client_id":"client-id"} }"#,
            ),
            source: PathBuf::from("/dev/null"),
        };
        preflight_and_validate(&with_scheme, &catalog, &store).unwrap();

        let metadata_only = MountConfig {
            name: MountName::try_from("github").unwrap(),
            config: spec_with_reference(&reference, r#"{ "mount": "github" }"#),
            source: PathBuf::from("/dev/null"),
        };
        preflight_and_validate(&metadata_only, &catalog, &store).unwrap();
    }

    #[test]
    fn preflight_errors_when_credential_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(tmp.path());
        std::fs::create_dir_all(&paths.providers_dir).unwrap();
        let reference = install_fixture_provider(&paths.providers_dir, "github");

        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("ghost").unwrap(),
            config: spec_with_reference(
                &reference,
                r#"{ "mount": "ghost", "auth": {"type":"static-token","scheme":"pat"} }"#,
            ),
            source: PathBuf::from("/dev/null"),
        };
        let catalog = test_catalog(tmp.path());
        let err = preflight_and_validate(&config, &catalog, &store).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("no stored credential"),
            "expected a missing-credential error, got: {chain}"
        );
    }
}
