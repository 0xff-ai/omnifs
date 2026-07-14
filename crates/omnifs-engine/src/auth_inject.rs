//! Authentication header injection for HTTP requests.
//!
//! `AuthManager` maps a mount's auth config onto one credential binding and
//! delegates every credential concern (store access, expiry, OAuth refresh) to
//! the shared `omnifs_auth::CredentialService`. `AuthManager` keeps only two
//! jobs: matching a request URL to the credential that applies to it, and
//! composing the resolved material into a wire header. The service is the single
//! fail-closed owner of the bytes.

use omnifs_auth::{
    AuthError as OAuthError, CredentialHealth, CredentialService, OAuthClient, OAuthRequest,
    RefreshOutcome, RejectionEvidence,
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
    #[error("credential unavailable: {0}")]
    Unavailable(String),
}

/// One mount credential resolved to the domains it authorizes. Domain matching
/// lives here; the credential bytes live in the `CredentialService` under `id`.
struct AuthBinding {
    service: Arc<CredentialService>,
    id: CredentialId,
    domains: Vec<String>,
}

impl AuthBinding {
    fn applies_to_url(&self, url: &str) -> bool {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(String::from));
        host.is_some_and(|host| self.domains.iter().any(|domain| domain == &host))
    }

    fn register(
        auth: &Auth,
        manifest: Option<&AuthManifest>,
        provider_name: &str,
        service: Arc<CredentialService>,
    ) -> Result<Self, InjectError> {
        match auth {
            Auth::StaticToken(config) => {
                Self::register_static(auth, config, manifest, provider_name, service)
            },
            Auth::OAuth(config) => {
                Self::register_oauth(auth, config, manifest, provider_name, service)
            },
        }
    }

    fn register_static(
        auth: &Auth,
        config: &StaticToken,
        manifest: Option<&AuthManifest>,
        provider_name: &str,
        service: Arc<CredentialService>,
    ) -> Result<Self, InjectError> {
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
        Ok(Self {
            service,
            id,
            domains,
        })
    }

    fn register_oauth(
        auth: &Auth,
        config: &OAuth,
        manifest: Option<&AuthManifest>,
        provider_name: &str,
        service: Arc<CredentialService>,
    ) -> Result<Self, InjectError> {
        let manifest = manifest.ok_or(InjectError::ManifestRequired(AuthKind::OAuth))?;
        let scheme = manifest
            .resolve_oauth_scheme(config.scheme.as_deref())
            .map_err(InjectError::from)?
            .clone();
        let id = credential_id(provider_name, auth, &scheme.key)?;
        let request = OAuthRequest::from_mount_config(Some(config), scheme)?;
        let domains = request.scheme().inject_domains.clone();
        service.register_oauth(id.clone(), request);
        Ok(Self {
            service,
            id,
            domains,
        })
    }

    async fn authorization(&self) -> Result<(String, String), InjectError> {
        let material = self
            .service
            .authorization(&self.id)
            .await
            .map_err(|error| InjectError::Unavailable(error.to_string()))?;
        Ok((
            material.name().to_string(),
            material.expose_value().to_string(),
        ))
    }

    fn health(&self) -> Option<CredentialHealth> {
        self.service.status(&self.id).map(|status| status.health)
    }

    fn credential_warning(&self) -> Option<String> {
        let status = self.service.status(&self.id)?;
        matches!(
            status.health,
            CredentialHealth::Missing | CredentialHealth::Expired | CredentialHealth::NeedsConsent
        )
        .then(|| format!("credential {} is {:?}", status.id, status.health))
    }
}

/// Maps a mount's optional auth config onto one binding over a shared
/// [`CredentialService`]. A no-auth mount has no binding.
pub struct AuthManager {
    binding: Option<AuthBinding>,
}

impl AuthManager {
    pub fn none() -> Self {
        Self { binding: None }
    }

    /// Build over the daemon-wide credential owner, registering the mount's
    /// single credential when auth is configured.
    pub(crate) fn from_config_manifest_service(
        config: Option<&Auth>,
        manifest: Option<&AuthManifest>,
        provider_name: &str,
        service: Arc<CredentialService>,
    ) -> Result<Self, InjectError> {
        Ok(Self {
            binding: config
                .map(|auth| AuthBinding::register(auth, manifest, provider_name, service))
                .transpose()?,
        })
    }

    /// Resolve the one host-owned authorization header for `url`. `None` means
    /// the mount's binding does not apply; a missing or unusable credential is
    /// a fail-closed error.
    pub async fn authorization_for(
        &self,
        url: &str,
    ) -> Result<Option<(String, String)>, InjectError> {
        let Some(binding) = self
            .binding
            .as_ref()
            .filter(|binding| binding.applies_to_url(url))
        else {
            return Ok(None);
        };
        binding.authorization().await.map(Some)
    }

    /// Live health for this mount's credential. `None` means the mount has no
    /// auth binding.
    pub(crate) fn health(&self) -> Option<CredentialHealth> {
        self.binding.as_ref()?.health()
    }

    pub async fn report_rejected_for_response(
        &self,
        url: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) -> RefreshOutcome {
        let Some(binding) = self
            .binding
            .as_ref()
            .filter(|binding| binding.applies_to_url(url))
        else {
            return RefreshOutcome::NotApplicable;
        };
        let evidence = RejectionEvidence::new(status.as_u16(), www_authenticate(headers));
        binding.service.report_rejected(&binding.id, evidence).await
    }
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
/// `NeedsConsent`. `None` for a no-auth mount or once the registered credential
/// is at least usable. The mount still loads either way; the
/// daemon surfaces the warning in the Mounts subsystem health without
/// blocking the mount on it.
pub(crate) fn build_time_credential_warning(manager: &AuthManager) -> Option<String> {
    manager.binding.as_ref()?.credential_warning()
}

fn www_authenticate(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let values = headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(", "))
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
