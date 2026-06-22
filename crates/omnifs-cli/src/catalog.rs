//! Shared discovery for configured mounts and provider templates.
//!
//! `ProviderCatalog` owns `Spec`-to-`Resolved` resolution. `ProviderTemplates`
//! owns the indexed provider-template surface derived from built-in manifests
//! and provider wasm metadata. Mount enumeration (per-file specs from the
//! `mounts/` directory) lives in `Workspace::mounts()`; catalog surfaces that
//! need the list accept it as a parameter.

use omnifs_core::{MountName, ProviderRef};
use omnifs_mount::mounts::{Catalog as MountCatalog, Resolved, Spec};
use omnifs_provider::{AuthManifest, ProviderAuthManifest, ProviderManifest};
use std::collections::BTreeMap;
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

    /// The authoring/selection templates: one per provider name, drawn from the
    /// latest installed artifact in the content-addressed store. Replaces the
    /// former builtin-manifest index plus filename scan with the single store.
    pub(crate) fn provider_templates(&self) -> anyhow::Result<ProviderTemplates> {
        let mut by_name = BTreeMap::new();
        for provider in self.mounts.list()? {
            let name = provider.meta.name.clone();
            // Only the latest artifact per name surfaces as a template; older
            // retained versions are upgrade history, not authoring choices.
            let Some(latest) = self.mounts.latest_by_name(&name)? else {
                continue;
            };
            if latest.id != provider.id {
                continue;
            }
            // A corrupt artifact must not brick catalog enumeration; skip it
            // with a warning and let the rest resolve.
            let manifest = match provider.manifest() {
                Ok(manifest) => manifest,
                Err(error) => {
                    anstream::eprintln!(
                        "{}",
                        crate::style::warn(format!(
                            "skipping provider `{name}`: its embedded manifest failed to load; reinstall it. Re-run with `-vv` for details."
                        ))
                    );
                    tracing::debug!(provider = %name, error = ?error, "skipping provider with unreadable manifest");
                    continue;
                },
            };
            let auth_manifest = manifest.wasm_auth_manifest();
            by_name.insert(
                name.to_string(),
                ProviderTemplate {
                    reference: provider.reference(),
                    manifest,
                    auth_manifest,
                },
            );
        }
        Ok(ProviderTemplates::new(by_name))
    }

    pub(crate) fn provider_dir_status(&self) -> ProviderDirStatus {
        if !self.providers_dir.exists() {
            return ProviderDirStatus::Missing;
        }
        match self.mounts.store().read_index() {
            Ok(index) => ProviderDirStatus::Present {
                wasm_count: index.providers.len(),
            },
            Err(error) => ProviderDirStatus::Unreadable(error.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderTemplates {
    by_name: BTreeMap<String, ProviderTemplate>,
}

impl ProviderTemplates {
    fn new(by_name: BTreeMap<String, ProviderTemplate>) -> Self {
        Self { by_name }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Look up a template by its provider name slug (e.g. `github`).
    pub(crate) fn by_id(&self, name: &str) -> Option<&ProviderTemplate> {
        self.by_name.get(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &ProviderTemplate)> + '_ {
        self.by_name
            .iter()
            .map(|(name, template)| (name.as_str(), template))
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = &str> + '_ {
        self.by_name.keys().map(String::as_str)
    }

    /// Map of provider name to the mount that already configures it, so the
    /// picker can hide already-configured providers.
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
            if self.by_name.contains_key(&mount.provider_name) {
                by_provider.insert(mount.provider_name.clone(), mount.spec.mount);
            }
        }
        by_provider
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
    /// The pinned reference for this provider, written into a mount spec when
    /// the CLI authors a mount against this template.
    pub(crate) reference: ProviderRef,
    pub(crate) manifest: ProviderManifest,
    pub(crate) auth_manifest: Option<AuthManifest>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{wasm_with_metadata_section, wasm_with_provider_metadata};
    use omnifs_core::{ProviderId, ProviderMeta, ProviderName};
    use omnifs_mount::mounts::ProviderStore;

    fn meta(name: &str) -> ProviderMeta {
        ProviderMeta {
            name: ProviderName::new(name).unwrap(),
            version: None,
        }
    }

    /// A corrupt artifact already in the store must not brick catalog
    /// enumeration: the valid providers alongside it still surface.
    #[test]
    fn provider_templates_skips_unreadable_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let providers_dir = tmp.path().join("providers");
        let store = ProviderStore::new(&providers_dir);

        let good = wasm_with_provider_metadata("demo", "omnifs_provider_demo.wasm");
        let good_id = ProviderId::from_wasm_bytes(&good);
        store.put_if_absent(&good_id, &good).unwrap();
        store
            .install(good_id, meta("demo"), "omnifs_provider_demo.wasm".into())
            .unwrap();

        // An indexed artifact whose embedded manifest fails to validate, the
        // shape a provider built against a newer/older omnifs takes.
        let broken =
            wasm_with_metadata_section(br#"{"id":"x","displayName":"X","unknownField":true}"#);
        let broken_id = ProviderId::from_wasm_bytes(&broken);
        store.put_if_absent(&broken_id, &broken).unwrap();
        store
            .install(
                broken_id,
                meta("broken"),
                "omnifs_provider_broken.wasm".into(),
            )
            .unwrap();

        let templates = ProviderCatalog::for_providers(&providers_dir)
            .provider_templates()
            .expect("a broken artifact must not fail catalog enumeration");

        assert!(
            templates.by_id("demo").is_some(),
            "the valid provider should surface despite the broken sibling"
        );
        assert!(
            templates.by_id("broken").is_none(),
            "the broken provider should be skipped"
        );
    }
}
