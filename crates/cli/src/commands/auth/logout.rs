//! Credential logout flow.

use omnifs_auth::{OAuthClient, RevokeOutcome};
use omnifs_creds::CredentialStore;
use omnifs_host::config::AuthConfig;

use super::shared::{load_mount, oauth_request, primary_auth, read_auth_manifest};
use crate::auth::AuthManifestView;
use crate::catalog::ProviderCatalog;
use crate::credential_target::CredentialTarget;
use crate::paths::Paths;

pub(super) async fn logout(
    _paths: &Paths,
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
    mount: &str,
    account: Option<&str>,
    revoke: bool,
) -> anyhow::Result<()> {
    let target = if let Ok((_mount, request, target)) = oauth_request(catalog, mount, account, &[])
    {
        if revoke
            && let Some(entry) = target.lookup(store)?
            && request.supports_revocation()
        {
            let client = OAuthClient::new()?;
            let outcome = client
                .revoke_access_token(request, entry.access_token().clone())
                .await?;
            if outcome == RevokeOutcome::Revoked {
                anstream::println!("Revoked remote token for `{mount}`");
            }
        }
        target
    } else {
        let mount_config = load_mount(catalog, mount)?;
        let auth = primary_auth(&mount_config.config);
        let manifest = read_auth_manifest(catalog, &mount_config.config)
            .ok()
            .flatten();
        let scheme = AuthManifestView::new(manifest.as_ref())
            .static_token_scheme_key(None, auth.and_then(AuthConfig::scheme))?;
        CredentialTarget::for_scheme(&mount_config.config, auth, &scheme, account)?
    };

    target.delete_from(store, mount)?;
    anstream::println!("Deleted credential for `{mount}`");
    Ok(())
}
