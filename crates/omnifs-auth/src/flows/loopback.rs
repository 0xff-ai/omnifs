use crate::callback::{LoopbackEndpoint, read_loopback_callback};
use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::flows::{authorization_url, exchange_code};
use crate::request::LoopbackLoginRequest;
use oauth2::{AuthorizationCode, PkceCodeChallenge};
use omnifs_workspace::creds::CredentialEntry;

impl OAuthClient {
    pub async fn login_loopback(
        &self,
        request: LoopbackLoginRequest,
    ) -> Result<CredentialEntry, AuthError> {
        let opener = self.opener.as_ref().ok_or(AuthError::MissingOpener)?;
        let endpoint = LoopbackEndpoint::bind(&request.flow.redirect_uri_template).await?;

        let client = request.oauth.client(endpoint.redirect_uri().clone())?;
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf_token) =
            authorization_url(&client, &request.oauth.scheme, pkce_challenge);

        opener.open(&auth_url).await?;
        let callback = read_loopback_callback(endpoint.into_listener()).await?;
        if callback.state.secret() != csrf_token.secret() {
            return Err(AuthError::StateMismatch);
        }

        exchange_code(
            &client,
            &self.http,
            AuthorizationCode::new(callback.code),
            pkce_verifier,
            &request.oauth.scheme,
        )
        .await
    }
}
