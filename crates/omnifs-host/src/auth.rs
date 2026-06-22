//! Authentication and credential injection for HTTP requests.
//!
//! `AuthManager` owns provider-agnostic HTTP auth strategies. Static tokens
//! inject headers directly. OAuth strategies keep tokens outside the provider
//! sandbox and let `HttpStack` retry once with rebuilt headers.

use arc_swap::ArcSwapOption;
use async_singleflight::Group;
use omnifs_auth::{AuthError as OAuthError, OAuthClient, OAuthRequest, oauth_request_from_config};
use omnifs_core::CredentialId;
use omnifs_creds::{CredentialEntry, CredentialStore, FileStore, StoreError};
use omnifs_mount::{Auth, AuthKind, OAuth, StaticToken};
use omnifs_provider::{AuthManifest, SchemeResolveError};
use secrecy::ExposeSecret;
use std::path::Path;
use std::sync::Arc;
use time::{Duration as TimeDuration, OffsetDateTime};

const DEFAULT_ACCOUNT: &str = "default";
const OAUTH_REFRESH_WINDOW: TimeDuration = TimeDuration::seconds(60);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("auth manifest is required for auth type `{0}`")]
    ManifestRequired(AuthKind),
    #[error("no static-token auth scheme matches `{0}`")]
    StaticSchemeNotFound(String),
    #[error("static-token auth scheme is ambiguous; set auth.scheme")]
    AmbiguousStaticScheme,
    #[error("no oauth auth scheme matches `{0}`")]
    OAuthSchemeNotFound(String),
    #[error("oauth auth scheme is ambiguous; set auth.scheme")]
    AmbiguousOAuthScheme,
    #[error("credential store is required for auth type `{0}`")]
    CredentialStoreRequired(AuthKind),
    #[error("credential id error: {0}")]
    CredentialId(String),
    #[error("credential store error: {0}")]
    CredentialStore(#[from] StoreError),
    #[error("oauth error: {0}")]
    OAuth(String),
    #[error("oauth refresh failed: {0}")]
    RefreshFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    Refreshed,
    NoCredential,
    NotApplicable,
}

enum AuthStrategy {
    Static(StaticTokenStrategy),
    OAuth(Box<OAuth2PkceStrategy>),
}

/// Manages authentication header injection for HTTP requests by delegating to
/// per-mount strategies.
pub struct AuthManager {
    strategies: Vec<AuthStrategy>,
}

struct StaticTokenStrategy {
    domain: Option<String>,
    header_name: String,
    header_value: Option<String>,
}

struct AuthStoreContext {
    provider_name: String,
    store: Arc<dyn CredentialStore>,
    oauth_http: reqwest_oauth2::Client,
}

struct OAuth2PkceStrategy {
    request: OAuthRequest,
    credential_id: CredentialId,
    store: Arc<dyn CredentialStore>,
    oauth_http: reqwest_oauth2::Client,
    current: ArcSwapOption<CredentialEntry>,
    refreshes: Group<String, Option<CredentialEntry>, String>,
}

impl AuthStrategy {
    fn headers_for_url(&self, url: &str) -> Vec<(String, String)> {
        match self {
            Self::Static(strategy) => strategy.headers_for_url(url),
            Self::OAuth(strategy) => strategy.headers_for_url(url),
        }
    }

    fn requires_auth_for_url(&self, url: &str) -> bool {
        match self {
            Self::Static(strategy) => strategy.requires_auth_for_url(url),
            Self::OAuth(strategy) => strategy.requires_auth_for_url(url),
        }
    }

    async fn prepare_for_url(&self, url: &str) -> Result<(), AuthError> {
        match self {
            Self::Static(_) => Ok(()),
            Self::OAuth(strategy) => strategy.prepare_for_url(url).await,
        }
    }

    fn should_refresh_for_response(
        &self,
        url: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) -> bool {
        match self {
            Self::Static(_) => false,
            Self::OAuth(strategy) => strategy.should_refresh_for_response(url, status, headers),
        }
    }

