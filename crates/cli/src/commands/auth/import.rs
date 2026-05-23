//! Static token import and OAuth maintenance commands.

use anyhow::anyhow;
use omnifs_auth::OAuthClient;
use omnifs_creds::{CredentialEntry, CredentialStore};
use omnifs_host::config::AuthConfig;
use secrecy::SecretString;
use time::OffsetDateTime;

use super::shared::{format_rfc3339, load_mount, oauth_request, primary_auth, read_auth_manifest};
use crate::auth::AuthManifestView;
use crate::catalog::ProviderCatalog;
use crate::credential_target::CredentialTarget;
use crate::paths::Paths;

pub(super) async fn refresh(
    _paths: &Paths,
    catalog: &ProviderCatalog,
    store: Box<dyn CredentialStore>,
    mount: &str,
    account: Option<&str>,
) -> anyhow::Result<()> {
    let (_mount, request, target) = oauth_request(catalog, mount, account, &[])?;
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
    store: &dyn CredentialStore,
    mount: &str,
    account: Option<&str>,
) -> anyhow::Result<()> {
    let (_mount, request, target) = oauth_request(catalog, mount, account, &[])?;
    let entry = target.lookup(store)?;
    anstream::println!("declared: {}", request.scheme().default_scopes.join(", "));
    match entry {
        Some(entry) => anstream::println!("granted: {}", entry.scopes().join(", ")),
        None => anstream::println!("granted: <no stored credential>"),
    }
    Ok(())
}

pub(super) fn schemes(catalog: &ProviderCatalog, mount: &str) -> anyhow::Result<()> {
    let mount = load_mount(catalog, mount)?;
    match read_auth_manifest(catalog, &mount.config)? {
        Some(manifest) => {
            anstream::println!("{}", serde_json::to_string_pretty(&manifest)?);
        },
        None => anstream::println!("no auth schemes declared"),
    }
    Ok(())
}

pub(crate) fn run_auth_manifest(catalog: &ProviderCatalog, mount: &str) -> anyhow::Result<()> {
    schemes(catalog, mount)
}

pub(super) fn import_static_token_value(
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
    mount: &str,
    token: SecretString,
    scheme: Option<&str>,
    account: Option<&str>,
) -> anyhow::Result<()> {
    let mount_config = load_mount(catalog, mount)?;
    let auth = primary_auth(&mount_config.config);
    let manifest = read_auth_manifest(catalog, &mount_config.config)
        .ok()
        .flatten();
    let scheme = AuthManifestView::new(manifest.as_ref())
        .static_token_scheme_key(scheme, auth.and_then(AuthConfig::scheme))?;
    let target = CredentialTarget::for_scheme(&mount_config.config, auth, &scheme, account)?;
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
