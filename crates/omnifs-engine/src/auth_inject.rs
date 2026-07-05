//! Authentication header injection for HTTP requests.
//!
//! `AuthManager` maps a mount's auth config onto per-credential strategies and
//! delegates every credential concern (store access, expiry, OAuth refresh) to
//! the shared `omnifs_auth::CredentialService`. `AuthManager` keeps only two
//! jobs: matching a request URL to the credentials that apply to it, and
//! composing the resolved material into wire headers. The service is the single
//! fail-closed owner of the bytes.

use omnifs_auth::{
    AuthError as OAuthError, AuthUnavailable, CredentialHealth, CredentialService, OAuthClient,
    OAuthRequest,
};
use omnifs_workspace::authn::{AuthKind, AuthManifest, CredentialId, SchemeResolveError};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::ids::ProviderName;
use omnifs_workspace::mounts::{Auth, OAuth, StaticToken};
use std::path::Path;
use std::sync::Arc;

/// Build the single host-wide credential owner over the on-disk store. OAuth
/// refresh uses a no-redirect client, matching the login flows.
pub(crate) fn credential_service_for_file(
    credentials_file: &Path,
) -> Result<Arc<CredentialService>, InjectError> {
    let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(credentials_file));
    let oauth = OAuthClient::new()?;
    Ok(Arc::new(CredentialService::new(store, oauth)))
}

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
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
    #[error("credential id error: {0}")]
    CredentialId(String),
    #[error("oauth error: {0}")]
    OAuth(String),
    #[error("oauth refresh failed: {0}")]
    RefreshFailed(String),
    #[error("credential unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    Refreshed,
    NoCredential,
    NotApplicable,
}

/// One mount credential resolved to the domains it authorizes. Domain matching
/// lives here; the credential bytes live in the `CredentialService` under `id`.
struct Strategy {
    kind: AuthKind,
    id: CredentialId,
    domains: Vec<String>,
}

impl Strategy {
    fn applies_to_url(&self, url: &str) -> bool {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(String::from));
        host.is_some_and(|host| self.domains.iter().any(|domain| domain == &host))
    }
}

/// Maps a mount's auth config onto per-credential strategies over a shared
/// [`CredentialService`]. A no-auth mount has no strategies and no service.
pub struct AuthManager {
    service: Option<Arc<CredentialService>>,
    strategies: Vec<Strategy>,
}

impl AuthManager {
    pub fn none() -> Self {
        Self {
            service: None,
            strategies: vec![],
        }
    }

    /// Build over a shared service (the daemon-wide credential owner). Each
    /// strategy registers its credential with the service.
    pub fn from_configs_manifest_service(
        configs: &[Auth],
        manifest: Option<&AuthManifest>,
        provider_name: &str,
        service: Arc<CredentialService>,
    ) -> Result<Self, InjectError> {
        let mut strategies = Vec::new();
        for config in configs {
            strategies.extend(build_strategies(config, manifest, provider_name, &service)?);
        }
        Ok(Self {
            service: Some(service),
            strategies,
        })
    }

    /// Test/standalone constructor: build a dedicated service around `store` and
    /// `oauth_http`, then register strategies against it.
    #[doc(hidden)]
    pub fn from_configs_manifest_store_with_http(
        configs: &[Auth],
        manifest: Option<&AuthManifest>,
        provider_name: impl Into<String>,
        store: Arc<dyn CredentialStore>,
        oauth_http: reqwest_oauth2::Client,
    ) -> Result<Self, InjectError> {
        let provider_name = provider_name.into();
        let service = Arc::new(CredentialService::new(
            store,
            OAuthClient::from_http_client(oauth_http),
        ));
        Self::from_configs_manifest_service(configs, manifest, &provider_name, service)
    }

    pub async fn prepare_for_url(&self, url: &str) -> Result<(), InjectError> {
        let Some(service) = &self.service else {
            return Ok(());
        };
        for strategy in self
            .strategies
            .iter()
            .filter(|strategy| strategy.kind == AuthKind::OAuth && strategy.applies_to_url(url))
        {
            match service.authorization(&strategy.id).await {
                // A never-authorized credential falls through to the empty-headers
                // "no credentials" denial in the caller; a refresh failure fails
                // the prepare outright. Both deny.
                Ok(_) | Err(AuthUnavailable::Missing) => {},
                Err(error) => return Err(InjectError::Unavailable(error.to_string())),
            }
        }
        Ok(())
    }

    pub fn headers_for_url(&self, url: &str) -> Vec<(String, String)> {
        let Some(service) = &self.service else {
            return Vec::new();
        };
        self.strategies
            .iter()
            .filter(|strategy| strategy.applies_to_url(url))
            .filter_map(|strategy| service.cached_authorization(&strategy.id))
            .map(|material| {
                (
                    material.name().to_string(),
                    material.expose_value().to_string(),
                )
            })
            .collect()
    }

    pub fn requires_auth_for_url(&self, url: &str) -> bool {
        self.strategies
            .iter()
            .any(|strategy| strategy.applies_to_url(url))
    }

