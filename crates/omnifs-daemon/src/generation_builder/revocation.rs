//! Prepares an OAuth credential's upstream revocation before it is removed
//! from durable state: resolves the pinned provider scheme, verifies the
//! stored credential's auth fingerprint still matches it, and builds the
//! revoke request when the scheme declares a revocation endpoint.

use anyhow::Context as _;
use omnifs_auth::{AuthKind, AuthScheme, OAuthClient, OAuthRequest};
use omnifs_state::{StateStore, StoredCredential};
use secrecy::SecretString;

use super::{decode_entry, runtime_overrides};
use crate::auth_fingerprint::auth_fingerprint;
use crate::credential_codec::decode_payload;

pub(crate) enum PreparedCredentialRevocation {
    LocalOnly,
    Remote {
        client: OAuthClient,
        request: Box<OAuthRequest>,
        access_token: SecretString,
    },
}

impl PreparedCredentialRevocation {
    pub(crate) async fn revoke(self) -> anyhow::Result<()> {
        match self {
            Self::LocalOnly => Ok(()),
            Self::Remote {
                client,
                request,
                access_token,
            } => {
                client
                    .revoke_access_token(*request, access_token)
                    .await
                    .context("revoke OAuth access token")?;
                Ok(())
            },
        }
    }
}

pub(crate) async fn prepare_credential_revocation(
    state: &StateStore,
    stored: &StoredCredential,
) -> anyhow::Result<PreparedCredentialRevocation> {
    if stored.summary.kind != AuthKind::OAuth {
        return Ok(PreparedCredentialRevocation::LocalOnly);
    }
    let provider = state
        .load_provider_metadata(stored.summary.provider)
        .await?
        .with_context(|| format!("provider {} is not retained", stored.summary.provider))?;
    let scheme = provider
        .manifest
        .auth
        .as_ref()
        .and_then(|manifest| manifest.scheme(stored.summary.id.scheme()))
        .with_context(|| {
            format!(
                "provider declares no auth scheme `{}`",
                stored.summary.id.scheme()
            )
        })?;
    let payload = decode_payload(stored.material.expose())?;
    let entry = decode_entry(&payload.material)?;
    let expected_fingerprint =
        auth_fingerprint(stored.summary.provider, scheme, &payload.overrides)?;
    let AuthScheme::Oauth(scheme) = scheme else {
        anyhow::bail!("stored OAuth credential no longer matches its provider scheme")
    };
    anyhow::ensure!(
        expected_fingerprint == stored.summary.auth_fingerprint,
        "stored OAuth runtime does not match its provider"
    );
    if scheme.revocation_endpoint.is_none() {
        return Ok(PreparedCredentialRevocation::LocalOnly);
    }
    let request =
        OAuthRequest::from_runtime(scheme.clone(), runtime_overrides(&payload.overrides)?)?;
    Ok(PreparedCredentialRevocation::Remote {
        client: OAuthClient::new().context("create OAuth revocation client")?,
        request: Box::new(request),
        access_token: entry.access_token().clone(),
    })
}
