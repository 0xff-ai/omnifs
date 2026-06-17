//! Shared discovery for configured mounts and provider templates.
//!
//! `ProviderCatalog` owns `Spec`-to-`Resolved` resolution and provider
//! template discovery. Mount enumeration (the merged `config.toml` inline
//! mounts + per-file specs) lives in `Workspace::mounts()`; methods here
//! that need the list accept it as a parameter.

use anyhow::{Context, anyhow};
use omnifs_core::MountName;
use omnifs_mount::mounts::{Catalog as MountCatalog, Resolved, Spec};
use omnifs_provider::{AuthManifest, ProviderAuthManifest, ProviderManifest};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{credential_target::CredentialTarget, session::MountConfig};

#[derive(Debug, Clone)]
pub(crate) struct ProviderCatalog {
    mounts: MountCatalog,
    providers_dir: PathBuf,
}

impl ProviderCatalog {
    pub(crate) fn for_dirs(mounts_dir: impl AsRef<Path>, providers_dir: impl AsRef<Path>) -> Self {
        let mounts_dir = mounts_dir.as_ref();
        let providers_dir = providers_dir.as_ref();
        Self {
            mounts: MountCatalog::new(mounts_dir, providers_dir),
            providers_dir: providers_dir.to_path_buf(),
        }
    }

    pub(crate) fn builtin_manifests() -> anyhow::Result<Vec<ProviderManifest>> {
        Ok(MountCatalog::builtin_manifests()?)
    }

    /// Build removal targets tolerantly, for use by `omnifs reset`.
    ///
    /// Unlike `mount_removal_targets`, this method enumerates the per-file
    /// spec paths directly and tolerates unparsable files: a broken JSON file
    /// still produces a removal target with `CredentialTarget::None` so reset
    /// can nuke broken state. Inline `config.toml` mounts are included first
    /// and resolved tolerantly too.
    pub(crate) fn reset_removal_targets(
        &self,
        inline_mounts: &[Spec],
        config_file: &std::path::Path,
    ) -> anyhow::Result<Vec<MountRemovalTarget>> {
        use omnifs_mount::mounts::Spec as MountSpec;

        let mut targets = Vec::new();

        // Inline config.toml mounts — tolerant resolve.
        for spec in inline_mounts {
            let name = spec.mount.clone();
            let credential = match self.resolve_mount_spec(spec.clone(), false) {
                Ok(resolved) => CredentialTarget::for_mount(&resolved),
                Err(error) => {
                    tracing::warn!(
                        config = %config_file.display(),
                        mount = name,
                        %error,
                        "unresolvable inline mount config; will remove the entry but cannot drop credentials"
                    );
                    CredentialTarget::None
                },
            };
            targets.push(MountRemovalTarget {
                name,
                path: config_file.to_path_buf(),
                credential,
            });
        }

        // Per-file specs — enumerate paths, parse tolerantly.
        let paths = crate::workspace::per_file_mount_paths(self.mounts.mounts_dir())?;
        for path in paths {
            let Some(name) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let credential = match MountSpec::from_file(&path) {
                Ok(spec) => match self.resolve_mount_spec(spec, false) {
                    Ok(resolved) => CredentialTarget::for_mount(&resolved),
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "unresolvable mount config; will remove the file but cannot drop credentials"
                        );
                        CredentialTarget::None
                    },
                },
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "unparsable mount config; will remove the file but cannot drop credentials"
                    );
                    CredentialTarget::None
                },
            };
            targets.push(MountRemovalTarget {
                name,
                path,
                credential,
            });
        }

        targets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(targets)
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

    /// Apply provider metadata to `config`, preferring metadata embedded in the
    /// provider file on disk and falling back to the built-in catalog when the
    /// provider is absent or carries no metadata section.
    pub(crate) fn apply_metadata(&self, spec: &mut Spec) -> anyhow::Result<bool> {
        self.mounts.apply_metadata(spec).map_err(Into::into)
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

    pub(crate) fn provider_templates(&self) -> anyhow::Result<BTreeMap<String, ProviderTemplate>> {
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
}

/// Returns `true` when a mount with `name` appears in `mounts`.
pub(crate) fn mount_exists(mounts: &[MountConfig], name: &MountName) -> bool {
    mounts.iter().any(|m| &m.name == name)
}

#[derive(Debug, Clone)]
pub(crate) struct MountRemovalTarget {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) credential: CredentialTarget,
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

        let catalog = ProviderCatalog::for_dirs(tmp.path().join("mounts"), &providers_dir);
        let templates = catalog
            .provider_templates()
            .expect("a broken disk provider must not fail catalog enumeration");

        assert!(
            matches!(
                templates.get("demo").map(|t| &t.source),
                Some(ProviderSource::Disk(_))
            ),
            "the valid disk provider should load despite the broken sibling"
        );
    }
}
