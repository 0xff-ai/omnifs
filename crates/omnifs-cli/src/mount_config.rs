//! Mount payload preparation.
//!
//! The host `OMNIFS_HOME` directory is mounted writable into the trusted
//! runtime container. Credentials stay in the resolved `credentials.json`
//! store; mount payload preparation validates that host-managed credentials
//! exist but does not copy or rewrite them into per-session files.

use anyhow::{Context, anyhow};
use omnifs_workspace::Workspace;
use omnifs_workspace::creds::CredentialStore;
use omnifs_workspace::mounts::Registry;

use crate::{
    auth::MountAuth,
    error::{ExitCode, WithExitCode, WithHint},
};

pub(crate) fn validate_host_managed_credentials(
    mount_auth: &MountAuth,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let Some(auth) = &mount_auth.spec().auth else {
        return Ok(());
    };
    let name = &mount_auth.spec().mount;
    let target = mount_auth
        .configured_credential_id(auth)
        .with_context(|| format!("resolve credential for mount `{name}`"))?;
    let key_name = target.storage_key();
    let entry = store
        .get(&target)
        .with_context(|| format!("fetch credential `{key_name}` for mount `{name}`"))?
        .ok_or_else(|| anyhow!("no stored credential for `{key_name}` (mount `{name}`)"));
    match (auth.is_oauth(), entry) {
        (_, Ok(_)) => Ok(()),
        (true, Err(error)) => Err(error)
            .with_hint(format!("Run `omnifs mount reauth {name}` to authenticate"))
            .with_exit_code(ExitCode::AuthRequired),
        (false, Err(error)) => Err(error)
            .with_hint(format!(
                "Run `omnifs mount reauth {name}` to configure this mount's token"
            ))
            .with_exit_code(ExitCode::AuthRequired),
    }
}

/// Load desired mounts strictly: one malformed spec invalidates commands that
/// need a complete registry view.
pub(crate) fn load_registry(workspace: &Workspace) -> anyhow::Result<Registry> {
    let registry = workspace.desired_state().registry()?;
    if let Some(failure) = registry.failures().first() {
        anyhow::bail!("{}", failure.error);
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::authn::CredentialId;
    use omnifs_workspace::creds::{CredentialEntry, CredentialStore, MemoryStore};
    use omnifs_workspace::mounts::Spec;
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
        Catalog::open(root.join("providers"))
    }

    /// Validate `config`'s host-managed credential. Authority belongs to daemon
    /// startup and is deliberately absent from this pre-stop preflight.
    fn preflight_and_validate(
        config: &Spec,
        catalog: &Catalog,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<()> {
        let mount_auth = MountAuth::from_spec(catalog, config.clone());
        validate_host_managed_credentials(&mount_auth, store)
    }

    #[test]
    fn preflight_validates_host_managed_static_token() {
        let tmp = tempfile::tempdir().unwrap();
        let providers_dir = tmp.path().join("providers");
        std::fs::create_dir_all(&providers_dir).unwrap();
        let reference = install_fixture_provider(&providers_dir, "github");

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "pat", "default").unwrap();
        store.put(&key, &sample_entry("sk-12345")).unwrap();

        let config = spec_with_reference(
            &reference,
            r#"{ "mount": "github", "auth": {"type":"static-token","scheme":"pat"} }"#,
        );

        let catalog = test_catalog(tmp.path());
        preflight_and_validate(&config, &catalog, &store).unwrap();
    }

    #[test]
    fn preflight_validates_oauth_mount_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let providers_dir = tmp.path().join("providers");
        std::fs::create_dir_all(&providers_dir).unwrap();
        let reference = install_fixture_provider(&providers_dir, "github");

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store.put(&key, &sample_oauth_entry("gho-access")).unwrap();

        let catalog = test_catalog(tmp.path());

        let with_scheme = spec_with_reference(
            &reference,
            r#"{ "mount": "github", "auth": {"type":"oauth","scheme":"device","client_id":"client-id"} }"#,
        );
        preflight_and_validate(&with_scheme, &catalog, &store).unwrap();

        let metadata_only = spec_with_reference(&reference, r#"{ "mount": "github" }"#);
        preflight_and_validate(&metadata_only, &catalog, &store).unwrap();
    }

    #[test]
    fn preflight_errors_when_credential_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let providers_dir = tmp.path().join("providers");
        std::fs::create_dir_all(&providers_dir).unwrap();
        let reference = install_fixture_provider(&providers_dir, "github");

        let store = MemoryStore::new();
        let config = spec_with_reference(
            &reference,
            r#"{ "mount": "ghost", "auth": {"type":"static-token","scheme":"pat"} }"#,
        );
        let catalog = test_catalog(tmp.path());
        let err = preflight_and_validate(&config, &catalog, &store).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("no stored credential"),
            "expected a missing-credential error, got: {chain}"
        );
    }
}
