//! Shared auth command helpers.

use omnifs_auth::OAuthRequest;
use omnifs_host::config::{AuthConfig, EffectiveConfig};
use omnifs_mount_schema::AuthManifest;

use crate::auth_mount::MountAuth;
use crate::catalog::ProviderCatalog;
use crate::credential_target::CredentialTarget;

#[derive(Debug, Clone)]
pub(super) struct MountConfig {
    pub(super) config: EffectiveConfig,
}

pub(super) fn load_mount(catalog: &ProviderCatalog, mount: &str) -> anyhow::Result<MountConfig> {
    let loaded = MountAuth::load(catalog, mount)?;
    Ok(MountConfig {
        config: loaded.config,
    })
}

pub(super) fn load_all_mounts(catalog: &ProviderCatalog) -> anyhow::Result<Vec<MountConfig>> {
    let mut mounts = catalog
        .mount_config_paths()?
        .into_iter()
        .map(|path| {
            catalog.load_mount(&path).map(|loaded| MountConfig {
                config: loaded.config,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    mounts.sort_by(|a, b| a.config.mount.cmp(&b.config.mount));
    Ok(mounts)
}

pub(super) fn read_auth_manifest(
    catalog: &ProviderCatalog,
    config: &EffectiveConfig,
) -> anyhow::Result<Option<AuthManifest>> {
    catalog.auth_manifest_for(config)
}

pub(super) fn primary_auth(config: &EffectiveConfig) -> Option<&AuthConfig> {
    config
        .auth
        .iter()
        .find(|auth| auth.is_oauth())
        .or_else(|| config.auth.first())
}

pub(super) fn oauth_request(
    catalog: &ProviderCatalog,
    mount: &str,
    account: Option<&str>,
    scopes: &[String],
) -> anyhow::Result<(MountConfig, OAuthRequest, CredentialTarget)> {
    let loaded = MountAuth::load(catalog, mount)?;
    let (request, target) = loaded.oauth_request(catalog, account, scopes)?;
    Ok((
        MountConfig {
            config: loaded.config,
        },
        request,
        target,
    ))
}

pub(super) fn format_scopes(scopes: &[String]) -> String {
    if scopes.is_empty() {
        "<none>".to_owned()
    } else {
        scopes.join(", ")
    }
}

pub(super) fn format_rfc3339(value: time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.to_string())
}