    async fn refresh_for_url(&self, url: &str) -> Result<RefreshOutcome, AuthError> {
        match self {
            Self::Static(_) => Ok(RefreshOutcome::NotApplicable),
            Self::OAuth(strategy) => strategy.refresh_for_url(url).await,
        }
    }
}

impl AuthManager {
    pub fn none() -> Self {
        Self { strategies: vec![] }
    }

    pub fn from_configs(configs: &[Auth]) -> Result<Self, AuthError> {
        Self::from_configs_and_manifest(configs, None)
    }

    pub fn from_config(config: &Auth) -> Result<Self, AuthError> {
        Self::from_configs(std::slice::from_ref(config))
    }

    pub fn from_configs_and_manifest(
        configs: &[Auth],
        manifest: Option<&AuthManifest>,
    ) -> Result<Self, AuthError> {
        Self::from_configs_manifest_store(configs, manifest, None)
    }

    fn from_configs_manifest_store(
        configs: &[Auth],
        manifest: Option<&AuthManifest>,
        store_context: Option<&AuthStoreContext>,
    ) -> Result<Self, AuthError> {
        let mut strategies = Vec::new();
        for config in configs {
            strategies.extend(Self::build_strategies(config, manifest, store_context)?);
        }
        Ok(Self { strategies })
    }

    #[doc(hidden)]
    pub fn from_configs_manifest_store_with_http(
        configs: &[Auth],
        manifest: Option<&AuthManifest>,
        provider_name: impl Into<String>,
        store: Arc<dyn CredentialStore>,
        oauth_http: reqwest_oauth2::Client,
    ) -> Result<Self, AuthError> {
        let context = AuthStoreContext {
            provider_name: provider_name.into(),
            store,
            oauth_http,
        };
        Self::from_configs_manifest_store(configs, manifest, Some(&context))
    }

    pub fn from_configs_manifest_store_with_store(
        configs: &[Auth],
        manifest: Option<&AuthManifest>,
        provider_name: impl Into<String>,
        store: Arc<dyn CredentialStore>,
    ) -> Result<Self, AuthError> {
        let oauth_http = reqwest_oauth2::ClientBuilder::new()
            .redirect(reqwest_oauth2::redirect::Policy::none())
            .build()
            .map_err(|e| AuthError::OAuth(e.to_string()))?;
        Self::from_configs_manifest_store_with_http(
            configs,
            manifest,
            provider_name,
            store,
            oauth_http,
        )
    }

    fn build_strategies(
        config: &Auth,
        manifest: Option<&AuthManifest>,
        store_context: Option<&AuthStoreContext>,
    ) -> Result<Vec<AuthStrategy>, AuthError> {
        match config {
            Auth::StaticToken(config) => {
                StaticTokenStrategy::from_manifest_config(config, manifest, store_context)
                    .map(|strategies| strategies.into_iter().map(AuthStrategy::Static).collect())
            },
            Auth::OAuth(config) => {
                let context =
                    store_context.ok_or(AuthError::CredentialStoreRequired(AuthKind::OAuth))?;
                let strategy = OAuth2PkceStrategy::from_manifest_config(
                    config,
                    manifest,
                    context.provider_name.as_str(),
                    Arc::clone(&context.store),
                    context.oauth_http.clone(),
                )?;
                Ok(vec![AuthStrategy::OAuth(Box::new(strategy))])
            },
        }
    }

    pub async fn prepare_for_url(&self, url: &str) -> Result<(), AuthError> {
        for strategy in self
            .strategies
            .iter()
            .filter(|strategy| strategy.requires_auth_for_url(url))
        {
            strategy.prepare_for_url(url).await?;
        }
        Ok(())
    }

    pub fn headers_for_url(&self, url: &str) -> Vec<(String, String)> {
        self.strategies
            .iter()
            .flat_map(|strategy| strategy.headers_for_url(url))
            .collect()
    }

    pub fn requires_auth_for_url(&self, url: &str) -> bool {
        self.strategies
            .iter()
            .any(|strategy| strategy.requires_auth_for_url(url))
    }

