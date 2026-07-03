use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::flows::{authorization_url, exchange_code};
use crate::request::ManualCodeLoginRequest;
use oauth2::{AuthorizationCode, CsrfToken, PkceCodeChallenge, RedirectUrl};
use omnifs_workspace::authn::PkceManualCodeConfig;
use omnifs_workspace::creds::CredentialEntry;
use std::future::Future;
use url::Url;

#[derive(Debug, Clone)]
pub struct ManualCode {
    pub code: String,
    pub state: CsrfToken,
}

impl ManualCode {
    pub fn new(code: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            state: CsrfToken::new(state.into()),
        }
    }
}

impl OAuthClient {
    pub async fn login_manual_code<F, Fut>(
        &self,
        request: ManualCodeLoginRequest,
        authorize: F,
    ) -> Result<CredentialEntry, AuthError>
    where
        F: FnOnce(Url) -> Fut,
        Fut: Future<Output = Result<ManualCode, AuthError>>,
    {
        let client = request.oauth.client(manual_redirect_uri(&request.flow)?)?;
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf_token) =
            authorization_url(&client, &request.oauth.scheme, pkce_challenge);

        let manual = authorize(auth_url).await?;
        if manual.state.secret() != csrf_token.secret() {
            return Err(AuthError::StateMismatch);
        }

        let entry = exchange_code(
            &client,
            &self.http,
            AuthorizationCode::new(manual.code),
            pkce_verifier,
            &request.oauth.scheme,
        )
        .await?;
        Ok(entry)
    }
}

fn manual_redirect_uri(config: &PkceManualCodeConfig) -> Result<RedirectUrl, AuthError> {
    Ok(RedirectUrl::new(config.redirect_uri.clone())?)
}
