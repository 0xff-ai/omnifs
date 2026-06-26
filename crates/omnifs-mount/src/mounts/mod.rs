//! Host mount spec loading and resolution.
//!
//! `Spec` represents the raw mount JSON. `Resolved` is the runtime-ready
//! mount after provider metadata has been applied.

pub mod store;

pub use store::{Index, IndexEntry, ProviderStore, StoreError};

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use omnifs_core::{ProviderId, ProviderMeta, ProviderName, ProviderRef, mount};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Auth, OAuth, ProviderConfig, StaticToken};
use omnifs_caps::{Grants, Need};
use omnifs_provider::{AuthManifest, ProviderAuthManifest, ProviderManifest};

/// Raw user-authored mount JSON.
///
/// Loaded from JSON files in the mount spec directory.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct Spec {
    /// The pinned provider reference: the content [`ProviderId`] plus the
    /// [`ProviderMeta`] (name, version) resolved when the CLI pinned it.
    /// Serving resolves the artifact by `provider.id`, never by name.
    pub provider: ProviderRef,
    pub mount: String,
    #[serde(default)]
    pub root_mount: bool,
    #[serde(default, deserialize_with = "crate::deserialize_mount_auth")]
    pub auth: Vec<Auth>,
    pub capabilities: Option<Grants>,
    #[serde(rename = "config")]
    pub config_raw: Option<ProviderConfig>,
}

