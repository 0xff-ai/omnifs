//! Shared discovery for configured mounts and provider templates.
//!
//! `ProviderCatalog` owns `Spec`-to-`Resolved` resolution. `ProviderTemplates`
//! owns the indexed provider-template surface derived from built-in manifests
//! and provider wasm metadata. Mount enumeration (per-file specs from the
//! `mounts/` directory) lives in `Workspace::mounts()`; catalog surfaces that
//! need the list accept it as a parameter.

use anyhow::{Context, anyhow};
use omnifs_core::MountName;
use omnifs_mount::mounts::{Catalog as MountCatalog, Resolved, Spec};
use omnifs_provider::{AuthManifest, ProviderAuthManifest, ProviderManifest};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::session::MountConfig;

#[derive(Debug, Clone)]
pub(crate) struct ProviderCatalog {
    mounts: MountCatalog,
    providers_dir: PathBuf,
}

impl ProviderCatalog {
    pub(crate) fn for_providers(providers_dir: impl AsRef<Path>) -> Self {
        let providers_dir = providers_dir.as_ref();
        Self {
            mounts: MountCatalog::for_providers(providers_dir),
            providers_dir: providers_dir.to_path_buf(),
        }
    }

    pub(crate) fn builtin_manifests() -> anyhow::Result<Vec<ProviderManifest>> {
        Ok(MountCatalog::builtin_manifests()?)
    }

    /// The underlying mount catalog, for callers that drive the shared
    /// materializer (`omnifs_mount::materialize`) directly.
    pub(crate) fn inner(&self) -> &MountCatalog {
        &self.mounts
    }

    /// Resolve runtime-ready mount, optionally requiring provider metadata.
    pub(crate) fn resolve_mount_spec(
        &self,
        spec: Spec,
        require_metadata: bool,
    ) -> anyhow::Result<Resolved> {
        self.mounts
            .resolve_spec(spec, require_metadata)
            .map_err(Into::into)
    }

    pub(crate) fn provider_path(&self, mount: &Resolved) -> PathBuf {
        self.mounts.provider_path(mount)
    }

    pub(crate) fn auth_manifest_for(
        &self,
        mount: &Resolved,
    ) -> anyhow::Result<Option<AuthManifest>> {
        self.mounts.auth_manifest_for(mount).map_err(Into::into)
    }

    pub(crate) fn provider_auth_manifest_for(
        &self,
        mount: &Resolved,
    ) -> anyhow::Result<Option<ProviderAuthManifest>> {
        self.mounts
            .provider_auth_manifest_for(mount)
            .map_err(Into::into)
    }

