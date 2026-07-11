#![allow(clippy::disallowed_macros)] // migrates in wave 4 (cli-redesign)
//! Discovery helpers over the provider [`Catalog`] and the configured mounts.
//!
//! These wrap the CLI-facing shapes: the picker's installed-provider list, the
//! already-configured set, and lookups by name. Mount enumeration lives in
//! `Workspace::mounts()`. A spec carries its provider-manifest defaults from
//! creation time, so reading one needs no resolution step.

use std::collections::{BTreeMap, HashSet};

use omnifs_workspace::mounts::Name as MountName;
use omnifs_workspace::provider::{Catalog, Provider, ProviderManifest};

use crate::mount_config::MountConfig;

/// The latest installed artifact per provider name, each paired with its loaded
/// manifest, for the `mount add` and `setup` provider pickers. A corrupt artifact is
/// skipped with a warning rather than bricking enumeration.
pub(crate) fn installed_providers(
    catalog: &Catalog,
) -> anyhow::Result<Vec<(Provider, ProviderManifest)>> {
    let mut providers = Vec::new();
    for provider in catalog.installable()? {
        match provider.manifest() {
            Ok(manifest) => providers.push((provider, manifest)),
            Err(error) => {
                let name = &provider.meta.name;
                anstream::eprintln!(
                    "{}",
                    crate::style::warn(format!(
                        "skipping provider `{name}`: its embedded manifest failed to load; reinstall it. Re-run with `-vv` for details."
                    ))
                );
                tracing::debug!(provider = %name, error = ?error, "skipping provider with unreadable manifest");
            },
        }
    }
    Ok(providers)
}

/// Find an installed provider (and its loaded manifest) by its name slug, within
/// a list produced by [`installed_providers`].
pub(crate) fn find_installed<'a>(
    installed: &'a [(Provider, ProviderManifest)],
    name: &str,
) -> Option<&'a (Provider, ProviderManifest)> {
    installed
        .iter()
        .find(|(provider, _)| provider.meta.name.as_str() == name)
}

/// Map of provider name to the mount that already configures it, so the picker
/// can hide already-configured providers. Intersects the installable provider
/// names with the configured mount specs.
pub(crate) fn configured_mounts(
    catalog: &Catalog,
    mounts: &[MountConfig],
) -> anyhow::Result<BTreeMap<String, String>> {
    let installable: HashSet<String> = catalog
        .installable()?
        .iter()
        .map(|provider| provider.meta.name.to_string())
        .collect();
    let mut by_provider = BTreeMap::new();
    for configured in mounts {
        let provider_name = configured.config.provider_name().to_string();
        if installable.contains(&provider_name) {
            by_provider.insert(provider_name, configured.config.mount.clone());
        }
    }
    Ok(by_provider)
}

/// Returns `true` when a mount with `name` appears in `mounts`.
pub(crate) fn mount_exists(mounts: &[MountConfig], name: &MountName) -> bool {
    mounts.iter().any(|m| &m.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{wasm_with_metadata_section, wasm_with_provider_metadata};
    use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName};
    use omnifs_workspace::provider::ProviderStore;

    fn meta(name: &str) -> ProviderMeta {
        ProviderMeta {
            name: ProviderName::new(name).unwrap(),
            version: None,
        }
    }

    /// A corrupt artifact already in the store must not brick catalog
    /// enumeration: the valid providers alongside it still surface.
    #[test]
    fn installed_providers_skips_unreadable_artifact() {
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

        let providers = installed_providers(&Catalog::open(&providers_dir))
            .expect("a broken artifact must not fail catalog enumeration");

        assert!(
            providers
                .iter()
                .any(|(provider, _)| provider.meta.name.as_str() == "demo"),
            "the valid provider should surface despite the broken sibling"
        );
        assert!(
            !providers
                .iter()
                .any(|(provider, _)| provider.meta.name.as_str() == "broken"),
            "the broken provider should be skipped"
        );
    }
}
