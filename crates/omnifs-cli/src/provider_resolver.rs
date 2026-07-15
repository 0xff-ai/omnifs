//! Exact provider selection for mount creation.
//!
//! A selector is either a local artifact path, an embedded provider name, or
//! a lowercase digest prefix. Resolution always ends at one validated
//! `ProviderRef` and its manifest. Provider names never select retained
//! artifacts by recency.

use anyhow::{Context as _, anyhow, bail};
use omnifs_workspace::ids::{ProviderId, ProviderRef};
use omnifs_workspace::provider::{Catalog, ProviderManifest, ProviderStore};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::mount_config::MountConfig;
use crate::provider_bundle::EmbeddedProviders;

pub(crate) struct ResolvedProvider {
    pub(crate) reference: ProviderRef,
    pub(crate) manifest: ProviderManifest,
}

pub(crate) struct ProviderResolver<'a> {
    store: ProviderStore,
    catalog: Catalog,
    embedded: &'a EmbeddedProviders,
}

impl<'a> ProviderResolver<'a> {
    pub(crate) fn new(providers_dir: &Path, embedded: &'a EmbeddedProviders) -> Self {
        Self {
            store: ProviderStore::new(providers_dir),
            catalog: Catalog::open(providers_dir),
            embedded,
        }
    }

    pub(crate) fn resolve(&self, selector: &str) -> anyhow::Result<ResolvedProvider> {
        let path = Path::new(selector);
        match fs::symlink_metadata(path) {
            Ok(metadata) => return self.resolve_path(path, metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error).with_context(|| format!("stat provider `{selector}`")),
        }

        if let Some(provider) = self.embedded.by_name(selector) {
            return self.resolve_artifact(provider.artifact());
        }
        if is_digest_prefix(selector) {
            return self.resolve_digest(selector);
        }
        bail!(
            "provider selector `{selector}` is not an existing WASM path, embedded provider name, or lowercase digest prefix"
        )
    }

    fn resolve_path(
        &self,
        path: &Path,
        metadata: fs::Metadata,
    ) -> anyhow::Result<ResolvedProvider> {
        if metadata.is_dir() {
            let wasm_files = fs::read_dir(path)
                .with_context(|| format!("read provider directory {}", path.display()))?
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| format!("read provider directory {}", path.display()))?
                .into_iter()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "wasm"))
                .map(|entry| {
                    let path = entry.path();
                    let metadata = fs::symlink_metadata(&path)
                        .with_context(|| format!("stat provider artifact {}", path.display()))?;
                    if !metadata.file_type().is_file() {
                        bail!("provider artifact {} is not a regular file", path.display());
                    }
                    Ok(path)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let [wasm] = wasm_files.as_slice() else {
                bail!(
                    "provider directory {} must contain exactly one regular `.wasm` file",
                    path.display()
                );
            };
            return self.resolve_file(wasm);
        }
        if !metadata.file_type().is_file() {
            bail!(
                "provider path {} is not a regular file or directory",
                path.display()
            );
        }
        self.resolve_file(path)
    }

    fn resolve_file(&self, path: &Path) -> anyhow::Result<ResolvedProvider> {
        let artifact = omnifs_workspace::provider::Artifact::from_file(path)
            .with_context(|| format!("validate provider artifact {}", path.display()))?;
        self.resolve_artifact(&artifact)
    }

    fn resolve_digest(&self, selector: &str) -> anyhow::Result<ResolvedProvider> {
        let index = self.store.read_index()?;
        let mut ids = BTreeMap::<String, ProviderId>::new();
        for entry in &index.providers {
            let id = entry.id.to_string();
            if id.starts_with(selector) {
                ids.insert(id, entry.id);
            }
        }
        for entry in self.embedded.entries() {
            let id = entry.artifact().id().to_string();
            if id.starts_with(selector) {
                ids.insert(id, entry.artifact().id());
            }
        }
        let matches = ids.into_values().collect::<Vec<_>>();
        let id = match matches.as_slice() {
            [id] => *id,
            [] => bail!(
                "provider digest prefix `{selector}` did not match a retained or embedded artifact"
            ),
            _ => bail!("provider digest prefix `{selector}` is ambiguous"),
        };
        if index.providers.iter().any(|entry| entry.id == id) {
            return self.resolve_id(&id);
        }
        let embedded = self
            .embedded
            .by_id(&id)
            .ok_or_else(|| anyhow!("embedded provider `{id}` disappeared during resolution"))?;
        self.resolve_artifact(embedded.artifact())
    }

    fn resolve_artifact(
        &self,
        artifact: &omnifs_workspace::provider::Artifact,
    ) -> anyhow::Result<ResolvedProvider> {
        let entry = self.store.retain(artifact)?;
        self.resolve_id(&entry.id)
    }

    fn resolve_id(&self, id: &ProviderId) -> anyhow::Result<ResolvedProvider> {
        let provider = self
            .catalog
            .get(id)?
            .ok_or_else(|| anyhow!("provider artifact `{id}` is missing from the store"))?;
        let manifest = provider.manifest()?;
        Ok(ResolvedProvider {
            reference: provider.reference(),
            manifest,
        })
    }
}