    pub(crate) fn provider_templates(&self) -> anyhow::Result<ProviderTemplates> {
        let mut templates = BTreeMap::new();
        for manifest in MountCatalog::builtin_manifests()? {
            let auth_manifest = manifest.wasm_auth_manifest();
            templates.insert(
                manifest.id.clone(),
                ProviderTemplate {
                    source: ProviderSource::Builtin,
                    manifest,
                    auth_manifest,
                },
            );
        }

        let read = match fs::read_dir(&self.providers_dir) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ProviderTemplates::new(templates));
            },
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

            // A single incompatible disk provider (e.g. one built against a newer
            // manifest shape) must not brick every command that enumerates the
            // catalog. Skip it with a warning; builtins still resolve.
            let manifest = match read_provider_metadata_file(&path) {
                Ok(Some(manifest)) => manifest,
                Ok(None) => continue,
                Err(error) => {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("<unknown>");
                    anstream::eprintln!(
                        "{}",
                        crate::style::warn(format!(
                            "skipping provider `{name}`: its metadata failed to parse (likely built against a newer omnifs); rebuild or remove it. Re-run with `-vv` for details."
                        ))
                    );
                    tracing::debug!(provider = %path.display(), error = ?error, "skipping provider with unreadable metadata");
                    continue;
                },
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
        Ok(ProviderTemplates::new(templates))
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
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderTemplates {
    by_id: BTreeMap<String, ProviderTemplate>,
    by_provider_file: BTreeMap<String, String>,
}

impl ProviderTemplates {
    fn new(by_id: BTreeMap<String, ProviderTemplate>) -> Self {
        let mut by_provider_file = BTreeMap::new();
        for (id, template) in &by_id {
            by_provider_file
                .entry(template.manifest.provider.clone())
                .or_insert_with(|| id.clone());
        }
        Self {
            by_id,
            by_provider_file,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub(crate) fn by_id(&self, id: &str) -> Option<&ProviderTemplate> {
        self.by_id.get(id)
    }

    pub(crate) fn by_provider_file(
        &self,
        provider_file: &str,
    ) -> Option<(&str, &ProviderTemplate)> {
        let id = self.by_provider_file.get(provider_file)?;
        self.by_id_entry(id)
    }

    pub(crate) fn by_reference(&self, provider_ref: &str) -> Option<(&str, &ProviderTemplate)> {
        self.by_id_entry(provider_ref)
            .or_else(|| self.by_provider_file(provider_ref))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &ProviderTemplate)> + '_ {
        self.by_id
            .iter()
            .map(|(id, template)| (id.as_str(), template))
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = &str> + '_ {
        self.by_id.keys().map(String::as_str)
    }

    pub(crate) fn configured_mounts(
        &self,
        catalog: &ProviderCatalog,
        mounts: &[MountConfig],
    ) -> BTreeMap<String, String> {
        let mut by_provider = BTreeMap::new();
        for configured in mounts {
            let mount = match catalog.resolve_mount_spec(configured.config.clone(), true) {
                Ok(mount) => mount,
                Err(error) => {
                    tracing::warn!(source = %configured.source.display(), %error, "skipping unparsable mount config");
                    continue;
                },
            };
            if let Some((id, _)) = self.by_resolved_mount(&mount) {
                by_provider.insert(id.to_owned(), mount.spec.mount);
            }
        }
        by_provider
    }

    fn by_id_entry(&self, id: &str) -> Option<(&str, &ProviderTemplate)> {
        self.by_id
            .get_key_value(id)
            .map(|(id, template)| (id.as_str(), template))
    }

    fn by_resolved_mount(&self, mount: &Resolved) -> Option<(&str, &ProviderTemplate)> {
        self.by_id_entry(&mount.provider_id)
            .or_else(|| self.by_provider_file(&mount.spec.provider))
    }
}

/// Returns `true` when a mount with `name` appears in `mounts`.
pub(crate) fn mount_exists(mounts: &[MountConfig], name: &MountName) -> bool {
    mounts.iter().any(|m| &m.name == name)
}

#[derive(Debug)]
pub(crate) enum ProviderDirStatus {
    Missing,
    Present { wasm_count: usize },
    Unreadable(anyhow::Error),
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

fn read_provider_metadata_file(path: &Path) -> anyhow::Result<Option<ProviderManifest>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    omnifs_provider::read_provider_metadata_section(&bytes)
        .with_context(|| format!("extract provider metadata from {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{wasm_with_metadata_section, wasm_with_provider_metadata};

    /// A single incompatible disk provider must not brick catalog enumeration:
    /// the valid providers alongside it still load.
    #[test]
    fn provider_templates_skips_unparseable_disk_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let providers_dir = tmp.path().join("providers");
        std::fs::create_dir_all(&providers_dir).unwrap();
        std::fs::write(
            providers_dir.join("omnifs_provider_demo.wasm"),
            wasm_with_provider_metadata("demo", "omnifs_provider_demo.wasm"),
        )
        .unwrap();
        // Metadata section present but holding a manifest that fails validation —
        // the shape a provider built against a newer/older omnifs takes (distinct
        // from a wasm with no metadata section, which is skipped silently).
        std::fs::write(
            providers_dir.join("omnifs_provider_broken.wasm"),
            wasm_with_metadata_section(br#"{"id":"x","displayName":"X","unknownField":true}"#),
        )
        .unwrap();

        let catalog = ProviderCatalog::for_providers(&providers_dir);
        let templates = catalog
            .provider_templates()
            .expect("a broken disk provider must not fail catalog enumeration");

        assert!(
            matches!(
                templates.by_id("demo").map(|t| &t.source),
                Some(ProviderSource::Disk(_))
            ),
            "the valid disk provider should load despite the broken sibling"
        );
        assert_eq!(
            templates
                .by_provider_file("omnifs_provider_demo.wasm")
                .map(|(id, _)| id),
            Some("demo")
        );
    }
}
