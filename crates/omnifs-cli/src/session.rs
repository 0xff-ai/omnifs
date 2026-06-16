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
use omnifs_provider::PreopenMode;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{auth::MountAuth, catalog::ProviderCatalog, error::WithHint};

pub(crate) const CONTAINER_NAME: &str = "omnifs";
pub(crate) const IMAGE: &str = concat!("ghcr.io/0xff-ai/omnifs:", env!("CARGO_PKG_VERSION"));
pub(crate) const ENV_IMAGE: &str = "OMNIFS_IMAGE";
pub(crate) const ENV_CONTAINER_NAME: &str = "OMNIFS_CONTAINER_NAME";

pub(crate) const GUEST_FUSE_MOUNT: &str = "/omnifs";
pub(crate) const GUEST_PREOPENS_DIR: &str = "/run/omnifs/preopens";
pub(crate) const OMNIFS_HOME: &str = "/root/.omnifs";

/// One mount ready for `POST /v1/mounts`.
#[derive(Debug, Clone)]
pub(crate) struct MountPayload {
    pub(crate) name: MountName,
    pub(crate) spec: Spec,
}

#[derive(Debug, Clone)]
pub(crate) struct MountConfig {
    pub(crate) name: MountName,
    pub(crate) config: Spec,
    /// Source file (informational; used for error messages).
    pub(crate) source: PathBuf,
}

impl MountConfig {
    pub(crate) fn from_path(path: &Path) -> anyhow::Result<Self> {
        let config = Spec::from_file(path)
            .with_context(|| format!("load mount config {}", path.display()))?;
        Self::from_parsed(config, path.to_path_buf())
    }

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

    pub(crate) fn materialize(
        &self,
        catalog: &ProviderCatalog,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<(Vec<String>, MountPayload)> {
        let mut instance = self.config.clone();
        let user_preopen_count = instance
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.preopened_paths.as_ref())
            .map_or(0, Vec::len);
        catalog
            .apply_metadata(&mut instance)
            .with_context(|| format!("apply provider metadata for {}", self.source.display()))?;
        let resolved = catalog
            .resolve_mount_spec(instance.clone(), false)
            .with_context(|| format!("resolve mount config for {}", self.source.display()))?;
        let mount_auth = catalog.resolve_mount_auth_tolerating_manifest_errors(resolved);
        self.validate_host_managed_credentials(&mount_auth, store)?;
        instance
            .materialize_runtime_capabilities()
            .with_context(|| {
                format!(
                    "materialize runtime capabilities for {}",
                    self.source.display()
                )
            })?;
        let preopen_binds = self.materialize_preopened_paths(&mut instance, user_preopen_count)?;

        Ok((
            preopen_binds,
            MountPayload {
                name: self.name.clone(),
                spec: instance,
            },
        ))
    }

