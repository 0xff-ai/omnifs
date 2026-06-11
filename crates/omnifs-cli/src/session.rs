//! Per-run session materialization for the host-managed runtime container.
//!
//! The host CLI is the only component that touches the OS credential
//! store. On `up` we resolve host-managed static-token credentials into
//! per-session secret files, and copy configured OAuth credentials into a
//! per-session credential store that the container daemon can read.

use anyhow::{Context, anyhow};
use omnifs_core::MountName;
use omnifs_creds::{CredentialStore, FileStore, KeyringStore};
use omnifs_home::CREDENTIALS_FILE;
use omnifs_mount::mounts::Spec;
use omnifs_provider::PreopenMode;
use secrecy::ExposeSecret;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{
    auth::MountAuth, catalog::ProviderCatalog, container_name::ContainerName, error::WithHint,
};

pub(crate) const CONTAINER_NAME: &str = "omnifs";
pub(crate) const IMAGE: &str = concat!("ghcr.io/0xff-ai/omnifs:", env!("CARGO_PKG_VERSION"));
pub(crate) const HOST_CRED_DIR: &str = "/run/omnifs/creds";
pub(crate) const HOST_FUSE_MOUNT: &str = "/omnifs";
pub(crate) const HOST_PREOPENS_DIR: &str = "/run/omnifs/preopens";
pub(crate) const ENV_IMAGE: &str = "OMNIFS_IMAGE";
pub(crate) const ENV_CONTAINER_NAME: &str = "OMNIFS_CONTAINER_NAME";

pub(crate) struct Session {
    root: PathBuf,
    creds_dir: PathBuf,
    credentials_file: PathBuf,
}

impl Session {
    pub(crate) fn prepare(
        container_name: &ContainerName,
        credentials_file: &Path,
    ) -> anyhow::Result<Self> {
        let root = container_name.session_root();
        if root.exists() {
            let synced = sync_session_credentials_to_host(container_name, credentials_file)?;
            if synced > 0 {
                anstream::println!(
                    "{}",
                    crate::style::dim(format!(
                        "Imported {synced} stale credential(s) from previous session"
                    ))
                );
            }
            fs::remove_dir_all(&root)
                .with_context(|| format!("clear stale session dir {}", root.display()))?;
        }
        fs::create_dir_all(&root)
            .with_context(|| format!("create session dir {}", root.display()))?;
        let creds_dir = root.join("creds");
        let credentials_file = root.join(CREDENTIALS_FILE);
        fs::create_dir_all(&creds_dir)?;
        set_private_dir(&root)?;
        set_private_dir(&creds_dir)?;
        Ok(Self {
            root,
            creds_dir,
            credentials_file,
        })
    }