fn is_digest_prefix(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One provider choice prepared for `mount add`. Terminal code receives the
/// value, label, and hint separately so it does not know manifest policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderOption {
    pub(crate) name: String,
    pub(crate) hint: String,
    pub(crate) default_selected: bool,
}

pub(crate) fn provider_options(
    embedded: &EmbeddedProviders,
    configured: &BTreeMap<String, String>,
) -> Vec<ProviderOption> {
    let mut options = embedded
        .entries()
        .iter()
        .filter(|entry| !configured.contains_key(&entry.manifest().id))
        .map(|entry| ProviderOption {
            name: entry.manifest().id.clone(),
            hint: entry
                .manifest()
                .description
                .clone()
                .unwrap_or_else(|| entry.manifest().display_name.clone()),
            default_selected: default_selected(entry.manifest()),
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| {
        right
            .default_selected
            .cmp(&left.default_selected)
            .then_with(|| left.name.cmp(&right.name))
    });
    options
}

/// A provider is initially selected when mount creation can proceed without an
/// interactive config prompt or an unavailable ambient credential. OAuth is
/// intentionally considered selectable here because an interactive mount can complete its
/// browser flow interactively; `--yes` keeps its stricter ambient-only policy.
fn default_selected(manifest: &ProviderManifest) -> bool {
    if manifest.requires_mount_input() {
        return false;
    }
    if manifest.auth.is_none() {
        return true;
    }
    if matches!(
        manifest
            .auth
            .as_ref()
            .and_then(|auth| auth.default_scheme()),
        Some((_, omnifs_workspace::authn::AuthScheme::Oauth(_)))
    ) {
        return true;
    }
    let auth_manifest = manifest
        .auth
        .as_ref()
        .map(omnifs_workspace::provider::ProviderAuthManifest::wasm_auth_manifest);
    !crate::commands::mount::detect::detect(auth_manifest.as_ref()).is_empty()
}

/// Returns `true` when setup can configure a provider under `--yes` without
/// asking for mount input or starting an interactive authentication flow.
pub(crate) fn safe_for_setup(manifest: &ProviderManifest) -> bool {
    if manifest.requires_mount_input() {
        return false;
    }
    if manifest.auth.is_none() {
        return true;
    }
    let auth_manifest = manifest
        .auth
        .as_ref()
        .map(omnifs_workspace::provider::ProviderAuthManifest::wasm_auth_manifest);
    !crate::commands::mount::detect::detect(auth_manifest.as_ref()).is_empty()
}

/// Returns `true` when a mount with `name` appears in `mounts`.
pub(crate) fn mount_exists(mounts: &[MountConfig], name: &omnifs_workspace::mounts::Name) -> bool {
    mounts.iter().any(|mount| &mount.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_prefix_accepts_only_lowercase_hex() {
        assert!(is_digest_prefix("abc123"));
        assert!(is_digest_prefix(&"a".repeat(64)));
        assert!(!is_digest_prefix(""));
        assert!(!is_digest_prefix("ABC123"));
        assert!(!is_digest_prefix(&"a".repeat(65)));
    }
}
