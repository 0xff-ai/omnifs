//! Static token import and OAuth maintenance commands.

use anyhow::anyhow;
use omnifs_auth::OAuthClient;
use omnifs_creds::{CredentialEntry, CredentialStore};
use secrecy::SecretString;
use time::OffsetDateTime;

use super::shared::format_rfc3339;
use crate::catalog::ProviderCatalog;
use crate::paths::Paths;
use crate::session::MountConfig;

pub(super) async fn refresh(
    _paths: &Paths,
    catalog: &ProviderCatalog,
    mounts: &[MountConfig],
    store: Box<dyn CredentialStore>,
    mount: &str,
    account: Option<&str>,
) -> anyhow::Result<()> {
    let mount_auth = catalog.load_mount_auth(mounts, mount)?;
    let (request, target) = mount_auth.oauth_request(account, &[])?;
    let entry = target
        .lookup(store.as_ref())?
        .ok_or_else(|| anyhow!("no stored OAuth credential for `{mount}`"))?;
    let refresh = entry
        .refresh_token()
        .ok_or_else(|| anyhow!("stored credential for `{mount}` has no refresh token"))?;
    let entry = OAuthClient::new()?.refresh(request, refresh).await?;
    for key in target.keys() {
        store.put(key, &entry)?;
    }
    anstream::println!(
        "Refreshed `{mount}`; expires_at={}",
        entry
            .expires_at()
            .map_or_else(|| "unknown".to_owned(), format_rfc3339)
    );
    Ok(())
}

pub(super) fn scopes(
    _paths: &Paths,
    catalog: &ProviderCatalog,
    mounts: &[MountConfig],
    store: &dyn CredentialStore,
    mount: &str,
    account: Option<&str>,
) -> anyhow::Result<()> {
    let mount_auth = catalog.load_mount_auth(mounts, mount)?;
    let (request, target) = mount_auth.oauth_request(account, &[])?;
    let entry = target.lookup(store)?;
    anstream::println!("declared: {}", request.scheme().default_scopes.join(", "));
    match entry {
        Some(entry) => anstream::println!("granted: {}", entry.scopes().join(", ")),
        None => anstream::println!("granted: <no stored credential>"),
    }
    Ok(())
}

pub(super) fn import_static_token_value(
    catalog: &ProviderCatalog,
    mounts: &[MountConfig],
    store: &dyn CredentialStore,
    mount: &str,
    token: SecretString,
    scheme: Option<&str>,
    account: Option<&str>,
) -> anyhow::Result<()> {
    let mount_config = catalog.load_mount_auth_tolerating_manifest_errors(mounts, mount)?;
    let target = mount_config.static_token_target(scheme, account)?;
    let key = target
        .primary_key()
        .expect("credential target for scheme is internal");

    let entry = CredentialEntry::static_token(token, OffsetDateTime::now_utc());
    for key in target.keys() {
        store.put(key, &entry)?;
    }
    anstream::println!(
        "Imported static token for `{mount}` as provider={} scheme={} account={}",
        key.provider_id(),
        key.scheme(),
        key.account()
    );
    Ok(())
}