    /// Credential ids this manager registered with the service, for the
    /// mount-start credential health check. Empty for a no-auth mount.
    pub(crate) fn credential_ids(&self) -> impl Iterator<Item = &CredentialId> {
        self.strategies.iter().map(|strategy| &strategy.id)
    }

    pub fn should_refresh_for_response(
        &self,
        url: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) -> bool {
        self.strategies.iter().any(|strategy| {
            strategy.kind == AuthKind::OAuth
                && strategy.applies_to_url(url)
                && oauth_should_refresh(status, headers)
        })
    }

    pub async fn refresh_for_url(&self, url: &str) -> Result<RefreshOutcome, InjectError> {
        let Some(service) = &self.service else {
            return Ok(RefreshOutcome::NotApplicable);
        };
        let mut saw_no_credential = false;
        for strategy in self
            .strategies
            .iter()
            .filter(|strategy| strategy.applies_to_url(url))
        {
            if strategy.kind != AuthKind::OAuth {
                continue;
            }
            match service.refresh(&strategy.id).await {
                Ok(Some(_)) => return Ok(RefreshOutcome::Refreshed),
                Ok(None) => saw_no_credential = true,
                Err(error) => return Err(InjectError::RefreshFailed(error.to_string())),
            }
        }
        Ok(if saw_no_credential {
            RefreshOutcome::NoCredential
        } else {
            RefreshOutcome::NotApplicable
        })
    }
}

fn build_strategies(
    config: &Auth,
    manifest: Option<&AuthManifest>,
    provider_name: &str,
    service: &CredentialService,
) -> Result<Vec<Strategy>, InjectError> {
    match config {
        Auth::StaticToken(inner) => build_static(config, inner, manifest, provider_name, service),
        Auth::OAuth(inner) => build_oauth(config, inner, manifest, provider_name, service),
    }
}

fn build_static(
    auth: &Auth,
    config: &StaticToken,
    manifest: Option<&AuthManifest>,
    provider_name: &str,
    service: &CredentialService,
) -> Result<Vec<Strategy>, InjectError> {
    let manifest = manifest.ok_or(InjectError::ManifestRequired(AuthKind::StaticToken))?;
    let scheme = manifest
        .resolve_static_scheme(config.scheme.as_deref())
        .map_err(InjectError::from)?;
    let id = credential_id(provider_name, auth, &scheme.key)?;
    let header_name = scheme
        .header_name
        .clone()
        .unwrap_or_else(|| "Authorization".to_string());
    let value_prefix = scheme.value_prefix.clone();
    let domains = scheme.inject_domains.clone();
    service.register_static(id.clone(), header_name, value_prefix);
    Ok(vec![Strategy {
        kind: AuthKind::StaticToken,
        id,
        domains,
    }])
}

fn build_oauth(
    auth: &Auth,
    config: &OAuth,
    manifest: Option<&AuthManifest>,
    provider_name: &str,
    service: &CredentialService,
) -> Result<Vec<Strategy>, InjectError> {
    let manifest = manifest.ok_or(InjectError::ManifestRequired(AuthKind::OAuth))?;
    let scheme = manifest
        .resolve_oauth_scheme(config.scheme.as_deref())
        .map_err(InjectError::from)?
        .clone();
    let id = credential_id(provider_name, auth, &scheme.key)?;
    let request = OAuthRequest::from_mount_config(Some(config), scheme)?;
    let domains = request.scheme().inject_domains.clone();
    service.register_oauth(id.clone(), request);
    Ok(vec![Strategy {
        kind: AuthKind::OAuth,
        id,
        domains,
    }])
}

fn credential_id(
    provider_name: &str,
    auth: &Auth,
    scheme: &str,
) -> Result<CredentialId, InjectError> {
    let provider =
        ProviderName::new(provider_name).map_err(|e| InjectError::CredentialId(e.to_string()))?;
    CredentialId::for_mount(&provider, auth, scheme)
        .map_err(|e| InjectError::CredentialId(e.to_string()))
}

/// Non-secret credential health for one mount, checked once at mount-start:
/// `Some` when a registered credential is `Missing`, `Expired`, or
/// `NeedsConsent`. `None` for a no-auth mount or once every registered
/// credential is at least usable. The mount still loads either way; the
/// daemon surfaces the warning in the Mounts subsystem health without
/// blocking the mount on it.
pub(crate) fn build_time_credential_warning(
    service: &CredentialService,
    manager: &AuthManager,
) -> Option<String> {
    let ids: std::collections::HashSet<&CredentialId> = manager.credential_ids().collect();
    if ids.is_empty() {
        return None;
    }
    service.health().into_iter().find_map(|status| {
        if !ids.contains(&status.id) {
            return None;
        }
        matches!(
            status.health,
            CredentialHealth::Missing | CredentialHealth::Expired | CredentialHealth::NeedsConsent
        )
        .then(|| format!("credential {} is {:?}", status.id, status.health))
    })
}

fn oauth_should_refresh(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED
        || (status == reqwest::StatusCode::FORBIDDEN && bearer_invalid_token(headers))
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

impl From<OAuthError> for InjectError {
    fn from(value: OAuthError) -> Self {
        Self::OAuth(value.to_string())
    }
}

impl From<SchemeResolveError> for InjectError {
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
