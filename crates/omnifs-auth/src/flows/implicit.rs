use crate::callback::{LoopbackEndpoint, read_client_side_callback};
use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::flows::{credential_entry_from_token, scopes};
use crate::request::{ClientSideTokenLoginRequest, ConfiguredAuthorizationClient};
use oauth2::CsrfToken;
use omnifs_workspace::authn::OauthScheme;
use omnifs_workspace::creds::CredentialEntry;
use url::Url;

impl OAuthClient {
    pub async fn login_client_side_token(
        &self,
        request: ClientSideTokenLoginRequest,
    ) -> Result<CredentialEntry, AuthError> {
        let opener = self.opener.as_ref().ok_or(AuthError::MissingOpener)?;
        let endpoint = LoopbackEndpoint::bind(&request.flow.redirect_uri_template).await?;
        let client = request
            .oauth
            .authorization_client(endpoint.redirect_uri().clone())?;
        let (auth_url, csrf_token) = implicit_authorization_url(&client, &request.oauth.scheme);

        opener.open(&auth_url).await?;
        let callback = read_client_side_callback(endpoint.into_listener()).await?;
        if callback.state.secret() != csrf_token.secret() {
            return Err(AuthError::StateMismatch);
        }
        Ok(credential_entry_from_token(&callback.token))
    }
}

fn implicit_authorization_url(
    client: &ConfiguredAuthorizationClient,
    scheme: &OauthScheme,
) -> (Url, CsrfToken) {
    let mut request = client
        .authorize_url(CsrfToken::new_random)
        .use_implicit_flow()
        .add_scopes(scopes(scheme));
    for param in &scheme.extra_authorize_params {
        request = request.add_extra_param(&param.key, &param.value);
    }
    request.url()
}
