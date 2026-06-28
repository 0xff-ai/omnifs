//! Runtime launch constants and mount payload preparation.
//!
//! The host `OMNIFS_HOME` directory is mounted writable into the trusted
//! runtime container. Credentials stay in the resolved `credentials.json`
//! store; mount payload preparation validates that host-managed credentials
//! exist but does not copy or rewrite them into per-session files.

use anyhow::{Context, anyhow};
use omnifs_core::MountName;
use omnifs_creds::CredentialStore;
use omnifs_mount::mounts::Spec;
use std::path::PathBuf;

use crate::{auth::MountAuth, error::WithHint};

pub(crate) const CONTAINER_NAME: &str = "omnifs";
pub(crate) const IMAGE: &str = concat!("ghcr.io/0xff-ai/omnifs:", env!("CARGO_PKG_VERSION"));
pub(crate) const ENV_IMAGE: &str = "OMNIFS_IMAGE";
pub(crate) const ENV_CONTAINER_NAME: &str = "OMNIFS_CONTAINER_NAME";

pub(crate) const GUEST_FUSE_MOUNT: &str = "/omnifs";
pub(crate) const OMNIFS_HOME: &str = "/root/.omnifs";

#[derive(Debug, Clone)]
pub(crate) struct MountConfig {
    pub(crate) name: MountName,
    pub(crate) config: Spec,
    /// Source file (informational; used for error messages).
    pub(crate) source: PathBuf,
}

impl MountConfig {
    pub(crate) fn from_parsed(config: Spec, source: PathBuf) -> anyhow::Result<Self> {
        let name = MountName::new(config.mount.clone()).with_context(|| {
            format!(
                "invalid mount name `{}` in {}",
                config.mount,
                source.display()
            )
        })?;
        Ok(Self {
            name,
            config,
            source,
        })
    }

    pub(crate) fn validate_host_managed_credentials(
        &self,
        mount_auth: &MountAuth,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<()> {
        for auth in &mount_auth.spec().auth {
            if auth.token_file().is_some() || auth.token_env().is_some() {
                continue;
            }
            let target = mount_auth
                .configured_target(auth, auth.account())
                .with_context(|| format!("resolve credential for mount `{}`", self.name))?;
            let key = target
                .primary_key()
                .expect("credential target for scheme is internal");
            let key_name = key.storage_key();
            let entry = target
                .lookup(store)
                .with_context(|| {
                    format!("fetch credential `{key_name}` for mount `{}`", self.name)
                })?
                .ok_or_else(|| {
                    anyhow!(
                        "no stored credential for `{key_name}` (mount `{}`)",
                        self.name
                    )
                });
            match (auth.is_oauth(), entry) {
                (_, Ok(_)) => {},
                (true, Err(error)) => {
                    return Err(error).with_hint(format!(
                        "Run `omnifs auth login {}` to authenticate",
                        self.name
                    ));
                },
                (false, Err(error)) => {
                    return Err(error).with_hint(format!(
                        "Run `omnifs auth import {}` or `omnifs init {}` to configure this mount's token",
                        self.name, self.name
                    ));
                },
            }
        }
        Ok(())
    }
}

pub(crate) fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::CredentialId;
    use omnifs_creds::{CredentialEntry, CredentialStore, MemoryStore};
    use secrecy::SecretString;
    use time::OffsetDateTime;

