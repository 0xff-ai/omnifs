//! The omnifs mount: the user-authored mount `Spec` (which bakes in its
//! provider-manifest defaults at creation), the `Registry` that owns specs on
//! disk, materialization against the provider manifest in [`crate::provider`],
//! and provider upgrade classification. Plus the sparse user `Auth` config.
//!
//! `Spec` represents the mount JSON. Provider-manifest defaults (the auth scheme
//! and config defaults) are baked into the spec at creation time by the CLI's
//! spec creator, so loading a spec is a plain parse: there is no read-time
//! resolution step and no separate runtime-ready type.

pub mod auth;
pub mod materialize;
pub mod name;
pub mod upgrade;

pub use auth::{Auth, AuthKind, OAuth, StaticToken};
pub use name::{Name, NameError};
pub use upgrade::{
    AddedField, AuthDelta, CapabilityChange, CapabilityDirection, FieldChange, UpgradePlan,
};

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::ids::{ProviderName, ProviderRef};
use serde::{Deserialize, Serialize};

use crate::provider::{Catalog, CatalogError, ProviderManifest};
use omnifs_caps::Grants;

/// Raw user-authored mount JSON.
///
/// Loaded from JSON files in the mount spec directory.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Spec {
    /// The pinned provider reference: the content [`ProviderId`] plus the
    /// [`ProviderMeta`] (name, version) resolved when the CLI pinned it.
    /// Serving resolves the artifact by `provider.id`, never by name.
    pub provider: ProviderRef,
    pub mount: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub root_mount: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Grants>,
    /// Opaque provider config. The provider owns its meaning, so the host keeps
    /// it as a free-form value and never parses it.
    #[serde(rename = "config", skip_serializing_if = "Option::is_none")]
    pub config_raw: Option<serde_json::Value>,
}

/// `skip_serializing_if` predicate: omit a `bool` field when it is `false`, so a
/// `Registry`-written spec matches the compact authored form (no `root_mount`
/// key unless the mount is a root mount).
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip predicate ABI"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

