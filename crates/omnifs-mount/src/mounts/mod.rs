//! Host mount spec loading and resolution.
//!
//! `Spec` represents the raw mount JSON. `Resolved` is the runtime-ready
//! mount after provider metadata has been applied.

mod builtins;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use builtins::Builtins;
use omnifs_core::{IdError, ProviderId, mount};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Auth, Contract, OAuth, ProviderConfig, StaticToken};
use omnifs_provider::{
    AuthManifest, ProviderAuthManifest, ProviderCapabilities, ProviderManifest,
    UnixSocketEndpointError,
};

/// Raw user-authored mount JSON.
///
/// Loaded from JSON files in the mount spec directory.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct Spec {
    /// Filename of the provider WASM component this mount loads, looked
    /// up in `providers_dir`.
    pub provider: String,
    pub mount: String,
    /// Stable provider identity from the provider metadata custom section.
    /// This is runtime-derived, not a user-authored config field.
    #[serde(default, skip)]
    #[schema(ignore)]
    provider_id: Option<String>,
    /// Provider config schema from the provider metadata custom section.
    #[serde(default, skip)]
    #[schema(ignore)]
    provider_config_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub root_mount: bool,
    #[serde(default, deserialize_with = "crate::deserialize_mount_auth")]
    pub auth: Vec<Auth>,
    pub capabilities: Option<ProviderCapabilities>,
    #[serde(rename = "config")]
    pub config_raw: Option<ProviderConfig>,
    /// Provider contract snapshot stamped at `omnifs init` time.
    ///
    /// Records the config fields, capabilities, and auth scheme the spec was
    /// built against, so `omnifs up` can classify and route a contract delta
    /// after a provider upgrade. Absent on specs written before contract
    /// versioning was introduced; those specs skip the pre-flight and run
    /// straight to reconcile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<Contract>,
}

/// Runtime-ready provider mount.
///
/// Wraps a [`Spec`] with the resolved stable provider id.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub spec: Spec,
    pub provider_id: String,
}

