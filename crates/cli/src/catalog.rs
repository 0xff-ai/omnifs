//! Shared discovery for configured mounts and provider templates.

use crate::builtin_catalog::BuiltinManifestIndex;
use anyhow::{Context, anyhow};
use omnifs_host::config::{EffectiveConfig, InstanceConfig};
use omnifs_mount_schema::{AuthManifest, ProviderManifest};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use omnifs_model::ProviderId;

#[derive(Debug, Clone)]
pub(crate) struct ProviderCatalog {
    mounts_dir: PathBuf,
    providers_dir: PathBuf,
}

impl ProviderCatalog {
    pub(crate) fn new(mounts_dir: impl AsRef<Path>, providers_dir: impl AsRef<Path>) -> Self {
        Self {
            mounts_dir: mounts_dir.as_ref().to_path_buf(),
            providers_dir: providers_dir.as_ref().to_path_buf(),
        }
    }

    pub(crate) fn mounts_dir(&self) -> &Path {
        &self.mounts_dir
    }

    pub(crate) fn builtin_manifests() -> anyhow::Result<Vec<ProviderManifest>> {
        Ok(BuiltinManifestIndex::embedded()?.all_manifests().to_vec())
    }

    pub(crate) fn mount_config_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        mount_config_paths(&self.mounts_dir).with_context(|| {
            format!(
                "read mount config directory {}",
                self.mounts_dir().display()
            )
        })
    }

    pub(crate) fn load_mount(&self, config_path: &Path) -> anyhow::Result<LoadedMount> {
        let config = omnifs_host::config::mount_load::load_mount_config(config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        let config = self.into_effective_mount(config, true).with_context(|| {
            format!(
                "resolve effective mount config for {}",
                config_path.display()
            )
        })?;
        Ok(LoadedMount { config })
    }

    /// Resolve runtime-ready mount config, optionally requiring provider metadata.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn into_effective_mount(
        &self,
        mut config: InstanceConfig,
        require_metadata: bool,
    ) -> anyhow::Result<EffectiveConfig> {
        if require_metadata {
            self.apply_metadata(&mut config)?;
        } else {
            let _ = self.apply_metadata(&mut config);
        }
        let fallback_provider_id = Self::provider_id(&config).map_or_else(
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
            .into_effective(fallback_provider_id, None)
            .map_err(|error| anyhow!("{error}"))
    }

    pub(crate) fn provider_path(&self, config: &EffectiveConfig) -> PathBuf {
        crate::paths::provider_path_for(&self.providers_dir, &config.provider)
    }

    /// Read provider metadata from the on-disk WASM file when it exists and
    /// carries an embedded metadata section.
    ///
    /// Returns `Ok(None)` when the file is missing or the WASM has no metadata
    /// section; callers fall back to the built-in catalog.
    fn load_disk_provider_manifest(
        &self,
        provider: &str,
    ) -> anyhow::Result<Option<(PathBuf, ProviderManifest)>> {
        let path = crate::paths::provider_path_for(&self.providers_dir, provider);
        read_provider_metadata_file(&path).map(|manifest| manifest.map(|manifest| (path, manifest)))
    }

    /// Apply provider metadata to `config`, preferring metadata embedded in the
    /// provider file on disk and falling back to the built-in catalog when the
    /// provider is absent or carries no metadata section.
    pub(crate) fn apply_metadata(&self, config: &mut InstanceConfig) -> anyhow::Result<bool> {
        if let Some((path, manifest)) = self.load_disk_provider_manifest(&config.provider)? {
            config
                .apply_provider_metadata(&manifest)
                .with_context(|| format!("apply provider metadata from {}", path.display()))?;
            return Ok(true);
        }
        BuiltinManifestIndex::embedded()?.apply_metadata_to(config)
    }

    pub(crate) fn auth_manifest_for(
        &self,
        config: &EffectiveConfig,
    ) -> anyhow::Result<Option<AuthManifest>> {
        if let Some((_path, manifest)) = self.load_disk_provider_manifest(&config.provider)? {
            if let Some(auth) = manifest.wasm_auth_manifest() {
                return Ok(Some(auth));
            }
        }
        Ok(BuiltinManifestIndex::embedded()?.auth_manifest_for(config))
    }

    pub(crate) fn provider_templates(&self) -> anyhow::Result<BTreeMap<String, ProviderTemplate>> {
        let mut templates = BTreeMap::new();
        let index = BuiltinManifestIndex::embedded()?;
        for (manifest, auth_manifest) in index.manifest_auth_pairs() {
            templates.insert(
                manifest.id.clone(),
                ProviderTemplate {
                    source: ProviderSource::Builtin,
                    manifest: manifest.clone(),
                    auth_manifest,
                },
            );
        }

        let read = match fs::read_dir(&self.providers_dir) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(templates),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read {}", self.providers_dir.display()));
            },
        };

        let mut disk_ids = BTreeSet::new();
        for entry in read {
            let path = entry
                .with_context(|| format!("scan {}", self.providers_dir.display()))?
                .path();
            if path.extension().is_none_or(|ext| ext != "wasm") {
                continue;
            }

            let Some(manifest) = read_provider_metadata_file(&path)? else {
                continue;
            };
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("invalid provider file name {}", path.display()))?;
            if manifest.provider != file_name {
                anyhow::bail!(
                    "provider metadata in {} declares provider `{}`, expected `{file_name}`",
                    path.display(),
                    manifest.provider
                );
            }
            let id = manifest.id.clone();
            if !disk_ids.insert(id.clone()) {
                anyhow::bail!(
                    "duplicate provider metadata id in {}",
                    self.providers_dir.display()
                );
            }
            let auth_manifest = manifest.wasm_auth_manifest();
            templates.insert(
                id,
                ProviderTemplate {
                    source: ProviderSource::Disk(path),
                    manifest,
                    auth_manifest,
                },
            );
        }
        Ok(templates)
    }

    pub(crate) fn provider_dir_status(&self) -> ProviderDirStatus {
        let read = match fs::read_dir(&self.providers_dir) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return ProviderDirStatus::Missing;
            },
            Err(error) => {
                return ProviderDirStatus::Unreadable(error.into());
            },
        };

        let mut wasm_count = 0;
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return ProviderDirStatus::Unreadable(error.into()),
            };
            if entry.path().extension().is_some_and(|ext| ext == "wasm") {
                wasm_count += 1;
            }
        }
        ProviderDirStatus::Present { wasm_count }
    }

    /// Resolve the provider id for `config`: prefer applied provider metadata,
    /// then fall back to the provider file stem.
    pub(crate) fn provider_id(config: &InstanceConfig) -> anyhow::Result<ProviderId> {
        if let Some(id) = config.provider_id() {
            return ProviderId::new(id).with_context(|| format!("invalid provider id `{id}`"));
        }
        let stem = Path::new(&config.provider)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "cannot derive provider id from provider `{}`",
                    config.provider
                )
            })?;
        ProviderId::new(stem).with_context(|| format!("invalid provider id `{stem}`"))
    }
}

#[derive(Debug)]
pub(crate) enum ProviderDirStatus {
    Missing,
    Present { wasm_count: usize },
    Unreadable(anyhow::Error),
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedMount {
    pub(crate) config: EffectiveConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderTemplate {
    pub(crate) source: ProviderSource,
    pub(crate) manifest: ProviderManifest,
    pub(crate) auth_manifest: Option<AuthManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderSource {
    Builtin,
    Disk(PathBuf),
}

impl ProviderSource {
    pub(crate) fn sort_key(&self) -> u8 {
        match self {
            Self::Builtin => 0,
            Self::Disk(_) => 1,
        }
    }
}

fn mount_config_paths(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut files = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| mount_config_path(path))
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn mount_config_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn read_provider_metadata_file(path: &Path) -> anyhow::Result<Option<ProviderManifest>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    omnifs_mount_schema::read_provider_metadata_section(&bytes)
        .with_context(|| format!("extract provider metadata from {}", path.display()))
}