impl Spec {
    pub fn parse(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// The provider NAME slug (e.g. "github", "linear"), pinned in
    /// `provider.meta.name`. Credentials key on this slug, not the content id, so
    /// they survive provider upgrades.
    #[must_use]
    pub fn provider_name(&self) -> &ProviderName {
        &self.provider.meta.name
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

    #[must_use]
    pub fn config_bytes(&self) -> Vec<u8> {
        self.config_raw.as_ref().map_or_else(
            || b"{}".to_vec(),
            |config| serde_json::to_vec(config).unwrap_or_else(|_| b"{}".to_vec()),
        )
    }

    /// Fill manifest-declared auth-scheme and config defaults into any field the
    /// user left unset. Capabilities are never filled here: the manifest
    /// declares needs, never grants; `omnifs init` writes explicit grants and
    /// the spec owns them. Identity is never touched: the provider name lives in
    /// `self.provider.meta.name`, not here.
    pub fn apply_provider_metadata(
        &mut self,
        manifest: &crate::provider::ProviderManifest,
    ) -> Result<(), serde_json::Error> {
        if self.auth.is_none()
            && let Some(auth) = &manifest.auth
            && let Some(default_scheme) = auth.scheme(&auth.default)
        {
            self.auth = match default_scheme {
                crate::authn::AuthScheme::StaticToken(_) => Some(Auth::StaticToken(StaticToken {
                    scheme: Some(auth.default.clone()),
                    ..StaticToken::default()
                })),
                crate::authn::AuthScheme::Oauth(_) => Some(Auth::OAuth(OAuth {
                    scheme: Some(auth.default.clone()),
                    ..OAuth::default()
                })),
                // Manifest validation rejects None at load time; this arm
                // makes future variants force a compile-time decision.
                crate::authn::AuthScheme::None => None,
            };
        }
        if let Some(config) = manifest.config.as_ref()
            && self.config_raw.is_none()
        {
            self.config_raw = Some(config.defaults());
        }
        Ok(())
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
        source: name::NameError,
    },
    #[error(
        "mount spec {} declares mount `{mount}` but must be named `{mount}.json`",
        path.display()
    )]
    FilenameMismatch { path: PathBuf, mount: String },
    #[error("failed to scan mount config directory {}: {source}", path.display())]
    ScanMounts {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config schema for provider `{provider}`: {source}")]
    ConfigSchema {
        provider: String,
        source: crate::provider::ProviderMetadataError,
    },
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("failed to serialize mount spec for {}: {source}", path.display())]
    SerializeSpec {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to write mount spec {}: {source}", path.display())]
    WriteSpec {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to remove mount spec {}: {source}", path.display())]
    RemoveSpec {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The pinned provider's manifest, or `None` when the artifact is not retained
/// (the missing-artifact error then surfaces at build time). Loads the artifact
/// once and never mutates the spec: defaults are baked in at creation time, not
/// here. Callers pluck what they need (`capabilities`, `config`,
/// `wasm_auth_manifest()`, the full `auth` block) from the returned manifest.
pub fn pinned_manifest(catalog: &Catalog, spec: &Spec) -> Result<Option<ProviderManifest>, Error> {
    let Some(provider) = catalog.get(&spec.provider.id)? else {
        return Ok(None);
    };
    Ok(Some(provider.manifest()?))
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
    specs: BTreeMap<name::Name, Spec>,
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
            let spec = match Spec::from_file(&path) {
                Ok(spec) => spec,
                Err(error) => {
                    self.failures.push(SpecLoadFailure { path, error });
                    continue;
                },
            };
            let name = match name::Name::new(spec.mount.clone()) {
                Ok(name) => name,
                Err(source) => {
                    self.failures.push(SpecLoadFailure {
                        error: Error::MountName {
                            path: path.clone(),
                            mount: spec.mount,
                            source,
                        },
                        path,
                    });
                    continue;
                },
            };
            // The file name carries the mount identity: a spec lives at
            // `<mount>.json`. Enforcing that here keeps the read side consistent
            // with `spec_path`/`put`/`remove` (which derive the file from the
            // name), so a misnamed file -- or a second file declaring an
            // already-claimed mount -- surfaces as a loud failure instead of
            // being silently mis-served or leaving `rm`/`upgrade` to act on the
            // wrong path.
            let stem_matches = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| spec.mount == stem);
            if !stem_matches {
                self.failures.push(SpecLoadFailure {
                    error: Error::FilenameMismatch {
                        path: path.clone(),
                        mount: spec.mount,
                    },
                    path,
                });
                continue;
            }
            self.specs.insert(name, spec);
        }
        Ok(())
    }

    /// Re-read the directory from disk (daemon reconcile, post-write refresh).
    pub fn reload(&mut self) -> Result<(), Error> {
        self.scan()
    }

    /// The pinned spec for `name`, if loaded.
    #[must_use]
    pub fn get(&self, name: &name::Name) -> Option<&Spec> {
        self.specs.get(name)
    }

    /// Every loaded spec, in mount-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&name::Name, &Spec)> + '_ {
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
    pub fn spec_path(&self, name: &name::Name) -> PathBuf {
        self.mounts_dir.join(format!("{name}.json"))
    }

    /// Persist `spec` and update the in-memory mirror. The spec's mount name
    /// (validated here) names the file `mounts_dir/<name>.json`; the write is
    /// atomic (a same-directory temp file renamed into place), so a concurrent
    /// reader (the daemon reconcile) sees either the old file or the new one,
    /// never a torn write.
    ///
    /// Specs are one file per mount with no shared mutable index, so atomic
    /// per-file rename is sufficient; unlike the provider store's `index.json`
    /// read-modify-write, no advisory lock is needed.
    pub fn put(&mut self, spec: &Spec) -> Result<(), Error> {
        let name = name::Name::new(spec.mount.clone()).map_err(|source| Error::MountName {
            path: self.mounts_dir.clone(),
            mount: spec.mount.clone(),
            source,
        })?;
        let path = self.spec_path(&name);
        let mut json =
            serde_json::to_string_pretty(spec).map_err(|source| Error::SerializeSpec {
                path: path.clone(),
                source,
            })?;
        json.push('\n');
        write_spec_atomic(&self.mounts_dir, &path, json.as_bytes())?;
        self.specs.insert(name, spec.clone());
        Ok(())
    }

    /// Remove a mount's spec file and drop it from the mirror. Returns whether a
    /// file was present (a missing file is not an error).
    pub fn remove(&mut self, name: &name::Name) -> Result<bool, Error> {
        self.specs.remove(name);
        let path = self.spec_path(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::RemoveSpec { path, source }),
        }
    }
}