impl Spec {
    pub fn parse(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, Error> {
        let content = std::fs::read_to_string(path).map_err(|source| Error::ReadSpec {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&content).map_err(|source| Error::ParseSpec {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn config_bytes(&self) -> Vec<u8> {
        self.config_raw
            .as_ref()
            .map_or_else(|| b"{}".to_vec(), ProviderConfig::to_bytes)
    }

    /// Stamp a contract block derived from `manifest` into this spec.
    ///
    /// Called by `omnifs init` after the mount file is written, so the spec
    /// carries the contract it was built against. Also used by the `omnifs up`
    /// pre-flight when it re-stamps after auto-migrating an additive change.
    pub fn stamp_contract(&mut self, manifest: &ProviderManifest) {
        self.contract = Some(Contract::from_manifest(manifest));
    }

    #[must_use]
    pub fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }

    #[must_use]
    pub fn provider_config_schema(&self) -> Option<&serde_json::Value> {
        self.provider_config_schema.as_ref()
    }

    pub fn materialize_runtime_capabilities(&mut self) -> Result<(), RuntimeCapabilitiesError> {
        let Some(endpoint) = self
            .config_raw
            .as_ref()
            .and_then(|config| config.as_value().get("endpoint"))
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(());
        };
        self.capabilities
            .get_or_insert_with(ProviderCapabilities::default)
            .grant_configured_unix_socket(endpoint)
            .map_err(RuntimeCapabilitiesError::ConfiguredUnixSocket)
    }

    pub fn apply_provider_metadata(
        &mut self,
        manifest: &omnifs_provider::ProviderManifest,
    ) -> Result<(), serde_json::Error> {
        self.provider_id = Some(manifest.id.clone());
        if self.auth.is_empty()
            && let Some(auth) = &manifest.auth
            && let Some(default_scheme) = auth.schemes.get(&auth.default)
        {
            let auth = match default_scheme {
                omnifs_provider::AuthScheme::StaticToken(_) => {
                    Some(Auth::StaticToken(StaticToken {
                        scheme: Some(auth.default.clone()),
                        ..StaticToken::default()
                    }))
                },
                omnifs_provider::AuthScheme::Oauth(_) => Some(Auth::OAuth(OAuth {
                    scheme: Some(auth.default.clone()),
                    ..OAuth::default()
                })),
                // Manifest validation rejects None at load time; this arm
                // makes future variants force a compile-time decision.
                omnifs_provider::AuthScheme::None => None,
            };
            if let Some(auth) = auth {
                self.auth.push(auth);
            }
        }
        if self.capabilities.is_none() && !manifest.capabilities.is_empty() {
            self.capabilities = Some(manifest.provider_capabilities());
        }
        if let Some(schema) = manifest.config_schema.as_ref() {
            if self.config_raw.is_none() {
                let config = omnifs_provider::ConfigSchema::parse(schema)
                    .map_err(serde::de::Error::custom)?
                    .defaults();
                self.config_raw = Some(ProviderConfig::from_value(config));
            }
            self.provider_config_schema = Some(schema.as_value().clone());
        }
        Ok(())
    }

    pub fn into_resolved(
        mut self,
        fallback_provider_id: impl Into<String>,
        manifest: Option<&omnifs_provider::ProviderManifest>,
    ) -> Result<Resolved, serde_json::Error> {
        if let Some(manifest) = manifest {
            self.apply_provider_metadata(manifest)?;
        }
        let provider_id = self
            .provider_id
            .take()
            .unwrap_or_else(|| fallback_provider_id.into());
        Ok(Resolved {
            spec: self,
            provider_id,
        })
    }
}

impl Resolved {
    /// Convenience delegate for callers that need the raw config bytes.
    #[must_use]
    pub fn config_bytes(&self) -> Vec<u8> {
        self.spec.config_bytes()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read mount spec {}: {source}", path.display())]
    ReadSpec {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse mount spec {}: {source}", path.display())]
    ParseSpec {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid mount name `{mount}` in {}: {source}", path.display())]
    MountName {
        path: PathBuf,
        mount: String,
        source: mount::NameError,
    },
    #[error("failed to scan mount config directory {}: {source}", path.display())]
    ScanMounts {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read provider metadata from {}: {source}", path.display())]
    ReadProviderMetadata {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to extract provider metadata from {}: {source}", path.display())]
    ExtractProviderMetadata {
        path: PathBuf,
        source: omnifs_provider::ProviderMetadataError,
    },
    #[error("failed to apply provider metadata from {}: {source}", path.display())]
    ApplyProviderMetadata {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to apply built-in provider metadata for `{provider_id}`: {source}")]
    ApplyBuiltinMetadata {
        provider_id: String,
        source: serde_json::Error,
    },
    #[error("failed to resolve mount: {0}")]
    Resolve(serde_json::Error),
    #[error("failed to load built-in provider manifests: {0}")]
    BuiltinManifest(String),
    #[error("invalid provider id `{id}`: {source}")]
    ProviderId { id: String, source: IdError },
    #[error("cannot derive provider id from provider `{0}`")]
    ProviderIdFromProvider(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeCapabilitiesError {
    #[error("failed to materialize configured unix socket grant: {0}")]
    ConfiguredUnixSocket(UnixSocketEndpointError),
}

#[derive(Debug, Clone)]
pub struct Catalog {
    mounts_dir: PathBuf,
    providers_dir: PathBuf,
}

impl Catalog {
    #[must_use]
    pub fn new(mounts_dir: impl AsRef<Path>, providers_dir: impl AsRef<Path>) -> Self {
        Self {
            mounts_dir: mounts_dir.as_ref().to_path_buf(),
            providers_dir: providers_dir.as_ref().to_path_buf(),
        }
    }

    #[must_use]
    pub fn for_providers(providers_dir: impl AsRef<Path>) -> Self {
        Self {
            mounts_dir: PathBuf::new(),
            providers_dir: providers_dir.as_ref().to_path_buf(),
        }
    }

    #[must_use]
    pub fn mounts_dir(&self) -> &Path {
        &self.mounts_dir
    }

    #[must_use]
    pub fn providers_dir(&self) -> &Path {
        &self.providers_dir
    }

    #[must_use]
    pub fn spec_path(&self, name: &mount::Name) -> PathBuf {
        self.mounts_dir.join(format!("{name}.json"))
    }

    pub fn spec_paths(&self) -> Result<Vec<PathBuf>, Error> {
        spec_paths_in(&self.mounts_dir).map_err(|source| Error::ScanMounts {
            path: self.mounts_dir.clone(),
            source,
        })
    }

    pub fn resolve(&self, spec_path: &Path) -> Result<Resolved, Error> {
        Resolver::new(self).resolve(spec_path)
    }

    pub fn resolve_by_name(&self, name: &mount::Name) -> Result<Resolved, Error> {
        self.resolve(&self.spec_path(name))
    }

    pub fn load_spec(&self, config_path: &Path) -> Result<Spec, Error> {
        load_spec(config_path)
    }

    pub fn resolve_spec(&self, spec: Spec, require_metadata: bool) -> Result<Resolved, Error> {
        Resolver::new(self)
            .with_required_metadata(require_metadata)
            .resolve_spec(spec)
    }

    pub fn apply_metadata(&self, config: &mut Spec) -> Result<bool, Error> {
        if let Some((path, manifest)) = self.load_disk_provider_manifest(&config.provider)? {
            config
                .apply_provider_metadata(&manifest)
                .map_err(|source| Error::ApplyProviderMetadata {
                    path: path.clone(),
                    source,
                })?;
            return Ok(true);
        }
        Builtins::embedded()?.apply_metadata_to(config)
    }

    /// Derive the live `Contract` for the provider named in `spec`, using the
    /// disk-side provider manifest when available and falling back to the
    /// embedded built-in index. Returns `None` when neither a disk manifest
    /// nor a built-in entry exists for the provider.
    pub fn live_contract_for(&self, spec: &Spec) -> Result<Option<Contract>, Error> {
        use crate::Contract;
        if let Some((_path, manifest)) = self.load_disk_provider_manifest(&spec.provider)? {
            return Ok(Some(Contract::from_manifest(&manifest)));
        }
        let Ok(builtins) = Builtins::embedded() else {
            return Ok(None);
        };
        let Some(manifest) = builtins.manifest_for_spec(spec) else {
            return Ok(None);
        };
        Ok(Some(Contract::from_manifest(manifest)))
    }

    /// Config-field default values from the live provider manifest, keyed by
    /// field name. The additive-migration pre-flight uses these to fill new
    /// optional fields into a spec; the contract block records required-ness,
    /// not values, so the defaults must come from the manifest. Same disk-then-
    /// embedded resolution as `live_contract_for`; an absent provider yields an
    /// empty map.
    pub fn live_field_defaults(
        &self,
        spec: &Spec,
    ) -> Result<std::collections::HashMap<String, serde_json::Value>, Error> {
        use crate::contract::config_field_defaults;
        if let Some((_path, manifest)) = self.load_disk_provider_manifest(&spec.provider)? {
            return Ok(config_field_defaults(&manifest));
        }
        let Ok(builtins) = Builtins::embedded() else {
            return Ok(std::collections::HashMap::new());
        };
        Ok(builtins
            .manifest_for_spec(spec)
            .map(config_field_defaults)
            .unwrap_or_default())
    }

    pub fn auth_manifest_for(&self, config: &Resolved) -> Result<Option<AuthManifest>, Error> {
        if let Some((_path, manifest)) = self.load_disk_provider_manifest(&config.spec.provider)?
            && let Some(auth) = manifest.wasm_auth_manifest()
        {
            return Ok(Some(auth));
        }
        Ok(Builtins::embedded()?.auth_manifest_for(config))
    }

    /// The full auth block (including display guidance) for a single mount's
    /// provider. Unlike [`auth_manifest_for`](Self::auth_manifest_for), which
    /// returns the injection-only wire form, this carries the per-scheme setup
    /// guidance. Loads only this mount's provider, never the whole directory.
    pub fn provider_auth_manifest_for(
        &self,
        config: &Resolved,
    ) -> Result<Option<ProviderAuthManifest>, Error> {
        if let Some((_path, manifest)) = self.load_disk_provider_manifest(&config.spec.provider)?
            && manifest.auth.is_some()
        {
            return Ok(manifest.auth);
        }
        Ok(Builtins::embedded()?
            .manifest_for_resolved(config)
            .and_then(|manifest| manifest.auth.clone()))
    }

    #[must_use]
    pub fn provider_path(&self, config: &Resolved) -> PathBuf {
        let provider = PathBuf::from(&config.spec.provider);
        if provider.is_absolute() {
            provider
        } else {
            self.providers_dir.join(provider)
        }
    }

    pub fn provider_id(config: &Spec) -> Result<ProviderId, Error> {
        if let Some(id) = config.provider_id() {
            return ProviderId::new(id).map_err(|source| Error::ProviderId {
                id: id.to_owned(),
                source,
            });
        }
        let stem = Path::new(&config.provider)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| Error::ProviderIdFromProvider(config.provider.clone()))?;
        ProviderId::new(stem).map_err(|source| Error::ProviderId {
            id: stem.to_owned(),
            source,
        })
    }

    pub fn builtin_manifests() -> Result<Vec<ProviderManifest>, Error> {
        Ok(Builtins::embedded()?.all_manifests().to_vec())
    }

    fn load_disk_provider_manifest(
        &self,
        provider: &str,
    ) -> Result<Option<(PathBuf, ProviderManifest)>, Error> {
        let path = self.providers_dir.join(provider);
        read_provider_metadata_file(&path).map(|manifest| manifest.map(|manifest| (path, manifest)))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Resolver<'a> {
    catalog: &'a Catalog,
    require_metadata: bool,
}

impl<'a> Resolver<'a> {
    #[must_use]
    pub fn new(catalog: &'a Catalog) -> Self {
        Self {
            catalog,
            require_metadata: true,
        }
    }

    #[must_use]
    pub fn with_required_metadata(mut self, require_metadata: bool) -> Self {
        self.require_metadata = require_metadata;
        self
    }

    pub fn resolve(&self, spec_path: &Path) -> Result<Resolved, Error> {
        let spec = load_spec(spec_path)?;
        self.resolve_spec(spec)
    }

    pub fn resolve_spec(&self, mut config: Spec) -> Result<Resolved, Error> {
        // Best-effort for delete/reset paths; strict when metadata is required.
        let applied = self.catalog.apply_metadata(&mut config);
        if self.require_metadata {
            applied?;
        }
        let fallback_provider_id = Catalog::provider_id(&config).map_or_else(
            |_| {
                Path::new(&config.provider)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or(&config.mount)
                    .to_string()
            },
            |id| id.to_string(),
        );
        config
            .into_resolved(fallback_provider_id, None)
            .map_err(Error::Resolve)
    }
}

fn load_spec(path: &Path) -> Result<Spec, Error> {
    let config = Spec::from_file(path)?;
    if let Err(source) = mount::Name::new(config.mount.clone()) {
        return Err(Error::MountName {
            path: path.to_path_buf(),
            mount: config.mount.clone(),
            source,
        });
    }
    Ok(config)
}

pub fn spec_paths_in(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut files = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn read_provider_metadata_file(path: &Path) -> Result<Option<ProviderManifest>, Error> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::ReadProviderMetadata {
                path: path.to_path_buf(),
                source,
            });
        },
    };
    omnifs_provider::read_provider_metadata_section(&bytes).map_err(|source| {
        Error::ExtractProviderMetadata {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR_METADATA_JSON: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../providers/linear/omnifs.provider.json"
    ));
    const GITHUB_METADATA_JSON: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../providers/github/omnifs.provider.json"
    ));

    fn linear_manifest() -> omnifs_provider::ProviderManifest {
        omnifs_provider::ProviderManifest::from_bytes(LINEAR_METADATA_JSON.as_bytes())
            .expect("linear manifest must parse")
    }

    fn github_manifest() -> omnifs_provider::ProviderManifest {
        omnifs_provider::ProviderManifest::from_bytes(GITHUB_METADATA_JSON.as_bytes())
            .expect("github manifest must parse")
    }

    #[test]
    fn linear_manifest_parses_with_static_token_scheme() {
        let manifest = linear_manifest();
        let auth = manifest.auth.as_ref().expect("linear auth block");
        let pat = auth.schemes.get("pat").expect("linear pat scheme");
        assert!(matches!(pat, omnifs_provider::AuthScheme::StaticToken(_)));
        let omnifs_provider::AuthScheme::StaticToken(static_token) = pat else {
            unreachable!()
        };
        assert!(static_token.creation_url.is_some());
        let val = static_token.validation.as_ref().expect("validation");
        assert_eq!(val.expect_status, 200);
        assert_eq!(val.json_pointer.as_deref(), Some("/data/viewer/id"));
    }

    #[test]
    fn github_manifest_parses_with_static_token_scheme() {
        let manifest = github_manifest();
        let auth = manifest.auth.as_ref().expect("github auth block");
        let pat = auth.schemes.get("pat").expect("github pat scheme");
        let omnifs_provider::AuthScheme::StaticToken(static_token) = pat else {
            panic!("expected static token");
        };
        assert_eq!(auth.inject.prefix, "Bearer ");
        let val = static_token.validation.as_ref().expect("validation");
        assert_eq!(val.method, "GET");
        assert_eq!(val.expect_status, 200);
    }

    #[test]
    fn thin_config_inherits_provider_metadata_defaults() {
        let manifest = linear_manifest();
        let cfg = Spec::parse(
            r#"{
                "provider": "omnifs_provider_linear.wasm",
                "mount": "linear"
            }"#,
        )
        .expect("minimal config must parse");

        let cfg = cfg
            .into_resolved("omnifs_provider_linear", Some(&manifest))
            .unwrap();

        assert_eq!(cfg.provider_id, "linear");
        assert_eq!(cfg.spec.auth.len(), 1);
        assert!(cfg.spec.auth[0].is_oauth());
        assert_eq!(cfg.spec.auth[0].scheme(), Some("oauth"));
        assert_eq!(
            cfg.spec
                .capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.max_memory_mb),
            Some(128),
        );
    }

    #[test]
    fn runtime_capabilities_materialize_configured_unix_socket() {
        let mut cfg = Spec::parse(
            r#"{
                "provider": "omnifs_provider_docker.wasm",
                "mount": "docker",
                "capabilities": {
                    "unix_sockets": ["docker-host-socket", "/tmp/kept.sock"]
                },
                "config": {"endpoint": "unix:///var/run/docker.sock"}
            }"#,
        )
        .expect("docker config must parse");

        cfg.materialize_runtime_capabilities()
            .expect("unix endpoint must materialize");

        assert_eq!(
            cfg.capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.unix_sockets.as_ref()),
            Some(&vec![
                "/tmp/kept.sock".to_string(),
                "/var/run/docker.sock".to_string(),
            ]),
        );
    }

    #[test]
    fn loader_rejects_invalid_mount_name_in_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("bad.json");
        std::fs::write(
            &path,
            r#"{"provider":"p.wasm","mount":"Bad-Name","config":{}}"#,
        )
        .expect("write config");

        let catalog = Catalog::new(dir.path(), dir.path());
        let error = catalog.load_spec(&path).expect_err("invalid mount name");
        assert!(matches!(error, Error::MountName { .. }));
    }
}