    use crate::launch::DockerMountMaterializer;
    use crate::test_support::{install_fixture_provider, spec_with_provider, spec_with_reference};
    use omnifs_provider::Catalog;

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
        let paths = omnifs_home::WorkspaceLayout::under_root(root);
        Catalog::open(paths.providers_dir)
    }

    #[test]
    fn materialize_validates_host_managed_static_token() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_home::WorkspaceLayout::under_root(tmp.path());
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
        // Validation accepts the present host-managed token; no preopens, so no
        // container binds.
        let mount = DockerMountMaterializer::new(&catalog, &store)
            .materialize(&config)
            .unwrap();
        assert!(mount.preopen_binds().is_empty());
    }

    #[test]
    fn materialize_passes_through_token_env_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("dns").unwrap(),
            config: spec_with_provider(
                "dns",
                r#"{"mount":"dns","auth":{"type":"static-token","scheme":"pat","token_env":"FOO"}}"#,
            ),
            source: PathBuf::from("/dev/null"),
        };
        let catalog = test_catalog(tmp.path());
        // A token_env credential is host-unmanaged, so validation requires no
        // stored credential.
        DockerMountMaterializer::new(&catalog, &store)
            .materialize(&config)
            .unwrap();
    }

    #[test]
    fn materialize_oauth_mount_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_home::WorkspaceLayout::under_root(tmp.path());
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
                r#"{ "mount": "github", "auth": {"type":"oauth","scheme":"device","clientId":"client-id"} }"#,
            ),
            source: PathBuf::from("/dev/null"),
        };
        DockerMountMaterializer::new(&catalog, &store)
            .materialize(&with_scheme)
            .unwrap();

        let metadata_only = MountConfig {
            name: MountName::try_from("github").unwrap(),
            config: spec_with_reference(&reference, r#"{ "mount": "github" }"#),
            source: PathBuf::from("/dev/null"),
        };
        DockerMountMaterializer::new(&catalog, &store)
            .materialize(&metadata_only)
            .unwrap();
    }

    #[test]
    fn materialize_rewrites_preopens_to_container_bind_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("chinook.sqlite"), "").unwrap();

        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("db").unwrap(),
            config: spec_with_provider(
                "db",
                &format!(
                    r#"{{
                    "mount": "db",
                    "config": {{"path": "/data/chinook.sqlite"}},
                    "capabilities": {{
                        "preopened_paths": [
                            {{"host": "{}", "guest": "/data", "mode": "ro"}}
                        ]
                    }}
                }}"#,
                    db_dir.display()
                ),
            ),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        let binds = DockerMountMaterializer::new(&catalog, &store)
            .materialize(&config)
            .unwrap()
            .into_preopen_binds()
            .into_docker_bind_specs();

        assert_eq!(
            binds,
            vec![format!(
                "{}:{}/db/0:ro",
                db_dir.canonicalize().unwrap().display(),
                omnifs_mount::materialize::GUEST_PREOPENS_DIR,
            )],
            "the CLI formats the container preopen bind for docker create"
        );
    }

    #[test]
    fn materialize_leaves_manifest_preopens_container_native() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("db").unwrap(),
            config: spec_with_provider(
                "db",
                r#"{
                    "mount": "db",
                    "config": {"path": "/data/test.db"},
                    "capabilities": {
                        "preopened_paths": [
                            {"host": "/data", "guest": "/data", "mode": "ro"}
                        ]
                    }
                }"#,
            ),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        let mount = DockerMountMaterializer::new(&catalog, &store)
            .materialize(&config)
            .unwrap();
        let spec = mount.spec();
        let preopen = &spec
            .capabilities
            .as_ref()
            .unwrap()
            .preopened_paths
            .as_ref()
            .and_then(|grant| match grant {
                omnifs_caps::Grant::Literal(paths) => Some(paths),
                omnifs_caps::Grant::Dynamic(_) => None,
            })
            .unwrap()[0];
        assert_eq!(
            (preopen.host.as_str(), preopen.guest.as_str()),
            ("/data", "/data"),
            "a container-native preopen (host == guest) passes through unrewritten"
        );
        assert!(
            mount.preopen_binds().is_empty(),
            "container-native preopens are fixture-provided, so no host bind is emitted"
        );
    }

    #[test]
    fn materialize_errors_when_credential_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_home::WorkspaceLayout::under_root(tmp.path());
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
        let err = DockerMountMaterializer::new(&catalog, &store)
            .materialize(&config)
            .unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("no stored credential"),
            "expected a missing-credential error, got: {chain}"
        );
    }
}