    pub fn should_refresh_for_response(
        &self,
        url: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) -> bool {
        self.strategies
            .iter()
            .any(|strategy| strategy.should_refresh_for_response(url, status, headers))
    }

    pub async fn refresh_for_url(&self, url: &str) -> Result<RefreshOutcome, AuthError> {
        let mut saw_no_credential = false;
        for strategy in &self.strategies {
            match strategy.refresh_for_url(url).await? {
                RefreshOutcome::Refreshed => return Ok(RefreshOutcome::Refreshed),
                RefreshOutcome::NoCredential => saw_no_credential = true,
                RefreshOutcome::NotApplicable => {},
            }
        }
        if saw_no_credential {
            Ok(RefreshOutcome::NoCredential)
        } else {
            Ok(RefreshOutcome::NotApplicable)
        }
    }
}

pub(crate) fn credential_store_for_file(credentials_file: &Path) -> Arc<dyn CredentialStore> {
    Arc::new(FileStore::new(credentials_file))
}

impl StaticTokenStrategy {
    fn from_manifest_config(
        config: &StaticToken,
        manifest: Option<&AuthManifest>,
        store_context: Option<&AuthStoreContext>,
    ) -> Result<Vec<Self>, AuthError> {
        let manifest = manifest.ok_or(AuthError::ManifestRequired(AuthKind::StaticToken))?;
        let scheme = manifest
            .resolve_static_scheme(config.scheme.as_deref())
            .map_err(AuthError::from)?;
        let header_value = Self::credential_value(config, &scheme.key, store_context)?
            .map(|token| format!("{}{}", scheme.value_prefix, token));
        let header_name = scheme
            .header_name
            .clone()
            .unwrap_or_else(|| "Authorization".to_string());
        Ok(scheme
            .inject_domains
            .iter()
            .map(|domain| Self {
                domain: Some(domain.clone()),
                header_name: header_name.clone(),
                header_value: header_value.clone(),
            })
            .collect())
    }

    fn credential_value(
        config: &StaticToken,
        scheme: &str,
        store_context: Option<&AuthStoreContext>,
    ) -> Result<Option<String>, AuthError> {
        if config.token_file.is_some() || config.token_env.is_some() {
            return Ok(read_credential(
                config.token_file.as_deref(),
                config.token_env.as_deref(),
            ));
        }

        let context =
            store_context.ok_or(AuthError::CredentialStoreRequired(AuthKind::StaticToken))?;
        let account = config
            .account
            .clone()
            .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
        let credential_id = CredentialId::new(&context.provider_name, scheme, account)
            .map_err(|e| AuthError::CredentialId(e.to_string()))?;
        Ok(context
            .store
            .get(&credential_id)?
            .filter(|entry| entry.kind() == AuthKind::StaticToken)
            .map(|entry| entry.access_token().expose_secret().to_string()))
    }

    fn applies_to_url(&self, url: &str) -> bool {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(String::from));
        match (&self.domain, &host) {
            (Some(d), Some(h)) => d == h,
            (None, _) => true,
            _ => false,
        }
    }
}

impl StaticTokenStrategy {
    fn headers_for_url(&self, url: &str) -> Vec<(String, String)> {
        if !self.applies_to_url(url) {
            return Vec::new();
        }
        self.header_value.as_ref().map_or_else(Vec::new, |value| {
            vec![(self.header_name.clone(), value.clone())]
        })
    }

    fn requires_auth_for_url(&self, url: &str) -> bool {
        self.applies_to_url(url)
    }
}

impl OAuth2PkceStrategy {
    fn from_manifest_config(
        config: &OAuth,
        manifest: Option<&AuthManifest>,
        provider_name: &str,
        store: Arc<dyn CredentialStore>,
        oauth_http: reqwest_oauth2::Client,
    ) -> Result<Self, AuthError> {
        let manifest = manifest.ok_or(AuthError::ManifestRequired(AuthKind::OAuth))?;
        let scheme = manifest
            .resolve_oauth_scheme(config.scheme.as_deref())
            .map_err(AuthError::from)?
            .clone();
        let account = config
            .account
            .clone()
            .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
        let credential_id = CredentialId::new(provider_name, &scheme.key, account)
            .map_err(|e| AuthError::CredentialId(e.to_string()))?;
        let current = store.get(&credential_id)?;
        let request = oauth_request_from_config(Some(config), scheme)?;
        Ok(Self {
            request,
            credential_id,
            store,
            oauth_http,
            current: ArcSwapOption::from(current.map(Arc::new)),
            refreshes: Group::new(),
        })
    }

