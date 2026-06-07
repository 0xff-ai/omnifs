//! Credential logout flow.

use omnifs_auth::{OAuthClient, RevokeOutcome};
use omnifs_creds::CredentialStore;

use crate::catalog::ProviderCatalog;
use crate::paths::Paths;

pub(super) async fn logout(
    _paths: &Paths,
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
    mount: &str,
    account: Option<&str>,
    revoke: bool,
) -> anyhow::Result<()> {
    let mount_auth = catalog.load_mount_auth_tolerating_manifest_errors(mount)?;
    let target = if let Ok((request, target)) = mount_auth.oauth_request(account, &[]) {
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
        mount_auth.static_token_target(None, account)?
    };

    target.delete_from(store, mount)?;
    anstream::println!("Deleted credential for `{mount}`");
    Ok(())
}