/// Runtime-ready provider mount.
///
/// Wraps a [`Spec`] with the provider name slug taken from `spec.provider.meta`.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub spec: Spec,
    /// Provider NAME slug (e.g. "github", "linear"), from `spec.provider.meta.name`.
    pub provider_name: String,
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

    /// Fill manifest-declared auth-scheme and config defaults into any field the
    /// user left unset. Capabilities are never filled here: the manifest
    /// declares needs, never grants; `omnifs init` writes explicit grants and
    /// the spec owns them. Identity is never touched: the provider name lives in
    /// `self.provider.meta.name`, not here.
    pub fn apply_provider_metadata(
        &mut self,
        manifest: &omnifs_provider::ProviderManifest,
    ) -> Result<(), serde_json::Error> {
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
        if let Some(schema) = manifest.config_schema.as_ref()
            && self.config_raw.is_none()
        {
            let config = omnifs_provider::ConfigSchema::parse(schema)
                .map_err(serde::de::Error::custom)?
                .defaults();
            self.config_raw = Some(ProviderConfig::from_value(config));
        }
        Ok(())
    }

    pub fn into_resolved(
        mut self,
        manifest: Option<&omnifs_provider::ProviderManifest>,
    ) -> Result<Resolved, serde_json::Error> {
        if let Some(manifest) = manifest {
            self.apply_provider_metadata(manifest)?;
        }
        let provider_name = self.provider.meta.name.to_string();
        Ok(Resolved {
            spec: self,
            provider_name,
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
    #[error("failed to resolve mount: {0}")]
    Resolve(serde_json::Error),
    #[error("provider store error: {0}")]
    Store(#[from] StoreError),
    #[error("provider artifact at {} has no embedded metadata section", path.display())]
    MissingProviderMetadata { path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Catalog {
    mounts_dir: PathBuf,
    providers_dir: PathBuf,
}

/// What materialization reads from a pinned manifest: the capability needs (the
/// oracle for the required-capabilities check) and the parsed config schema,
/// which names the host-resource config fields a dynamic grant resolves from.
pub struct AppliedMetadata {
    pub needs: Vec<Need>,
    pub config_schema: Option<omnifs_provider::ConfigSchema>,
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

    pub fn load_spec(&self, config_path: &Path) -> Result<Spec, Error> {
        load_spec(config_path)
    }

    pub fn apply_metadata(&self, config: &mut Spec) -> Result<bool, Error> {
        let Some(provider) = self.get(&config.provider.id)? else {
            return Ok(false);
        };
        let manifest = provider.manifest()?;
        config
            .apply_provider_metadata(&manifest)
            .map_err(|source| Error::ApplyProviderMetadata {
                path: provider.wasm_path().to_path_buf(),
                source,
            })?;
        Ok(true)
    }

    /// Apply the pinned manifest's metadata to `spec` and return what
    /// materialization checks against it, loading the artifact once. `None` when
    /// the artifact is not retained (metadata is not applied and the checks are
    /// skipped; the missing-artifact error surfaces elsewhere).
    pub fn apply_metadata_and_needs(
        &self,
        spec: &mut Spec,
    ) -> Result<Option<AppliedMetadata>, Error> {
        let Some(provider) = self.get(&spec.provider.id)? else {
            return Ok(None);
        };
        let manifest = provider.manifest()?;
        spec.apply_provider_metadata(&manifest)
            .map_err(|source| Error::ApplyProviderMetadata {
                path: provider.wasm_path().to_path_buf(),
                source,
            })?;
        let config_schema = manifest
            .config_schema
            .as_ref()
            .map(omnifs_provider::ConfigSchema::parse)
            .transpose()
            .map_err(|source| Error::ExtractProviderMetadata {
                path: provider.wasm_path().to_path_buf(),
                source,
            })?;
        Ok(Some(AppliedMetadata {
            needs: manifest.capabilities,
            config_schema,
        }))
    }

    pub fn auth_manifest_for(&self, config: &Resolved) -> Result<Option<AuthManifest>, Error> {
        let Some(provider) = self.get(&config.spec.provider.id)? else {
            return Ok(None);
        };
        Ok(provider.manifest()?.wasm_auth_manifest())
    }

    /// The full auth block (including display guidance) for a single mount's
    /// provider. Unlike [`auth_manifest_for`](Self::auth_manifest_for), which
    /// returns the injection-only wire form, this carries the per-scheme setup
    /// guidance. Loads only this mount's pinned artifact.
    pub fn provider_auth_manifest_for(
        &self,
        config: &Resolved,
    ) -> Result<Option<ProviderAuthManifest>, Error> {
        let Some(provider) = self.get(&config.spec.provider.id)? else {
            return Ok(None);
        };
        Ok(provider.manifest()?.auth)
    }

    /// The serving path for a resolved mount: `by-hash/<hex>.wasm` for its
    /// pinned [`ProviderId`].
    #[must_use]
    pub fn provider_path(&self, config: &Resolved) -> PathBuf {
        self.provider_path_by_id(&config.spec.provider.id)
    }

    /// The content-addressed store rooted at this catalog's providers dir.
    #[must_use]
    pub fn store(&self) -> ProviderStore {
        ProviderStore::new(&self.providers_dir)
    }

    fn provider_from_entry(&self, entry: &IndexEntry) -> Provider {
        Provider {
            id: entry.id,
            meta: ProviderMeta {
                name: entry.name.clone(),
                version: entry.version.clone(),
            },
            artifact: ProviderArtifact {
                wasm_path: self.store().by_hash_path(&entry.id),
            },
        }
    }

    /// Resolve a pinned id to its retained artifact. `None` means the artifact is
    /// not retained (the use site raises `ArtifactMissing`).
    pub fn get(&self, id: &ProviderId) -> Result<Option<Provider>, Error> {
        let index = self.store().read_index()?;
        let Some(entry) = index.providers.iter().find(|entry| &entry.id == id) else {
            return Ok(None);
        };
        let provider = self.provider_from_entry(entry);
        Ok(provider.wasm_path().exists().then_some(provider))
    }

    /// The most recently installed artifact for a name. Init and upgrade only,
    /// never serving.
    pub fn latest_by_name(&self, name: &ProviderName) -> Result<Option<Provider>, Error> {
        let index = self.store().read_index()?;
        let Some(id) = index.latest.get(name.as_str()) else {
            return Ok(None);
        };
        Ok(index
            .providers
            .iter()
            .find(|entry| &entry.id == id)
            .map(|entry| self.provider_from_entry(entry)))
    }

    /// All installed providers (picker / `omnifs init` listing).
    pub fn list(&self) -> Result<Vec<Provider>, Error> {
        let index = self.store().read_index()?;
        Ok(index
            .providers
            .iter()
            .map(|entry| self.provider_from_entry(entry))
            .collect())
    }

    /// `by-hash/<hex>.wasm` for a pinned id (the serving path).
    #[must_use]
    pub fn provider_path_by_id(&self, id: &ProviderId) -> PathBuf {
        self.store().by_hash_path(id)
    }

    /// The host-internal archive tool, installed flat (never in by-hash/index).
    #[must_use]
    pub fn archive_tool_path(&self, file: &str) -> PathBuf {
        self.providers_dir.join(file)
    }
}

/// A retained provider artifact resolved from the store: content id, catalog/UI
/// meta, and a lazily-read handle to the by-hash WASM.
#[derive(Debug, Clone)]
pub struct Provider {
    pub id: ProviderId,
    pub meta: ProviderMeta,
    artifact: ProviderArtifact,
}

#[derive(Debug, Clone)]
struct ProviderArtifact {
    wasm_path: PathBuf,
}

impl Provider {
    /// The pinned reference the CLI writes into a mount spec.
    #[must_use]
    pub fn reference(&self) -> ProviderRef {
        ProviderRef {
            id: self.id,
            meta: self.meta.clone(),
        }
    }

    /// `by-hash/<hex>.wasm` path of this artifact.
    #[must_use]
    pub fn wasm_path(&self) -> &Path {
        &self.artifact.wasm_path
    }

    /// The provider manifest embedded in the artifact's metadata section.
    pub fn manifest(&self) -> Result<ProviderManifest, Error> {
        read_provider_metadata_file(&self.artifact.wasm_path)?.ok_or_else(|| {
            Error::MissingProviderMetadata {
                path: self.artifact.wasm_path.clone(),
            }
        })
    }
}

/// Resolve a raw [`Spec`] against the provider index: fill manifest defaults
/// into unset fields and attach the provider name slug, yielding a [`Resolved`].
///
/// `require_metadata` selects strict (the pinned artifact must be retained, so
/// its manifest can hydrate the spec; serving and auth paths) versus best-effort
/// (skip hydration when the artifact is gone; delete/reset/ls display paths).
///
/// This is the explicit join. It belongs to neither catalog: it is the point
/// where a held `&Catalog` and a `&Spec` meet, which is also where pinning
/// already forces the two together. [`materialize`](crate::materialize::materialize)
/// is the deeper join that additionally extracts capability needs and rewrites
/// preopens.
pub fn resolve(catalog: &Catalog, spec: &Spec, require_metadata: bool) -> Result<Resolved, Error> {
    let mut config = spec.clone();
    // Best-effort for delete/reset paths; strict when metadata is required.
    let applied = catalog.apply_metadata(&mut config);
    if require_metadata {
        applied?;
    }
    config.into_resolved(None).map_err(Error::Resolve)
}

/// In-memory mirror of the on-disk mount-spec directory, and the sole owner of
/// mount specs.
///
/// `Registry` reads every `mounts/*.json` once into memory and serves lookups;
/// it replaces the two duplicated scan-and-parse pipelines (the CLI's
/// `Workspace::mounts` and the host reconcile scan). Parsing is tolerant: a file
/// that fails to parse or carries an invalid mount name is recorded in
/// [`failures`](Self::failures) rather than aborting the load, so one malformed
/// file cannot hide every other mount. Callers that want strict behavior inspect
/// `failures` themselves.
///
/// A `Registry` is a per-process snapshot, not a shared singleton; disk stays
/// the source of truth across the CLI and daemon processes. The CLI mutates
/// through its `Registry` then triggers a daemon reconcile, which rebuilds its
/// own `Registry` from disk via [`reload`](Self::reload).
#[derive(Debug)]
pub struct Registry {
    mounts_dir: PathBuf,
    specs: BTreeMap<mount::Name, Spec>,
    failures: Vec<SpecLoadFailure>,
}

/// A `mounts/*.json` file that failed to load, retained so a tolerant reader
/// (host reconcile, `omnifs reset`) can still account for it.
#[derive(Debug)]
pub struct SpecLoadFailure {
    pub path: PathBuf,
    pub error: Error,
}

impl Registry {
    /// Read and parse every `*.json` under `mounts_dir`. Errors only on a
    /// directory-scan I/O failure; per-file parse and mount-name errors land in
    /// [`failures`](Self::failures).
    pub fn load(mounts_dir: impl AsRef<Path>) -> Result<Self, Error> {
        let mut registry = Self {
            mounts_dir: mounts_dir.as_ref().to_path_buf(),
            specs: BTreeMap::new(),
            failures: Vec::new(),
        };
        registry.scan()?;
        Ok(registry)
    }

    fn scan(&mut self) -> Result<(), Error> {
        self.specs.clear();
        self.failures.clear();
        let paths = spec_paths_in(&self.mounts_dir).map_err(|source| Error::ScanMounts {
            path: self.mounts_dir.clone(),
            source,
        })?;
        for path in paths {
            match Spec::from_file(&path) {
                Ok(spec) => match mount::Name::new(spec.mount.clone()) {
                    Ok(name) => {
                        self.specs.insert(name, spec);
                    },
                    Err(source) => self.failures.push(SpecLoadFailure {
                        error: Error::MountName {
                            path: path.clone(),
                            mount: spec.mount,
                            source,
                        },
                        path,
                    }),
                },
                Err(error) => self.failures.push(SpecLoadFailure { path, error }),
            }
        }
        Ok(())
    }

    /// Re-read the directory from disk (daemon reconcile, post-write refresh).
    pub fn reload(&mut self) -> Result<(), Error> {
        self.scan()
    }

    /// The pinned spec for `name`, if loaded.
    #[must_use]
    pub fn get(&self, name: &mount::Name) -> Option<&Spec> {
        self.specs.get(name)
    }

    /// Every loaded spec, in mount-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&mount::Name, &Spec)> + '_ {
        self.specs.iter()
    }

    /// The files that failed to load, in directory-scan order.
    #[must_use]
    pub fn failures(&self) -> &[SpecLoadFailure] {
        &self.failures
    }

    #[must_use]
    pub fn mounts_dir(&self) -> &Path {
        &self.mounts_dir
    }

    /// The on-disk path a mount's spec occupies: `mounts_dir/<name>.json`.
    #[must_use]
    pub fn spec_path(&self, name: &mount::Name) -> PathBuf {
        self.mounts_dir.join(format!("{name}.json"))
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

    fn linear_manifest() -> omnifs_provider::ProviderManifest {
        provider_manifest_from_wasm("omnifs_provider_linear.wasm")
    }

    fn github_manifest() -> omnifs_provider::ProviderManifest {
        provider_manifest_from_wasm("omnifs_provider_github.wasm")
    }

    fn provider_manifest_from_wasm(file: &str) -> omnifs_provider::ProviderManifest {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wasm32-wasip2/release")
            .join(file);
        read_provider_metadata_file(&path)
            .unwrap_or_else(|source| {
                panic!("read provider metadata from {}: {source}", path.display())
            })
            .unwrap_or_else(|| {
                panic!(
                    "provider metadata missing from {}; run `just providers-build`",
                    path.display()
                )
            })
    }

    fn provider_ref(name: &str) -> ProviderRef {
        ProviderRef {
            id: ProviderId::from_wasm_bytes(name.as_bytes()),
            meta: ProviderMeta {
                name: ProviderName::new(name).unwrap(),
                version: None,
            },
        }
    }

    /// Build a `Spec` from a JSON `body` (no `provider` field) plus a dummy
    /// pinned `ProviderRef` named `name`. Parse-only: the id resolves in no store.
    fn spec_with_provider(name: &str, body: &str) -> Spec {
        let mut value: serde_json::Value = serde_json::from_str(body).unwrap();
        value["provider"] = serde_json::to_value(provider_ref(name)).unwrap();
        serde_json::from_value(value).unwrap()
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
        let cfg = spec_with_provider("linear", r#"{ "mount": "linear" }"#);

        let cfg = cfg.into_resolved(Some(&manifest)).unwrap();

        assert_eq!(cfg.provider_name, "linear");
        assert_eq!(cfg.spec.auth.len(), 1);
        assert!(cfg.spec.auth[0].is_oauth());
        assert_eq!(cfg.spec.auth[0].scheme(), Some("oauth"));
    }

    #[test]
    fn loader_rejects_invalid_mount_name_in_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("bad.json");
        let spec = spec_with_provider("p", r#"{ "mount": "Bad-Name", "config": {} }"#);
        std::fs::write(&path, serde_json::to_string(&spec).unwrap()).expect("write config");

        let catalog = Catalog::new(dir.path(), dir.path());
        let error = catalog.load_spec(&path).expect_err("invalid mount name");
        assert!(matches!(error, Error::MountName { .. }));
    }

    fn write_spec(dir: &Path, file: &str, spec: &Spec) {
        std::fs::write(dir.join(file), serde_json::to_string(spec).unwrap()).unwrap();
    }

    #[test]
    fn registry_loads_specs_in_name_order_and_isolates_bad_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mounts = dir.path();
        // Two valid specs, written out of name order.
        write_spec(
            mounts,
            "zeta.json",
            &spec_with_provider("zeta", r#"{ "mount": "zeta" }"#),
        );
        write_spec(
            mounts,
            "alpha.json",
            &spec_with_provider("alpha", r#"{ "mount": "alpha" }"#),
        );
        // An unparseable file, and a file whose mount name is a path traversal.
        std::fs::write(mounts.join("broken.json"), b"{ not json").unwrap();
        write_spec(
            mounts,
            "poison.json",
            &spec_with_provider("p", r#"{ "mount": "../../../tmp/poison" }"#),
        );

        let registry = Registry::load(mounts).expect("scan succeeds");

        // Valid specs are served in mount-name order.
        let names: Vec<_> = registry.iter().map(|(name, _)| name.to_string()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert!(
            registry
                .get(&mount::Name::new("alpha".to_owned()).unwrap())
                .is_some()
        );

        // Both malformed files are recorded as failures and never served; a path
        // traversal in the mount name is rejected, not turned into a key.
        assert_eq!(registry.failures().len(), 2);
        assert!(
            registry
                .failures()
                .iter()
                .any(|f| matches!(f.error, Error::ParseSpec { .. }))
        );
        assert!(
            registry
                .failures()
                .iter()
                .any(|f| matches!(f.error, Error::MountName { .. }))
        );
    }

    #[test]
    fn registry_tolerates_missing_dir_and_derives_spec_path() {
        let registry = Registry::load("/no/such/mounts").expect("missing dir is not an error");
        assert!(registry.iter().next().is_none());
        let name = mount::Name::new("github".to_owned()).unwrap();
        assert_eq!(
            registry.spec_path(&name),
            Path::new("/no/such/mounts/github.json")
        );
    }
}