    fn validate_host_managed_credentials(
        &self,
        mount_auth: &MountAuth,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<()> {
        for auth in &mount_auth.config().spec.auth {
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

    fn materialize_preopened_paths(
        &self,
        instance: &mut Spec,
        user_preopen_count: usize,
    ) -> anyhow::Result<Vec<String>> {
        if user_preopen_count == 0 {
            return Ok(Vec::new());
        }
        let Some(preopens) = instance
            .capabilities
            .as_mut()
            .and_then(|capabilities| capabilities.preopened_paths.as_mut())
        else {
            return Ok(Vec::new());
        };

        preopens
            .iter_mut()
            .take(user_preopen_count)
            .enumerate()
            .map(|(index, preopen)| {
                let host_path = Path::new(&preopen.host)
                    .canonicalize()
                    .with_context(|| format!("canonicalize preopen {}", preopen.host))?;
                if !host_path.is_dir() {
                    anyhow::bail!("preopen {} is not a directory", host_path.display());
                }

                let container_path = format!("{GUEST_PREOPENS_DIR}/{}/{index}", self.name);
                let bind_mode = match preopen.mode {
                    PreopenMode::Ro => "ro",
                    PreopenMode::Rw => "rw",
                };
                preopen.host.clone_from(&container_path);
                Ok(format!(
                    "{}:{container_path}:{bind_mode}",
                    host_path.display()
                ))
            })
            .collect()
    }
}

pub(crate) fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub(crate) fn set_private_dir(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 700 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::CredentialId;
    use omnifs_creds::{CredentialEntry, CredentialStore, MemoryStore};
    use omnifs_mount::mounts::Spec;
    use secrecy::SecretString;
    use serde_json::Value;
    use time::OffsetDateTime;

    use crate::test_support::wasm_with_provider_metadata;

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

    fn test_catalog(root: &Path) -> ProviderCatalog {
        let paths = omnifs_home::Paths::under_root(root);
        ProviderCatalog::for_dirs(paths.mounts_dir, paths.providers_dir)
    }

    fn payload_json(payload: &MountPayload) -> Value {
        serde_json::to_value(&payload.spec).expect("serialize payload spec")
    }

    #[test]
    fn materialize_validates_host_managed_static_token_without_rewriting_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_home::Paths::under_root(tmp.path());
        std::fs::create_dir_all(&paths.providers_dir).unwrap();
        std::fs::write(
            paths.providers_dir.join("omnifs_provider_github.wasm"),
            wasm_with_provider_metadata("github", "omnifs_provider_github.wasm"),
        )
        .unwrap();

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "pat", "default").unwrap();
        store.put(&key, &sample_entry("sk-12345")).unwrap();

        let config = MountConfig {
            name: MountName::try_from("github").unwrap(),
            config: Spec::parse(
                r#"{
                    "provider": "omnifs_provider_github.wasm",
                    "mount": "github",
                    "auth": {"type":"static-token","scheme":"pat"}
                }"#,
            )
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        let (_, payload) = config.materialize(&catalog, &store).unwrap();
        let written = payload_json(&payload);

        assert_eq!(written["auth"][0]["scheme"], "pat");
        assert!(written["auth"][0].get("token_file").is_none());
        assert!(written["auth"][0].get("token_env").is_none());
    }

    #[test]
    fn from_path_rejects_invalid_mount_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.json");
        fs::write(
            &path,
            r#"{"provider":"p.wasm","mount":"../../../tmp/poison"}"#,
        )
        .unwrap();

        let err = MountConfig::from_path(&path).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("invalid mount name"),
            "expected invalid mount name, got: {chain}"
        );
    }

    #[test]
    fn materialize_passes_through_token_env_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("dns").unwrap(),
            config: Spec::parse(
                r#"{"provider":"p.wasm","mount":"dns","auth":{"type":"static-token","scheme":"pat","token_env":"FOO"}}"#,
            )
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };
        let catalog = test_catalog(tmp.path());
        let (_, payload) = config.materialize(&catalog, &store).unwrap();
        let written = payload_json(&payload);
        assert_eq!(written["auth"][0]["token_env"], "FOO");
    }

    #[test]
    fn materialize_validates_oauth_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_home::Paths::under_root(tmp.path());
        std::fs::create_dir_all(&paths.providers_dir).unwrap();

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store.put(&key, &sample_oauth_entry("gho-access")).unwrap();

        let config = MountConfig {
            name: MountName::try_from("github").unwrap(),
            config: Spec::parse(
                r#"{
                    "provider": "omnifs_provider_github.wasm",
                    "mount": "github",
                    "auth": {"type":"oauth","scheme":"device","clientId":"client-id"}
                }"#,
            )
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };

        std::fs::write(
            paths.providers_dir.join("omnifs_provider_github.wasm"),
            wasm_with_provider_metadata("github", "omnifs_provider_github.wasm"),
        )
        .unwrap();

        let catalog = test_catalog(tmp.path());
        let (_, payload) = config.materialize(&catalog, &store).unwrap();
        let written = payload_json(&payload);
        assert_eq!(written["auth"][0]["scheme"], "device");
    }

    #[test]
    fn materialize_applies_provider_metadata_before_credential_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_home::Paths::under_root(tmp.path());
        std::fs::create_dir_all(&paths.providers_dir).unwrap();
        std::fs::write(
            paths.providers_dir.join("omnifs_provider_github.wasm"),
            wasm_with_provider_metadata("github", "omnifs_provider_github.wasm"),
        )
        .unwrap();

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store.put(&key, &sample_oauth_entry("gho-access")).unwrap();

        let config = MountConfig {
            name: MountName::try_from("github").unwrap(),
            config: Spec::parse(
                r#"{
                    "provider": "omnifs_provider_github.wasm",
                    "mount": "github"
                }"#,
            )
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        config.materialize(&catalog, &store).unwrap();
    }

    #[test]
    fn materialize_uses_builtin_metadata_without_host_wasm() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store.put(&key, &sample_oauth_entry("gho-access")).unwrap();

        let config = MountConfig {
            name: MountName::try_from("github").unwrap(),
            config: Spec::parse(
                r#"{
                    "provider": "omnifs_provider_github.wasm",
                    "mount": "github"
                }"#,
            )
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        config.materialize(&catalog, &store).unwrap();
    }

    #[test]
    fn materialize_configured_docker_socket_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("docker").unwrap(),
            config: Spec::parse(
                r#"{
                    "provider": "omnifs_provider_docker.wasm",
                    "mount": "docker",
                    "config": {"endpoint": "unix:///var/run/docker.sock"}
                }"#,
            )
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        let (_, payload) = config.materialize(&catalog, &store).unwrap();
        let written = payload_json(&payload);
        assert_eq!(
            written["capabilities"]["unix_sockets"],
            serde_json::json!(["/var/run/docker.sock"]),
        );
    }

    #[test]
    fn materialize_rewrites_preopens_to_container_bind_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        fs::create_dir_all(&db_dir).unwrap();
        fs::write(db_dir.join("chinook.sqlite"), "").unwrap();

        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("db").unwrap(),
            config: Spec::parse(&format!(
                r#"{{
                    "provider": "omnifs_provider_db.wasm",
                    "mount": "db",
                    "config": {{"database_type": "sqlite", "path": "/data/chinook.sqlite"}},
                    "capabilities": {{
                        "preopened_paths": [
                            {{"host": "{}", "guest": "/data", "mode": "ro"}}
                        ]
                    }}
                }}"#,
                db_dir.display()
            ))
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        let (binds, payload) = config.materialize(&catalog, &store).unwrap();

        assert_eq!(
            binds,
            vec![format!(
                "{}:{GUEST_PREOPENS_DIR}/db/0:ro",
                db_dir.canonicalize().unwrap().display()
            )],
        );
        let written = payload_json(&payload);
        assert_eq!(
            written["capabilities"]["preopened_paths"][0]["host"],
            format!("{GUEST_PREOPENS_DIR}/db/0"),
        );
        assert_eq!(
            written["capabilities"]["preopened_paths"][0]["guest"],
            "/data",
        );
        assert_eq!(
            written["config"]["path"], "/data/chinook.sqlite",
            "provider config should keep the guest path selected by init"
        );
    }

    #[test]
    fn materialize_leaves_manifest_preopens_container_native() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("db").unwrap(),
            config: Spec::parse(
                r#"{
                    "provider": "omnifs_provider_db.wasm",
                    "mount": "db",
                    "config": {"database_type": "sqlite", "path": "/data/test.db"}
                }"#,
            )
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };

        let catalog = test_catalog(tmp.path());
        let (binds, payload) = config.materialize(&catalog, &store).unwrap();

        assert!(
            binds.is_empty(),
            "manifest preopens are already container paths"
        );
        let written = payload_json(&payload);
        assert_eq!(
            written["capabilities"]["preopened_paths"][0]["host"],
            "/data"
        );
        assert_eq!(
            written["capabilities"]["preopened_paths"][0]["guest"],
            "/data",
        );
    }

    #[test]
    fn materialize_errors_when_credential_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_home::Paths::under_root(tmp.path());
        std::fs::create_dir_all(&paths.providers_dir).unwrap();
        std::fs::write(
            paths.providers_dir.join("omnifs_provider_github.wasm"),
            wasm_with_provider_metadata("github", "omnifs_provider_github.wasm"),
        )
        .unwrap();

        let store = MemoryStore::new();
        let config = MountConfig {
            name: MountName::try_from("ghost").unwrap(),
            config: Spec::parse(
                r#"{"provider":"omnifs_provider_github.wasm","mount":"ghost","auth":{"type":"static-token","scheme":"pat"}}"#,
            )
            .unwrap(),
            source: PathBuf::from("/dev/null"),
        };
        let catalog = test_catalog(tmp.path());
        let err = config.materialize(&catalog, &store).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("no stored credential"),
            "expected a missing-credential error, got: {chain}"
        );
    }
}
