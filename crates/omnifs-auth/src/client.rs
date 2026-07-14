use crate::error::AuthError;
use crate::flows::credential_entry_from_token;
use crate::request::OAuthRequest;
use oauth2::RefreshToken;
use omnifs_workspace::creds::CredentialEntry;
use secrecy::{ExposeSecret, SecretString};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use url::Url;

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait UrlOpener: Send + Sync {
    fn open<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<(), AuthError>>;
}

#[must_use]
#[derive(Clone)]
pub struct OAuthClient {
    pub(crate) http: reqwest::Client,
    pub(crate) opener: Option<Arc<dyn UrlOpener>>,
}

impl OAuthClient {
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: reqwest::ClientBuilder::new()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            opener: None,
        })
    }

    pub fn from_http_client(http: reqwest::Client) -> Self {
        Self { http, opener: None }
    }

    pub fn with_opener(mut self, opener: Arc<dyn UrlOpener>) -> Self {
        self.opener = Some(opener);
        self
    }

    pub fn with_system_browser(mut self) -> Self {
        self.opener = Some(Arc::new(SystemBrowser));
        self
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
        Ok(credential_entry_from_token(&token))
    }
}

struct SystemBrowser;

impl UrlOpener for SystemBrowser {
    fn open<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<(), AuthError>> {
        Box::pin(async move {
            webbrowser::open(url.as_str()).map_err(|err| AuthError::BrowserOpen(err.to_string()))
        })
    }
}