/// Write `bytes` to `path` atomically: serialize to a same-directory temp file,
/// then rename over the target. `rename(2)` is atomic on a single filesystem, so
/// a concurrent reader never observes a partial spec. The temp name is dot-hidden
/// and lacks a `.json` extension, so [`spec_paths_in`] skips it even if a crash
/// leaves it behind.
fn write_spec_atomic(mounts_dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), Error> {
    fs::create_dir_all(mounts_dir).map_err(|source| Error::WriteSpec {
        path: mounts_dir.to_path_buf(),
        source,
    })?;
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("spec.json");
    let tmp = mounts_dir.join(format!(".{file}.tmp-{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|source| Error::WriteSpec {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| Error::WriteSpec {
        path: path.to_path_buf(),
        source,
    })
}

fn spec_paths_in(dir: &Path) -> io::Result<Vec<PathBuf>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ProviderId, ProviderMeta, ProviderName};

    fn linear_manifest() -> crate::provider::ProviderManifest {
        provider_manifest_from_wasm("omnifs_provider_linear.wasm")
    }

    fn github_manifest() -> crate::provider::ProviderManifest {
        provider_manifest_from_wasm("omnifs_provider_github.wasm")
    }

    fn provider_manifest_from_wasm(file: &str) -> crate::provider::ProviderManifest {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wasm32-wasip2/release")
            .join(file);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|source| panic!("read provider wasm {}: {source}", path.display()));
        crate::provider::read_provider_metadata_section(&bytes)
            .unwrap_or_else(|source| {
                panic!(
                    "extract provider metadata from {}: {source}",
                    path.display()
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "provider metadata missing from {}; run `just providers build`",
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
        let pat = auth.scheme("pat").expect("linear pat scheme");
        assert!(matches!(pat, crate::authn::AuthScheme::StaticToken(_)));
        let crate::authn::AuthScheme::StaticToken(static_token) = pat else {
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
        let pat = auth.scheme("pat").expect("github pat scheme");
        let crate::authn::AuthScheme::StaticToken(static_token) = pat else {
            panic!("expected static token");
        };
        assert_eq!(static_token.value_prefix, "Bearer ");
        let val = static_token.validation.as_ref().expect("validation");
        assert_eq!(val.method, "GET");
        assert_eq!(val.expect_status, 200);
    }

    #[test]
    fn thin_config_inherits_provider_metadata_defaults() {
        let manifest = linear_manifest();
        let mut cfg = spec_with_provider("linear", r#"{ "mount": "linear" }"#);

        cfg.apply_provider_metadata(&manifest).unwrap();

        assert_eq!(cfg.provider_name().as_str(), "linear");
        let auth = cfg
            .auth
            .as_ref()
            .expect("auth filled from manifest default");
        assert!(auth.is_oauth());
        assert_eq!(auth.scheme(), Some("oauth"));
    }

    #[test]
    fn loader_rejects_invalid_mount_name_in_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_spec(
            dir.path(),
            "bad.json",
            &spec_with_provider("p", r#"{ "mount": "Bad-Name", "config": {} }"#),
        );

        let registry = Registry::load(dir.path()).expect("scan succeeds");
        assert!(
            registry.iter().next().is_none(),
            "an invalid-name spec is never served"
        );
        assert!(matches!(
            registry.failures().first().map(|failure| &failure.error),
            Some(Error::MountName { .. })
        ));
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
                .get(&name::Name::new("alpha".to_owned()).unwrap())
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
    fn registry_rejects_filename_mount_mismatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mounts = dir.path();
        // Well-named spec: served.
        write_spec(
            mounts,
            "github.json",
            &spec_with_provider("github", r#"{ "mount": "github" }"#),
        );
        // A second file declaring the same mount (file stem != mount name): it is
        // rejected as a loud failure instead of silently shadowing the first or
        // leaving `rm github` to act on a non-existent `github.json`.
        write_spec(
            mounts,
            "github-backup.json",
            &spec_with_provider("github", r#"{ "mount": "github" }"#),
        );

        let registry = Registry::load(mounts).expect("scan succeeds");

        let names: Vec<_> = registry.iter().map(|(name, _)| name.to_string()).collect();
        assert_eq!(
            names,
            ["github"],
            "only the canonically-named file is served"
        );
        assert!(
            registry
                .failures()
                .iter()
                .any(|failure| matches!(failure.error, Error::FilenameMismatch { .. })),
            "the misnamed duplicate surfaces as a FilenameMismatch failure"
        );
    }

    #[test]
    fn registry_reload_reflects_on_disk_changes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mounts = dir.path();
        write_spec(
            mounts,
            "alpha.json",
            &spec_with_provider("alpha", r#"{ "mount": "alpha" }"#),
        );
        let mut registry = Registry::load(mounts).expect("scan succeeds");
        assert_eq!(
            registry
                .iter()
                .map(|(n, _)| n.to_string())
                .collect::<Vec<_>>(),
            ["alpha"]
        );

        // Add one spec and remove the original on disk, then reload the mirror.
        write_spec(
            mounts,
            "beta.json",
            &spec_with_provider("beta", r#"{ "mount": "beta" }"#),
        );
        std::fs::remove_file(mounts.join("alpha.json")).unwrap();
        registry.reload().expect("reload");

        assert_eq!(
            registry
                .iter()
                .map(|(n, _)| n.to_string())
                .collect::<Vec<_>>(),
            ["beta"],
            "reload drops removed specs and picks up added ones"
        );
    }

    #[test]
    fn registry_tolerates_missing_dir_and_derives_spec_path() {
        let registry = Registry::load("/no/such/mounts").expect("missing dir is not an error");
        assert!(registry.iter().next().is_none());
        let name = name::Name::new("github".to_owned()).unwrap();
        assert_eq!(
            registry.spec_path(&name),
            Path::new("/no/such/mounts/github.json")
        );
    }

    #[test]
    fn registry_put_writes_compact_spec_and_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        // The mounts directory does not exist yet: `put` must create it.
        let mounts = dir.path().join("mounts");
        let mut registry = Registry::load(&mounts).expect("load empty");

        let spec = spec_with_provider(
            "github",
            r#"{ "mount": "github", "auth": { "type": "static-token", "scheme": "pat" } }"#,
        );
        registry.put(&spec).expect("put");

        let path = mounts.join("github.json");
        let written = std::fs::read_to_string(&path).expect("spec written");
        // Compact authored shape: a lone auth entry stays a single object (not an
        // array), default `root_mount`/absent `capabilities`/`config` are omitted,
        // and the file ends in a trailing newline.
        assert!(written.ends_with("}\n"), "trailing newline: {written:?}");
        assert!(
            !written.contains("root_mount"),
            "default root_mount omitted: {written}"
        );
        assert!(
            !written.contains("capabilities"),
            "absent capabilities omitted: {written}"
        );
        let value: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert!(
            value["auth"].is_object(),
            "a single auth serializes as one object, got {}",
            value["auth"]
        );
        assert_eq!(value["auth"]["type"], "static-token");

        // The mirror and a fresh load both observe the written spec.
        let name = name::Name::new("github".to_owned()).unwrap();
        assert!(registry.get(&name).is_some());
        assert!(Registry::load(&mounts).unwrap().get(&name).is_some());

        // Remove clears both the file and the mirror; a second remove is Ok(false).
        assert!(registry.remove(&name).expect("remove"));
        assert!(registry.get(&name).is_none());
        assert!(!path.exists());
        assert!(
            !registry.remove(&name).expect("remove absent"),
            "removing an absent mount is Ok(false)"
        );
    }
}