    /// Attach to the live session directory created by `omnifs up`,
    /// without clearing it. `None` when no session exists (daemon not
    /// started through this host, or never started).
    pub(crate) fn attach(container_name: &ContainerName) -> Option<Self> {
        let root = container_name.session_root();
        let creds_dir = root.join("creds");
        let credentials_file = root.join(CREDENTIALS_FILE);
        creds_dir.is_dir().then_some(Self {
            root,
            creds_dir,
            credentials_file,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn creds_dir(&self) -> &Path {
        &self.creds_dir
    }

    pub(crate) fn credentials_file(&self) -> &Path {
        &self.credentials_file
    }

    pub(crate) fn cleanup_on_drop(&self) -> SessionCleanup {
        SessionCleanup::armed(self)
    }

    /// Materialize credentials into the session and produce the rewritten
    /// mount spec payloads the CLI pushes to the daemon over the control
    /// API. Returns the extra container binds (user preopens) alongside.
    pub(crate) fn populate(
        &self,
        configs: &[MountConfig],
        catalog: &ProviderCatalog,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<(Vec<String>, Vec<MountPayload>)> {
        let materializer = SessionMaterializer {
            session: self,
            catalog,
            store,
        };
        let mut binds = Vec::new();
        let mut payloads = Vec::new();
        for cfg in configs {
            let (preopen_binds, payload) = materializer.materialize(cfg)?;
            binds.extend(preopen_binds);
            payloads.push(payload);
        }
        Ok((binds, payloads))
    }
}

/// One mount ready for `POST /v1/mounts`: the session-rewritten spec.
/// Secret values stay in session files; the spec carries only their paths.
#[derive(Debug, Clone)]
pub(crate) struct MountPayload {
    pub(crate) name: MountName,
    pub(crate) spec: Spec,
}

struct SessionMaterializer<'a> {
    session: &'a Session,
    catalog: &'a ProviderCatalog,
    store: &'a dyn CredentialStore,
}

impl SessionMaterializer<'_> {
    fn materialize(&self, cfg: &MountConfig) -> anyhow::Result<(Vec<String>, MountPayload)> {
        let mut instance = cfg.config.clone();
        let user_preopen_count = instance
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.preopened_paths.as_ref())
            .map_or(0, Vec::len);
        self.catalog
            .apply_metadata(&mut instance)
            .with_context(|| format!("apply provider metadata for {}", cfg.source.display()))?;
        let resolved = self
            .catalog
            .resolve_mount_spec(instance.clone(), false)
            .with_context(|| format!("resolve mount config for {}", cfg.source.display()))?;
        let mount_auth = self
            .catalog
            .resolve_mount_auth_tolerating_manifest_errors(resolved);
        self.materialize_oauth(&mount_auth, &cfg.name)?;
        self.materialize_host_managed_auth(&mount_auth, &cfg.name)?;
        instance
            .materialize_runtime_capabilities()
            .with_context(|| {
                format!(
                    "materialize runtime capabilities for {}",
                    cfg.source.display()
                )
            })?;
        let preopen_binds =
            Self::materialize_preopened_paths(&mut instance, &cfg.name, user_preopen_count)?;
        Self::patch_auth(&mut instance, &mount_auth)?;

        Ok((
            preopen_binds,
            MountPayload {
                name: cfg.name.clone(),
                spec: instance,
            },
        ))
    }

    fn materialize_preopened_paths(
        instance: &mut Spec,
        name: &MountName,
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

                let container_path = format!("{HOST_PREOPENS_DIR}/{name}/{index}");
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

    fn materialize_oauth(
        &self,
        mount_auth: &MountAuth,
        mount_name: &MountName,
    ) -> anyhow::Result<()> {
        for auth in mount_auth
            .config()
            .spec
            .auth
            .iter()
            .filter(|auth| auth.is_oauth())
        {
            let target = mount_auth
                .configured_target(auth, None)
                .with_context(|| format!("resolve OAuth credential for mount `{mount_name}`"))?;
            let key = target
                .primary_key()
                .expect("credential target for scheme is internal");
            let key_name = key.storage_key();
            let entry = target
                .lookup(self.store)
                .with_context(|| {
                    format!("fetch OAuth credential `{key_name}` for mount `{mount_name}`")
                })?
                .ok_or_else(|| {
                    anyhow!("no stored OAuth credential for `{key_name}` (mount `{mount_name}`)")
                })
                .with_hint(format!(
                    "Run `omnifs auth login {mount_name}` to authenticate"
                ))?;
            FileStore::new(self.session.credentials_file()).put(key, &entry)?;
        }
        Ok(())
    }

    fn materialize_host_managed_auth(
        &self,
        mount_auth: &MountAuth,
        mount_name: &MountName,
    ) -> anyhow::Result<()> {
        for auth in &mount_auth.config().spec.auth {
            if auth.is_oauth() {
                continue;
            }
            if auth.token_file().is_some() || auth.token_env().is_some() {
                continue;
            }
            let target = mount_auth
                .configured_target(auth, auth.account())
                .with_context(|| format!("resolve credential for mount `{mount_name}`"))?;
            let key = target
                .primary_key()
                .expect("credential target for scheme is internal");
            let key_name = key.storage_key();
            let entry = target
                .lookup(self.store)
                .with_context(|| format!("fetch credential `{key_name}` for mount `{mount_name}`"))?
                .ok_or_else(|| anyhow!("no stored credential for `{key_name}` (mount `{mount_name}`)"))
                .with_hint(format!(
                    "Run `omnifs auth import {mount_name}` or `omnifs init {mount_name}` to configure this mount's token"
                ))?;
            let cred_path = self.session.creds_dir().join(&key_name);
            write_secret(&cred_path, entry.access_token().expose_secret())?;
        }
        Ok(())
    }

    /// Rewrite host-managed static-token entries to point at their session
    /// secret files. The spec stays typed end-to-end; the daemon parses the
    /// same `Spec` this produces.
    fn patch_auth(instance: &mut Spec, mount_auth: &MountAuth) -> anyhow::Result<()> {
        for (entry, auth_config) in instance
            .auth
            .iter_mut()
            .zip(mount_auth.config().spec.auth.iter())
        {
            if auth_config.is_oauth()
                || auth_config.token_file().is_some()
                || auth_config.token_env().is_some()
                || auth_config.scheme().is_none()
            {
                continue;
            }
            let target = mount_auth
                .configured_target(auth_config, auth_config.account())
                .map_err(|error| anyhow!("invalid credential id: {error}"))?;
            let key = target
                .primary_key()
                .expect("credential target for scheme is internal");
            if let omnifs_mount::Auth::StaticToken(static_token) = entry {
                static_token.token_env = None;
                static_token.token_file = Some(format!("{HOST_CRED_DIR}/{}", key.storage_key()));
            }
        }
        Ok(())
    }
}

pub(crate) struct SessionCleanup {
    root: PathBuf,
    armed: bool,
}

impl SessionCleanup {
    fn armed(session: &Session) -> Self {
        Self {
            root: session.root.clone(),
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        if self.armed && self.root.exists() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

pub(crate) fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub(crate) fn clean_session_dir(container_name: &ContainerName) -> anyhow::Result<()> {
    let root = container_name.session_root();
    if root.exists() {
        fs::remove_dir_all(&root)
            .with_context(|| format!("remove session dir {}", root.display()))?;
        anstream::println!("✓ Session dir {} cleaned", root.display());
    }
    Ok(())
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
}

pub(crate) fn write_secret(path: &Path, secret: &str) -> anyhow::Result<()> {
    fs::write(path, secret).with_context(|| format!("write secret to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
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

/// Env var to opt into a non-default credential backend. Values: `file`,
/// `keychain`. Unset uses the file backend.
pub(crate) const ENV_CREDS_BACKEND: &str = "OMNIFS_CREDS_BACKEND";

/// Constructors for the durable credential store.
///
/// The default production backend is the JSON file at the resolved
/// `credentials.json` path. Avoiding the OS keychain keeps `omnifs up`,
/// `omnifs dev`, auth commands, and session sync from triggering platform
/// permission prompts when the binary path or signature changes. The keychain
/// backend remains available for explicit opt-in.
pub(crate) struct CredsBackend;

impl CredsBackend {
    /// Default production policy: use the resolved JSON credential file.
    pub(crate) fn auto(credentials_file: &Path, verbose: bool) -> Box<dyn CredentialStore> {
        match env_string(ENV_CREDS_BACKEND).as_deref() {
            Some("keychain") => Self::keychain(credentials_file, verbose),
            Some("file") | None => Self::file(credentials_file, verbose),
            Some(other) => {
                if verbose {
                    anstream::eprintln!(
                        "{ENV_CREDS_BACKEND}=`{other}` is not a recognized value (file|keychain); \
                         using file-backed credentials"
                    );
                }
                Self::file(credentials_file, verbose)
            },
        }
    }

    /// Use the JSON credential store.
    pub(crate) fn file(credentials_file: &Path, verbose: bool) -> Box<dyn CredentialStore> {
        if verbose {
            anstream::eprintln!(
                "Using file-backed credentials store at {}",
                credentials_file.display()
            );
        }
        Box::new(FileStore::new(credentials_file))
    }

    /// Use the OS keychain when explicitly requested. If the platform backend
    /// is unavailable, fall back to the JSON file instead of failing startup.
    pub(crate) fn keychain(credentials_file: &Path, verbose: bool) -> Box<dyn CredentialStore> {
        match KeyringStore::new() {
            Ok(store) => Box::new(store),
            Err(error) => {
                if verbose {
                    anstream::eprintln!(
                        "Keychain requested but unavailable ({error}); using file-backed credentials store at {}",
                        credentials_file.display()
                    );
                }
                Box::new(FileStore::new(credentials_file))
            },
        }
    }
}

pub(crate) fn sync_session_credentials_to_host(
    container_name: &ContainerName,
    credentials_file: &Path,
) -> anyhow::Result<usize> {
    let session_credentials = container_name.session_root().join(CREDENTIALS_FILE);
    if !session_credentials.exists() {
        return Ok(0);
    }
    let session_store = FileStore::new(&session_credentials);
    let host_store = CredsBackend::auto(credentials_file, true);
    let mut synced = 0;
    let keys = session_store.list().with_context(|| {
        format!(
            "list session credentials in {}",
            session_credentials.display()
        )
    })?;
    let Some(keys) = keys else {
        return Ok(0);
    };
    for key in keys {
        if let Some(entry) = session_store
            .get(&key)
            .with_context(|| format!("read session credential `{key}`"))?
        {
            host_store
                .put(&key, &entry)
                .with_context(|| format!("sync session credential `{key}` to host store"))?;
            synced += 1;
        }
    }
    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::CredentialId;
    use omnifs_creds::{CredentialEntry, MemoryStore};
    use omnifs_home::MOUNTS_SUBDIR;
    use omnifs_mount::mounts::Spec;
    use secrecy::{ExposeSecret, SecretString};
    use serde_json::Value;
    use time::OffsetDateTime;

    fn sample_entry(value: &str) -> CredentialEntry {
        CredentialEntry::static_token(
            SecretString::from(value.to_string()),
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    use crate::test_support::wasm_with_provider_metadata;

    fn test_catalog(providers_dir: &Path) -> ProviderCatalog {
        ProviderCatalog::for_dirs(providers_dir.join(MOUNTS_SUBDIR), providers_dir)
    }

    fn payload_for(payloads: &[MountPayload], name: &str) -> Value {
        serde_json::to_value(
            &payloads
                .iter()
                .find(|payload| payload.name.as_str() == name)
                .unwrap_or_else(|| panic!("payload for mount `{name}`"))
                .spec,
        )
        .expect("serialize payload spec")
    }

    #[test]
    fn populate_session_materializes_host_managed_static_token_to_token_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();
        std::fs::write(
            tmp.path().join("omnifs_provider_github.wasm"),
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
        let (_, payloads) = session.populate(&[config], &catalog, &store).unwrap();

        let written = payload_for(&payloads, "github");
        assert_eq!(
            written["auth"][0]["token_file"], "/run/omnifs/creds/github:pat:default",
            "host-managed static token should rewrite to a session token_file"
        );

        let secret = fs::read_to_string(session.creds_dir.join("github:pat:default")).unwrap();
        assert_eq!(secret, "sk-12345");
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
    fn populate_session_passes_through_token_env_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();

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
        let (_, payloads) = session.populate(&[config], &catalog, &store).unwrap();
        let written = payload_for(&payloads, "dns");
        assert_eq!(written["auth"][0]["token_env"], "FOO");
    }

    #[test]
    fn populate_session_materializes_oauth_credentials_for_container() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store
            .put(
                &key,
                &CredentialEntry::oauth(
                    SecretString::from("gho-access".to_owned()),
                    None,
                    None,
                    "bearer".to_owned(),
                    vec!["repo".to_owned()],
                    OffsetDateTime::UNIX_EPOCH,
                ),
            )
            .unwrap();

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
            tmp.path().join("omnifs_provider_github.wasm"),
            wasm_with_provider_metadata("github", "omnifs_provider_github.wasm"),
        )
        .unwrap();

        let catalog = test_catalog(tmp.path());
        session.populate(&[config], &catalog, &store).unwrap();

        let session_store = FileStore::new(&session.credentials_file);
        let copied = session_store
            .get(&key)
            .unwrap()
            .expect("copied oauth entry");
        assert_eq!(copied.access_token().expose_secret(), "gho-access");
    }

    #[test]
    fn populate_session_applies_provider_metadata_before_oauth_materialization() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();
        std::fs::write(
            tmp.path().join("omnifs_provider_github.wasm"),
            wasm_with_provider_metadata("github", "omnifs_provider_github.wasm"),
        )
        .unwrap();

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store
            .put(
                &key,
                &CredentialEntry::oauth(
                    SecretString::from("gho-access".to_owned()),
                    None,
                    None,
                    "bearer".to_owned(),
                    vec![],
                    OffsetDateTime::UNIX_EPOCH,
                ),
            )
            .unwrap();

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
        session.populate(&[config], &catalog, &store).unwrap();

        let session_store = FileStore::new(&session.credentials_file);
        assert!(
            session_store.get(&key).unwrap().is_some(),
            "metadata default auth should cause the OAuth credential to be copied"
        );
    }

    #[test]
    fn populate_session_uses_builtin_metadata_without_host_wasm() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();

        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store
            .put(
                &key,
                &CredentialEntry::oauth(
                    SecretString::from("gho-access".to_owned()),
                    None,
                    None,
                    "bearer".to_owned(),
                    vec![],
                    OffsetDateTime::UNIX_EPOCH,
                ),
            )
            .unwrap();

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
        session.populate(&[config], &catalog, &store).unwrap();

        let session_store = FileStore::new(&session.credentials_file);
        assert!(
            session_store.get(&key).unwrap().is_some(),
            "built-in metadata default auth should cause the OAuth credential to be copied"
        );
    }

    #[test]
    fn populate_session_materializes_configured_docker_socket_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();

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
        let (_, payloads) = session.populate(&[config], &catalog, &store).unwrap();

        let written = payload_for(&payloads, "docker");
        assert_eq!(
            written["capabilities"]["unix_sockets"],
            serde_json::json!(["/var/run/docker.sock"]),
        );
    }

    #[test]
    fn populate_session_rewrites_preopens_to_container_bind_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        fs::create_dir_all(&db_dir).unwrap();
        fs::write(db_dir.join("chinook.sqlite"), "").unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();

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
        let (binds, payloads) = session.populate(&[config], &catalog, &store).unwrap();

        assert_eq!(
            binds,
            vec![format!(
                "{}:{HOST_PREOPENS_DIR}/db/0:ro",
                db_dir.canonicalize().unwrap().display()
            )],
        );
        let written = payload_for(&payloads, "db");
        assert_eq!(
            written["capabilities"]["preopened_paths"][0]["host"],
            format!("{HOST_PREOPENS_DIR}/db/0"),
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
    fn populate_session_leaves_manifest_preopens_container_native() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();

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
        let (binds, payloads) = session.populate(&[config], &catalog, &store).unwrap();

        assert!(
            binds.is_empty(),
            "manifest preopens are already container paths"
        );
        let written = payload_for(&payloads, "db");
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
    fn populate_session_errors_when_credential_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session {
            root: tmp.path().to_path_buf(),
            creds_dir: tmp.path().join("creds"),
            credentials_file: tmp.path().join(CREDENTIALS_FILE),
        };
        fs::create_dir_all(&session.creds_dir).unwrap();
        std::fs::write(
            tmp.path().join("omnifs_provider_github.wasm"),
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
        let err = session.populate(&[config], &catalog, &store).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("no stored credential"),
            "expected a missing-credential error, got: {chain}"
        );
    }
}