    fn applies_to_url(&self, url: &str) -> bool {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(String::from));
        host.is_some_and(|host| {
            self.request
                .scheme()
                .inject_domains
                .iter()
                .any(|domain| domain == &host)
        })
    }

    fn header_name(&self) -> String {
        self.request
            .scheme()
            .inject_header_name
            .clone()
            .unwrap_or_else(|| "Authorization".to_string())
    }

    async fn refresh_if_needed(&self, force: bool) -> Result<Option<CredentialEntry>, AuthError> {
        let prior = self.current.load_full();
        if !force && prior.as_deref().is_some_and(oauth_entry_is_fresh) {
            return Ok(prior.as_deref().cloned());
        }

        if !force
            && let Some(stored) = self.load_store_entry()?
            && oauth_entry_is_fresh(&stored)
        {
            self.current.store(Some(Arc::new(stored.clone())));
            return Ok(Some(stored));
        }

        let key = self.credential_id.storage_key();
        let result = self
            .refreshes
            .work(&key, async move {
                self.refresh_under_lock(force)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;
        match result {
            Ok(entry) => Ok(entry),
            Err(Some(error)) => Err(AuthError::RefreshFailed(error)),
            Err(None) => Err(AuthError::RefreshFailed(
                "refresh leader failed".to_string(),
            )),
        }
    }

    async fn refresh_under_lock(&self, force: bool) -> Result<Option<CredentialEntry>, AuthError> {
        let current = self.current.load_full();
        let Some(stored) = self.load_store_entry()? else {
            self.current.store(None);
            return Ok(None);
        };

        if stored_entry_satisfies(&stored, current.as_deref(), force) {
            self.current.store(Some(Arc::new(stored.clone())));
            return Ok(Some(stored));
        }

        let Some(refresh_token) = stored.refresh_token() else {
            self.current.store(Some(Arc::new(stored.clone())));
            return if force {
                Err(AuthError::RefreshFailed(format!(
                    "OAuth credential {} has no refresh token",
                    self.credential_id
                )))
            } else {
                Ok(Some(stored))
            };
        };

        let client = OAuthClient::from_http_client(self.oauth_http.clone());
        match client.refresh(self.request.clone(), refresh_token).await {
            Ok(refreshed) => {
                self.store.put(&self.credential_id, &refreshed)?;
                self.current.store(Some(Arc::new(refreshed.clone())));
                Ok(Some(refreshed))
            },
            Err(OAuthError::TokenEndpoint { error, .. }) if error == "invalid_grant" => {
                self.store.delete(&self.credential_id)?;
                self.current.store(None);
                Err(AuthError::RefreshFailed(
                    "OAuth refresh token was rejected".to_string(),
                ))
            },
            Err(error) => Err(AuthError::OAuth(error.to_string())),
        }
    }

    fn load_store_entry(&self) -> Result<Option<CredentialEntry>, AuthError> {
        let entry = self.store.get(&self.credential_id)?;
        Ok(entry.filter(|entry| entry.kind() == AuthKind::OAuth))
    }
}

impl OAuth2PkceStrategy {
    fn headers_for_url(&self, url: &str) -> Vec<(String, String)> {
        if !self.applies_to_url(url) {
            return Vec::new();
        }
        let Some(entry) = self.current.load_full() else {
            return Vec::new();
        };
        if !oauth_entry_is_valid(&entry) {
            return Vec::new();
        }
        let value = format!(
            "{}{}",
            self.request.scheme().inject_value_prefix,
            entry.access_token().expose_secret()
        );
        vec![(self.header_name(), value)]
    }

    fn requires_auth_for_url(&self, url: &str) -> bool {
        self.applies_to_url(url)
    }

    async fn prepare_for_url(&self, url: &str) -> Result<(), AuthError> {
        if !self.applies_to_url(url) {
            return Ok(());
        }
        let prior = self.current.load_full();
        match self.refresh_if_needed(false).await {
            Ok(_) => Ok(()),
            Err(_error) if prior.as_deref().is_some_and(oauth_entry_is_valid) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn should_refresh_for_response(
        &self,
        url: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) -> bool {
        if !self.applies_to_url(url) {
            return false;
        }
        status == reqwest::StatusCode::UNAUTHORIZED
            || (status == reqwest::StatusCode::FORBIDDEN && bearer_invalid_token(headers))
    }

    async fn refresh_for_url(&self, url: &str) -> Result<RefreshOutcome, AuthError> {
        if !self.applies_to_url(url) {
            return Ok(RefreshOutcome::NotApplicable);
        }
        match self.refresh_if_needed(true).await {
            Ok(Some(_)) => Ok(RefreshOutcome::Refreshed),
            Ok(None) => Ok(RefreshOutcome::NoCredential),
            Err(error) => Err(error),
        }
    }
}

impl From<OAuthError> for AuthError {
    fn from(value: OAuthError) -> Self {
        Self::OAuth(value.to_string())
    }
}

impl From<SchemeResolveError> for AuthError {
    fn from(error: SchemeResolveError) -> Self {
        const STATIC_KIND: &str = "static-token";
        match error {
            SchemeResolveError::NotFound { kind, key } if kind == STATIC_KIND => {
                Self::StaticSchemeNotFound(key)
            },
            SchemeResolveError::NotFound { key, .. } => Self::OAuthSchemeNotFound(key),
            SchemeResolveError::Ambiguous { kind } if kind == STATIC_KIND => {
                Self::AmbiguousStaticScheme
            },
            SchemeResolveError::Ambiguous { .. } => Self::AmbiguousOAuthScheme,
            SchemeResolveError::NoSchemes { kind } if kind == STATIC_KIND => {
                Self::StaticSchemeNotFound("<default>".to_string())
            },
            SchemeResolveError::NoSchemes { .. } => {
                Self::OAuthSchemeNotFound("<default>".to_string())
            },
        }
    }
}

fn read_credential(token_file: Option<&str>, token_env: Option<&str>) -> Option<String> {
    token_file
        .and_then(|path| {
            std::fs::read_to_string(path)
                .ok()
                .map(|contents| contents.trim().to_string())
                .filter(|contents| !contents.is_empty())
        })
        .or_else(|| {
            token_env
                .and_then(|env_var| std::env::var(env_var).ok())
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
        })
}

fn oauth_entry_is_valid(entry: &CredentialEntry) -> bool {
    entry
        .expires_at()
        .is_none_or(|expires_at| expires_at > OffsetDateTime::now_utc())
}

fn oauth_entry_is_fresh(entry: &CredentialEntry) -> bool {
    entry
        .expires_at()
        .is_none_or(|expires_at| expires_at - OffsetDateTime::now_utc() > OAUTH_REFRESH_WINDOW)
}

fn stored_entry_satisfies(
    stored: &CredentialEntry,
    current: Option<&CredentialEntry>,
    force: bool,
) -> bool {
    if !oauth_entry_is_valid(stored) {
        return false;
    }
    if !force {
        return oauth_entry_is_fresh(stored);
    }
    current.is_none_or(|current| !same_oauth_token(stored, current))
}

fn same_oauth_token(left: &CredentialEntry, right: &CredentialEntry) -> bool {
    left.access_token().expose_secret() == right.access_token().expose_secret()
        && left
            .refresh_token()
            .map(|token| token.expose_secret().to_owned())
            == right
                .refresh_token()
                .map(|token| token.expose_secret().to_owned())
}

fn bearer_invalid_token(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            let lower = value.to_ascii_lowercase();
            lower.contains("bearer") && lower.contains("invalid_token")
        })
}
