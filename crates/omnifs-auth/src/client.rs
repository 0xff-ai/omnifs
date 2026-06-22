use crate::request::{
    ClientSideTokenLoginRequest, ConfiguredAuthorizationClient, ConfiguredClient,
    DeviceCodeLoginRequest, LoopbackLoginRequest, ManualCodeLoginRequest, OAuthRequest,
};
use oauth2::basic::BasicTokenResponse;
use oauth2::{
    AccessToken, AuthorizationCode, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    RefreshToken, Scope, StandardRevocableToken, TokenResponse,
};
use omnifs_creds::CredentialEntry;
use omnifs_provider::{OauthScheme, PkceManualCodeConfig};
use secrecy::{ExposeSecret, SecretString};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

#[cfg(test)]
mod test_support;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait UrlOpener: Send + Sync {
    fn open<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<(), AuthError>>;
}

#[must_use]
#[derive(Clone)]
pub struct OAuthClient {
    http: reqwest::Client,
    opener: Option<Arc<dyn UrlOpener>>,
}

impl OAuthClient {
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: oauth_http_client()?,
            opener: None,
        })
    }

    pub fn from_http_client(http: reqwest::Client) -> Self {
        Self { http, opener: None }
    }

    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    pub fn with_opener(mut self, opener: Arc<dyn UrlOpener>) -> Self {
        self.opener = Some(opener);
        self
    }

    pub fn with_system_browser(mut self) -> Self {
        self.opener = Some(Arc::new(SystemBrowser));
        self
    }

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

        let entry = exchange_code(
            &client,
            &self.http,
            AuthorizationCode::new(callback.code),
            pkce_verifier,
            &request.oauth.scheme,
        )
        .await?;
        Ok(entry)
    }

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

    pub async fn login_device_code<F, Fut>(
        &self,
        request: DeviceCodeLoginRequest,
        present: F,
    ) -> Result<CredentialEntry, AuthError>
    where
        F: FnOnce(DeviceCodePrompt) -> Fut,
        Fut: Future<Output = Result<(), AuthError>>,
    {
        let client = request.oauth.device_client(&request.flow)?;
        let mut exchange = client
            .exchange_device_code()
            .add_scopes(scopes(&request.oauth.scheme));
        for param in &request.oauth.scheme.extra_authorize_params {
            exchange = exchange.add_extra_param(&param.key, &param.value);
        }

        let details = exchange.request_async(&self.http).await?;
        present(DeviceCodePrompt::from_response(&details)).await?;

        let mut token_request = client.exchange_device_access_token(&details);
        for param in &request.oauth.scheme.extra_token_params {
            token_request = token_request.add_extra_param(&param.key, &param.value);
        }
        let polling_http = device_polling_http(&self.http);
        let token = token_request
            .request_async(
                &polling_http,
                tokio::time::sleep,
                Some(details.expires_in()),
            )
            .await?;
        let entry = credential_entry_from_token(&token);
        Ok(entry)
    }

    pub async fn refresh(
        &self,
        request: OAuthRequest,
        refresh_token: SecretString,
    ) -> Result<CredentialEntry, AuthError> {
        let client = request.token_client()?;
        let refresh_token = RefreshToken::new(refresh_token.expose_secret().to_owned());
        let mut exchange = client.exchange_refresh_token(&refresh_token);
        for param in &request.scheme.extra_token_params {
            exchange = exchange.add_extra_param(&param.key, &param.value);
        }

        let token = exchange.request_async(&self.http).await?;
        let entry = credential_entry_from_token(&token);
        Ok(entry)
    }

    pub async fn revoke_access_token(
        &self,
        request: OAuthRequest,
        access_token: SecretString,
    ) -> Result<RevokeOutcome, AuthError> {
        if !request.supports_revocation() {
            return Ok(RevokeOutcome::Unsupported);
        }
        let client = request.token_client()?;
        let revoke = client.revoke_token(StandardRevocableToken::from(AccessToken::new(
            access_token.expose_secret().to_owned(),
        )))?;
        revoke.request_async(&self.http).await?;
        Ok(RevokeOutcome::Revoked)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeOutcome {
    Revoked,
    Unsupported,
}

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

#[derive(Clone, Debug)]
pub struct DeviceCodePrompt {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_in: Duration,
}

impl DeviceCodePrompt {
    fn from_response(response: &oauth2::StandardDeviceAuthorizationResponse) -> DeviceCodePrompt {
        DeviceCodePrompt {
            verification_uri: response.verification_uri().to_string(),
            verification_uri_complete: response
                .verification_uri_complete()
                .map(|uri| uri.secret().to_owned()),
            user_code: response.user_code().secret().to_owned(),
            expires_in: response.expires_in(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    RequestConfig(String),
    #[error("oauth client_id is required")]
    MissingClientId,
    #[error("oauth client_secret is required for this token endpoint auth method")]
    MissingClientSecret,
    #[error("oauth loopback flow requires an opener")]
    MissingOpener,
    #[error("oauth state did not match pending login")]
    StateMismatch,
    #[error("oauth callback did not include authorization code")]
    MissingCode,
    #[error("oauth callback did not include access token")]
    MissingAccessToken,
    #[error("oauth callback did not include state")]
    MissingState,
    #[error("oauth callback request is invalid")]
    InvalidCallback,
    #[error("oauth loopback redirect URI must use 127.0.0.1 or localhost with an explicit port")]
    InvalidRedirectUri,
    #[error("oauth callback returned error {error}: {description:?}")]
    AuthorizationError {
        error: String,
        description: Option<String>,
    },
    #[error("oauth token endpoint error: {error}: {description:?}")]
    TokenEndpoint {
        error: String,
        description: Option<String>,
    },
    #[error("oauth revocation endpoint error: {0}")]
    RevocationEndpoint(String),
    #[error("oauth url error: {0}")]
    Url(#[from] url::ParseError),
    #[error("oauth client configuration error: {0}")]
    Configuration(#[from] oauth2::ConfigurationError),
    #[error("oauth http transport error: {0}")]
    HttpTransport(String),
    #[error("http client error: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("browser open failed: {0}")]
    BrowserOpen(String),
}

struct SystemBrowser;

impl UrlOpener for SystemBrowser {
    fn open<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<(), AuthError>> {
        Box::pin(async move {
            webbrowser::open(url.as_str()).map_err(|err| AuthError::BrowserOpen(err.to_string()))
        })
    }
}

fn manual_redirect_uri(config: &PkceManualCodeConfig) -> Result<RedirectUrl, AuthError> {
    Ok(RedirectUrl::new(config.redirect_uri.clone())?)
}

struct LoopbackEndpoint {
    listener: TcpListener,
    redirect_uri: RedirectUrl,
}

impl LoopbackEndpoint {
    async fn bind(template: &str) -> Result<Self, AuthError> {
        let bind_url = Self::url_with_port(template, 0)?;
        let host = bind_url.host_str().ok_or(AuthError::InvalidRedirectUri)?;
        let port = bind_url.port().ok_or(AuthError::InvalidRedirectUri)?;
        let listener = TcpListener::bind(format!("{host}:{port}")).await?;
        let redirect_uri = RedirectUrl::from_url(Self::url_with_port(
            template,
            listener.local_addr()?.port(),
        )?);
        Ok(Self {
            listener,
            redirect_uri,
        })
    }

    fn redirect_uri(&self) -> &RedirectUrl {
        &self.redirect_uri
    }

    fn into_listener(self) -> TcpListener {
        self.listener
    }

    fn url_with_port(template: &str, port: u16) -> Result<Url, AuthError> {
        let raw = if template.contains("{port}") {
            template.replace("{port}", &port.to_string())
        } else {
            template.to_owned()
        };
        let url = Url::parse(&raw)?;
        if url.scheme() != "http" {
            return Err(AuthError::InvalidRedirectUri);
        }
        let host = url.host_str().ok_or(AuthError::InvalidRedirectUri)?;
        if host != "127.0.0.1" && host != "localhost" {
            return Err(AuthError::InvalidRedirectUri);
        }
        if url.port().is_none() {
            return Err(AuthError::InvalidRedirectUri);
        }
        Ok(url)
    }
}

fn oauth_http_client() -> Result<reqwest::Client, AuthError> {
    Ok(reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Wraps a [`reqwest::Client`] for device-code polling and rewrites GitHub's
/// non-RFC-8628 behavior: GitHub returns 200 OK with an error JSON body
/// (`{"error":"authorization_pending",...}`) while the user is still
/// approving the device. Without this shim oauth2 5.x parses the 200 as a
/// success token response, fails the JSON schema check, and the entire poll
/// loop bails on the first iteration with `Failed to parse server response`.
fn device_polling_http(http: &reqwest::Client) -> DevicePollingHttp {
    DevicePollingHttp(http.clone())
}

struct DevicePollingHttp(reqwest::Client);

impl<'c> oauth2::AsyncHttpClient<'c> for DevicePollingHttp {
    type Error = oauth2::HttpClientError<reqwest::Error>;
    type Future =
        Pin<Box<dyn Future<Output = Result<oauth2::HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: oauth2::HttpRequest) -> Self::Future {
        let inner = self.0.clone();
        Box::pin(async move {
            let response =
                <reqwest::Client as oauth2::AsyncHttpClient<'_>>::call(&inner, request).await?;
            Ok(rewrite_pending_to_error_status(response))
        })
    }
}

/// If the response is 200 OK but the JSON body has an `error` field, rewrite
/// to 400 so oauth2's response handling routes it through the error-response
/// path (where `authorization_pending` and `slow_down` continue the poll).
/// Compliant providers never hit this rewrite (they already return 4xx with
/// an error body), so this is a no-op for everything but GitHub.
fn rewrite_pending_to_error_status(response: oauth2::HttpResponse) -> oauth2::HttpResponse {
    if response.status() != http::StatusCode::OK {
        return response;
    }
    let Some(content_type) = response.headers().get(http::header::CONTENT_TYPE) else {
        return response;
    };
    let ct = content_type.to_str().unwrap_or("").to_ascii_lowercase();
    if !ct.starts_with("application/json") {
        return response;
    }
    let body = response.body();
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        return response;
    };
    if parsed
        .get("error")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts.status = http::StatusCode::BAD_REQUEST;
    http::Response::from_parts(parts, body)
}

fn authorization_url(
    client: &ConfiguredClient,
    scheme: &OauthScheme,
    pkce_challenge: oauth2::PkceCodeChallenge,
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

fn scopes(scheme: &OauthScheme) -> impl Iterator<Item = Scope> + '_ {
    scheme.default_scopes.iter().cloned().map(Scope::new)
}

async fn exchange_code(
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

fn credential_entry_from_token(
    token: &oauth2::StandardTokenResponse<
        oauth2::EmptyExtraTokenFields,
        oauth2::basic::BasicTokenType,
    >,
) -> CredentialEntry {
    let now = OffsetDateTime::now_utc();
    let expires_at = token.expires_in().map(|expires_in| {
        let skew = Duration::from_mins(1);
        let effective = expires_in.checked_sub(skew).unwrap_or(expires_in);
        now + effective
    });
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

#[derive(Debug)]
struct LoopbackCallback {
    code: String,
    state: CsrfToken,
}

#[derive(Debug)]
struct ClientSideTokenCallback {
    token: BasicTokenResponse,
    state: CsrfToken,
}

/// Accept one request on the loopback listener and parse its request line into
/// the requested URL. Non-GET methods are rejected with a 405 (a browser
/// redirect only ever issues a GET); the returned stream stays open so the
/// caller can write its own completion response.
async fn accept_callback_request(
    listener: &TcpListener,
) -> Result<(tokio::net::TcpStream, Url), AuthError> {
    let (mut stream, _) = listener.accept().await?;
    let mut buf = vec![0; 4096];
    let read = stream.read(&mut buf).await?;
    let request = std::str::from_utf8(&buf[..read]).map_err(|_| AuthError::InvalidCallback)?;
    let request_line = request.lines().next().ok_or(AuthError::InvalidCallback)?;
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().ok_or(AuthError::InvalidCallback)?;
    let target = parts.next().ok_or(AuthError::InvalidCallback)?;
    if method != "GET" {
        write_callback_response(&mut stream, "405 Method Not Allowed", "method not allowed")
            .await?;
        return Err(AuthError::InvalidCallback);
    }
    let url =
        Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| AuthError::InvalidCallback)?;
    Ok((stream, url))
}

/// Acknowledge a parsed callback to the browser: 200 on success, 400 on a
/// parse/authorization failure.
async fn respond_to_callback<T>(
    stream: &mut tokio::net::TcpStream,
    result: &Result<T, AuthError>,
) -> Result<(), AuthError> {
    match result {
        Ok(_) => write_callback_response(stream, "200 OK", "authorization complete").await,
        Err(_) => write_callback_response(stream, "400 Bad Request", "authorization failed").await,
    }
}

async fn read_loopback_callback(listener: TcpListener) -> Result<LoopbackCallback, AuthError> {
    let (mut stream, url) = accept_callback_request(&listener).await?;
    let callback = parse_callback_url(&url);
    respond_to_callback(&mut stream, &callback).await?;
    callback
}

fn parse_callback_url(url: &Url) -> Result<LoopbackCallback, AuthError> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_description = Some(value.into_owned()),
            _ => {},
        }
    }
    if let Some(error) = error {
        return Err(AuthError::AuthorizationError {
            error,
            description: error_description,
        });
    }
    Ok(LoopbackCallback {
        code: code.ok_or(AuthError::MissingCode)?,
        state: CsrfToken::new(state.ok_or(AuthError::MissingState)?),
    })
}

async fn read_client_side_callback(
    listener: TcpListener,
) -> Result<ClientSideTokenCallback, AuthError> {
    for _ in 0..3 {
        let (mut stream, url) = accept_callback_request(&listener).await?;
        if url.query().is_some() {
            let callback = parse_client_side_token_url(&url);
            respond_to_callback(&mut stream, &callback).await?;
            return callback;
        }

        write_fragment_capture_response(&mut stream).await?;
    }
    Err(AuthError::MissingAccessToken)
}

fn parse_client_side_token_url(url: &Url) -> Result<ClientSideTokenCallback, AuthError> {
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    let mut has_access_token = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "access_token" => has_access_token = true,
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_description = Some(value.into_owned()),
            _ => {},
        }
    }
    if let Some(error) = error {
        return Err(AuthError::AuthorizationError {
            error,
            description: error_description,
        });
    }
    if !has_access_token {
        return Err(AuthError::MissingAccessToken);
    }
    let query = url.query().ok_or(AuthError::MissingAccessToken)?;
    let token = serde_urlencoded::from_str(query).map_err(|_| AuthError::InvalidCallback)?;
    Ok(ClientSideTokenCallback {
        token,
        state: CsrfToken::new(state.ok_or(AuthError::MissingState)?),
    })
}

async fn write_fragment_capture_response(
    stream: &mut tokio::net::TcpStream,
) -> Result<(), AuthError> {
    const BODY: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>omnifs authorization</title>
<p>Completing authorization...</p>
<script>
const fragment = window.location.hash.startsWith("#") ? window.location.hash.slice(1) : "";
if (fragment) {
  window.location.replace(window.location.pathname + "?" + fragment);
} else {
  document.body.textContent = "Authorization response did not include a token.";
}
</script>
"##;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{BODY}",
        BODY.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> Result<(), AuthError> {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

impl
    From<
        oauth2::RequestTokenError<
            oauth2::HttpClientError<reqwest::Error>,
            oauth2::StandardErrorResponse<oauth2::basic::BasicErrorResponseType>,
        >,
    > for AuthError
{
    fn from(
        value: oauth2::RequestTokenError<
            oauth2::HttpClientError<reqwest::Error>,
            oauth2::StandardErrorResponse<oauth2::basic::BasicErrorResponseType>,
        >,
    ) -> Self {
        match value {
            oauth2::RequestTokenError::ServerResponse(response) => Self::TokenEndpoint {
                error: response.error().to_string(),
                description: response.error_description().map(ToString::to_string),
            },
            oauth2::RequestTokenError::Request(err) => err.into(),
            other => Self::TokenEndpoint {
                error: "request_failed".to_owned(),
                description: Some(other.to_string()),
            },
        }
    }
}

impl
    From<
        oauth2::RequestTokenError<
            oauth2::HttpClientError<reqwest::Error>,
            oauth2::StandardErrorResponse<oauth2::RevocationErrorResponseType>,
        >,
    > for AuthError
{
    fn from(
        value: oauth2::RequestTokenError<
            oauth2::HttpClientError<reqwest::Error>,
            oauth2::StandardErrorResponse<oauth2::RevocationErrorResponseType>,
        >,
    ) -> Self {
        match value {
            oauth2::RequestTokenError::ServerResponse(response) => {
                Self::RevocationEndpoint(response.error().to_string())
            },
            oauth2::RequestTokenError::Request(err) => err.into(),
            other => Self::RevocationEndpoint(other.to_string()),
        }
    }
}

impl
    From<
        oauth2::RequestTokenError<
            oauth2::HttpClientError<reqwest::Error>,
            oauth2::DeviceCodeErrorResponse,
        >,
    > for AuthError
{
    fn from(
        value: oauth2::RequestTokenError<
            oauth2::HttpClientError<reqwest::Error>,
            oauth2::DeviceCodeErrorResponse,
        >,
    ) -> Self {
        match value {
            oauth2::RequestTokenError::ServerResponse(response) => Self::TokenEndpoint {
                error: response.error().to_string(),
                description: response.error_description().map(ToString::to_string),
            },
            oauth2::RequestTokenError::Request(err) => err.into(),
            other => Self::TokenEndpoint {
                error: "request_failed".to_owned(),
                description: Some(other.to_string()),
            },
        }
    }
}

impl From<oauth2::HttpClientError<reqwest::Error>> for AuthError {
    fn from(err: oauth2::HttpClientError<reqwest::Error>) -> Self {
        match err {
            oauth2::HttpClientError::Reqwest(err) => Self::HttpClient(*err),
            other => Self::HttpTransport(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LoginRequest;
    use omnifs_creds::Refreshability;
    use omnifs_provider::OauthScheme;
    use test_support::{FakeAuthServer, FakeBehavior, FakeOpener, FakeRevocationServer};

    #[tokio::test]
    async fn pkce_loopback_login_against_fake_server() {
        let fake = FakeAuthServer::start(FakeBehavior::default()).await;
        let scheme = fake.loopback_scheme(None);
        let opener: Arc<dyn UrlOpener> = Arc::new(FakeOpener(fake.clone()));
        let client = OAuthClient::new().unwrap().with_opener(opener);

        let entry = client
            .login_loopback(loopback_login_request(scheme))
            .await
            .unwrap();

        assert_eq!(entry.access_token().expose_secret(), "access-1");
        assert_eq!(entry.refresh_token().unwrap().expose_secret(), "refresh-1");
        assert_eq!(entry.token_type(), "bearer");
        assert_eq!(entry.scopes(), ["read", "write"]);
    }

    #[tokio::test]
    async fn pkce_manual_code_login_against_fake_server() {
        let fake = FakeAuthServer::start(FakeBehavior::default()).await;
        let scheme = fake.manual_scheme(None);
        let client = OAuthClient::new().unwrap();
        let entry = client
            .login_manual_code(manual_code_login_request(scheme), |url| {
                let fake = fake.clone();
                async move { fake.manual_authorize(url).await }
            })
            .await
            .unwrap();

        assert_eq!(entry.access_token().expose_secret(), "access-1");
        assert_eq!(entry.refresh_token().unwrap().expose_secret(), "refresh-1");
    }

    #[tokio::test]
    async fn client_side_token_login_captures_fragment_token() {
        let fake = FakeAuthServer::start(FakeBehavior::default()).await;
        let scheme = fake.client_side_scheme(None);
        let opener: Arc<dyn UrlOpener> = Arc::new(FakeOpener(fake.clone()));
        let client = OAuthClient::new().unwrap().with_opener(opener);

        let entry = client
            .login_client_side_token(client_side_token_login_request(scheme))
            .await
            .unwrap();

        assert_eq!(entry.access_token().expose_secret(), "implicit-access-1");
        assert!(entry.refresh_token().is_none());
        assert_eq!(entry.refreshability(), Refreshability::NotRefreshable);
        assert_eq!(entry.token_type(), "bearer");
        assert_eq!(entry.scopes(), ["read", "write"]);
        assert!(entry.expires_at().is_some());
    }

    #[tokio::test]
    async fn device_code_login_against_fake_server() {
        let fake = FakeAuthServer::start(FakeBehavior::default()).await;
        let scheme = fake.device_scheme(None);
        let client = OAuthClient::new().unwrap();

        let entry = client
            .login_device_code(device_code_login_request(scheme), |prompt| async move {
                assert_eq!(prompt.verification_uri, "https://example.test/device");
                assert_eq!(
                    prompt.verification_uri_complete.as_deref(),
                    Some("https://example.test/device?user_code=WDJB-MJHT")
                );
                assert_eq!(prompt.user_code, "WDJB-MJHT");
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(entry.access_token().expose_secret(), "device-access-1");
        assert!(entry.refresh_token().is_none());
        assert_eq!(entry.scopes(), ["read", "write"]);
    }

    #[tokio::test]
    async fn device_code_login_polls_past_pending_response() {
        let fake = FakeAuthServer::start(FakeBehavior {
            device_pending_responses: 1,
            ..FakeBehavior::default()
        })
        .await;
        let scheme = fake.device_scheme(None);
        let client = OAuthClient::new().unwrap();

        let entry = client
            .login_device_code(device_code_login_request(scheme), |_| async { Ok(()) })
            .await
            .unwrap();

        assert_eq!(entry.access_token().expose_secret(), "device-access-1");
    }

    #[test]
    fn rewrite_pending_status_handling() {
        let pending_body =
            br#"{"error":"authorization_pending","error_description":"..."}"#.to_vec();
        let pending = http::Response::builder()
            .status(http::StatusCode::OK)
            .header(
                http::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .body(pending_body.clone())
            .unwrap();
        let rewritten = rewrite_pending_to_error_status(pending);
        assert_eq!(rewritten.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(rewritten.body(), &pending_body);

        let token_body = br#"{"access_token":"x","token_type":"bearer"}"#.to_vec();
        let token = http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(token_body.clone())
            .unwrap();
        let rewritten = rewrite_pending_to_error_status(token);
        assert_eq!(rewritten.status(), http::StatusCode::OK);
        assert_eq!(rewritten.body(), &token_body);

        let form_body = b"error=authorization_pending".to_vec();
        let form = http::Response::builder()
            .status(http::StatusCode::OK)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body.clone())
            .unwrap();
        let rewritten = rewrite_pending_to_error_status(form);
        assert_eq!(rewritten.status(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn loopback_endpoint_exposes_concrete_redirect_uri() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let fixed_port = probe.local_addr().unwrap().port();
        drop(probe);

        let fixed_template = format!("http://127.0.0.1:{fixed_port}/callback");
        let fixed = LoopbackEndpoint::bind(&fixed_template).await.unwrap();
        assert_eq!(fixed.redirect_uri().as_str(), fixed_template);

        let dynamic = LoopbackEndpoint::bind("http://127.0.0.1:{port}/callback")
            .await
            .unwrap();
        let dynamic_url = Url::parse(dynamic.redirect_uri().as_str()).unwrap();
        assert_eq!(dynamic_url.host_str(), Some("127.0.0.1"));
        assert!(dynamic_url.port().is_some_and(|port| port > 0));

        assert!(matches!(
            LoopbackEndpoint::bind("https://example.com/callback").await,
            Err(AuthError::InvalidRedirectUri)
        ));
    }

    #[test]
    fn loopback_callback_surfaces_authorization_error() {
        let err = parse_callback_url(
            &Url::parse("http://127.0.0.1/callback?error=access_denied&error_description=denied")
                .unwrap(),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            AuthError::AuthorizationError {
                error,
                description
            } if error == "access_denied" && description.as_deref() == Some("denied")
        ));
    }

    #[test]
    fn loopback_callback_requires_code_and_state() {
        let missing_code =
            parse_callback_url(&Url::parse("http://127.0.0.1/callback?state=ok").unwrap())
                .unwrap_err();
        assert!(matches!(missing_code, AuthError::MissingCode));

        let missing_state =
            parse_callback_url(&Url::parse("http://127.0.0.1/callback?code=ok").unwrap())
                .unwrap_err();
        assert!(matches!(missing_state, AuthError::MissingState));
    }

    #[tokio::test]
    async fn csrf_state_mismatch_is_rejected() {
        let fake = FakeAuthServer::start(FakeBehavior {
            state_override: Some("wrong-state".to_owned()),
            ..FakeBehavior::default()
        })
        .await;
        let scheme = fake.loopback_scheme(None);
        let opener: Arc<dyn UrlOpener> = Arc::new(FakeOpener(fake.clone()));
        let client = OAuthClient::new().unwrap().with_opener(opener);

        let err = client
            .login_loopback(loopback_login_request(scheme))
            .await
            .unwrap_err();

        assert!(matches!(err, AuthError::StateMismatch));
    }

    #[tokio::test]
    async fn token_endpoint_errors_surface_typed_errors() {
        let fake = FakeAuthServer::start(FakeBehavior {
            token_error: Some(("invalid_grant".to_owned(), "bad code".to_owned())),
            ..FakeBehavior::default()
        })
        .await;
        let scheme = fake.manual_scheme(None);
        let client = OAuthClient::new().unwrap();

        let err = client
            .login_manual_code(manual_code_login_request(scheme), |url| {
                let fake = fake.clone();
                async move { fake.manual_authorize(url).await }
            })
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            AuthError::TokenEndpoint {
                error,
                description
            } if error == "invalid_grant" && description.as_deref() == Some("bad code")
        ));
    }

    #[tokio::test]
    async fn optional_revocation_endpoint_works_without_builder_type_branching() {
        let fake = FakeAuthServer::start(FakeBehavior::default()).await;
        let revoke_fake = FakeRevocationServer::start().await;
        let scheme = fake.loopback_scheme(Some(revoke_fake.endpoint()));
        let http = reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let client = OAuthClient::new().unwrap().with_http_client(http);

        let revoked = client
            .revoke_access_token(
                OAuthRequest::new(scheme),
                SecretString::from("access-1".to_owned()),
            )
            .await
            .unwrap();

        assert_eq!(revoked, RevokeOutcome::Revoked);
        assert_eq!(revoke_fake.revocations(), 1);

        let no_revoke_scheme = fake.loopback_scheme(None);
        let skipped = client
            .revoke_access_token(
                OAuthRequest::new(no_revoke_scheme),
                SecretString::from("access-2".to_owned()),
            )
            .await
            .unwrap();
        assert_eq!(skipped, RevokeOutcome::Unsupported);
    }

    #[tokio::test]
    async fn refresh_exchange_parses_rotated_refresh_token() {
        let fake = FakeAuthServer::start(FakeBehavior::default()).await;
        let scheme = fake.loopback_scheme(None);
        let client = OAuthClient::new().unwrap();

        let entry = client
            .refresh(
                OAuthRequest::new(scheme),
                SecretString::from("refresh-1".to_owned()),
            )
            .await
            .unwrap();

        assert_eq!(entry.access_token().expose_secret(), "access-refresh-1");
        assert_eq!(
            entry.refresh_token().unwrap().expose_secret(),
            "refresh-rotated-1"
        );
    }

    #[tokio::test]
    async fn device_code_refresh_does_not_require_redirect_uri() {
        let fake = FakeAuthServer::start(FakeBehavior::default()).await;
        let scheme = fake.device_scheme(None);
        let client = OAuthClient::new().unwrap();

        let entry = client
            .refresh(
                OAuthRequest::new(scheme),
                SecretString::from("refresh-1".to_owned()),
            )
            .await
            .unwrap();

        assert_eq!(entry.access_token().expose_secret(), "access-refresh-1");
    }

    fn loopback_login_request(scheme: OauthScheme) -> LoopbackLoginRequest {
        let LoginRequest::Loopback(request) = OAuthRequest::new(scheme).into_login_request() else {
            panic!("expected loopback login request");
        };
        request
    }

    fn manual_code_login_request(scheme: OauthScheme) -> ManualCodeLoginRequest {
        let LoginRequest::ManualCode(request) = OAuthRequest::new(scheme).into_login_request()
        else {
            panic!("expected manual-code login request");
        };
        request
    }

    fn client_side_token_login_request(scheme: OauthScheme) -> ClientSideTokenLoginRequest {
        let LoginRequest::ClientSideToken(request) = OAuthRequest::new(scheme).into_login_request()
        else {
            panic!("expected client-side token login request");
        };
        request
    }

    fn device_code_login_request(scheme: OauthScheme) -> DeviceCodeLoginRequest {
        let LoginRequest::DeviceCode(request) = OAuthRequest::new(scheme).into_login_request()
        else {
            panic!("expected device-code login request");
        };
        request
    }
}
