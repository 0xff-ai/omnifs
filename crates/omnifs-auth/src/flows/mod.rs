pub(crate) mod device;
pub(crate) mod implicit;
pub(crate) mod loopback;
pub(crate) mod manual;

use crate::error::AuthError;
use crate::request::ConfiguredClient;
use oauth2::{
    AuthorizationCode, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, Scope, TokenResponse,
};
use omnifs_workspace::authn::OauthScheme;
use omnifs_workspace::creds::CredentialEntry;
use secrecy::SecretString;
use time::OffsetDateTime;
use url::Url;

pub use device::DeviceCodePrompt;
pub use manual::ManualCode;

pub(crate) fn authorization_url(
    client: &ConfiguredClient,
    scheme: &OauthScheme,
    pkce_challenge: PkceCodeChallenge,
) -> (Url, CsrfToken) {
    let mut request = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(pkce_challenge)
        .add_scopes(scopes(scheme));
    for param in &scheme.extra_authorize_params {
        request = request.add_extra_param(&param.key, &param.value);
    }
    request.url()
}

pub(crate) fn scopes(scheme: &OauthScheme) -> impl Iterator<Item = Scope> + '_ {
    scheme.default_scopes.iter().cloned().map(Scope::new)
}

pub(crate) async fn exchange_code(
    client: &ConfiguredClient,
    http: &reqwest::Client,
    code: AuthorizationCode,
    pkce_verifier: PkceCodeVerifier,
    scheme: &OauthScheme,
) -> Result<CredentialEntry, AuthError> {
    let mut exchange = client.exchange_code(code).set_pkce_verifier(pkce_verifier);
    for param in &scheme.extra_token_params {
        exchange = exchange.add_extra_param(&param.key, &param.value);
    }
    let token = exchange.request_async(http).await?;
    Ok(credential_entry_from_token(&token))
}

pub(crate) fn credential_entry_from_token(
    token: &oauth2::StandardTokenResponse<
        oauth2::EmptyExtraTokenFields,
        oauth2::basic::BasicTokenType,
    >,
) -> CredentialEntry {
    let now = OffsetDateTime::now_utc();
    // Stamp the real expiry; the freshness margin is applied at check time in
    // `service::is_fresh`, not baked in here.
    let expires_at = token.expires_in().map(|expires_in| now + expires_in);
    let refresh_token = token
        .refresh_token()
        .map(|token| SecretString::from(token.secret().to_owned()));
    let scopes = token
        .scopes()
        .map(|s| s.iter().map(|sc| sc.as_ref().to_owned()).collect())
        .unwrap_or_default();
    let token_type = token.token_type().as_ref().to_owned();
    CredentialEntry::oauth(
        SecretString::from(token.access_token().secret().to_owned()),
        refresh_token,
        expires_at,
        token_type,
        scopes,
        now,
    )
}
