//! Discovery helpers over the provider [`Catalog`] and the configured mounts.
//!
//! These wrap the CLI-facing shapes: the installed-provider selection list, the
//! already-configured set, and lookups by name. Mount enumeration lives in
//! `Workspace::mounts()`. A spec carries its provider-manifest defaults from
//! creation time, so reading one needs no resolution step.

use omnifs_workspace::mounts::Name as MountName;
use omnifs_workspace::provider::{Catalog, Provider, ProviderAuthManifest, ProviderManifest};

use crate::mount_config::MountConfig;

/// The latest installed artifact per provider name, each paired with its loaded
/// manifest, for the `mount add` and `setup` provider selections. A corrupt artifact is
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
                crate::ui::eprint_raw(&format!(
                    "{}\n",
                    crate::ui::style::warn(format!(
                        "skipping provider `{name}`: its embedded manifest failed to load; reinstall it. Re-run with `-vv` for details."
                    ))
                ));
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

/// One provider choice prepared for an interactive command. Terminal code
/// receives the value, label, and hint separately so it does not need to know
/// anything about provider manifests or credential policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderOption {
    pub(crate) name: String,
    pub(crate) hint: String,
    pub(crate) default_selected: bool,
}

/// Build the provider choices shared by `mount add` and `setup`.
pub(crate) fn provider_options(
    installed: &[(Provider, ProviderManifest)],
    configured: &std::collections::BTreeMap<String, String>,
) -> Vec<ProviderOption> {
    let mut options = installed
        .iter()
        .filter(|(provider, _)| !configured.contains_key(provider.meta.name.as_str()))
        .map(|(provider, manifest)| {
            let name = provider.meta.name.to_string();
            ProviderOption {
                hint: manifest
                    .description
                    .clone()
                    .unwrap_or_else(|| manifest.display_name.clone()),
                name,
                default_selected: default_selected(manifest),
            }
        })
        .collect::<Vec<_>>();
    options.sort_by(|a, b| {
        b.default_selected
            .cmp(&a.default_selected)
            .then_with(|| a.name.cmp(&b.name))
    });
    options
}

/// A provider is initially selected when setup can proceed without an
/// interactive config prompt or an unavailable ambient credential. OAuth is
/// intentionally considered selectable here because setup can complete its
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
        .map(ProviderAuthManifest::wasm_auth_manifest);
    !crate::commands::mount::detect::detect(auth_manifest.as_ref()).is_empty()
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

        let options = provider_options(&providers, &std::collections::BTreeMap::new());
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].name, "demo");
        assert!(options[0].default_selected);

        let configured = [("demo".to_string(), "demo".to_string())]
            .into_iter()
            .collect();
        assert!(provider_options(&providers, &configured).is_empty());
    }
}
