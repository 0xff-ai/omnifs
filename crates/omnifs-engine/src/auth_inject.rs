//! Mount configuration to startup-owned auth binding.

use omnifs_auth::{AuthBinding, AuthError, CredentialService, OAuthClient, OAuthRequest};
use omnifs_workspace::authn::{AuthKind, AuthManifest, CredentialId, SchemeResolveError};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::ids::ProviderName;
use omnifs_workspace::mounts::Auth;
use std::path::Path;
use std::sync::Arc;

pub(crate) fn credential_service_for_file(
    credentials_file: &Path,
) -> Result<Arc<CredentialService>, InjectError> {
    let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(credentials_file));
    Ok(Arc::new(CredentialService::new(store, OAuthClient::new()?)))
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
    #[error("auth error: {0}")]
    Auth(#[from] AuthError),
}

pub(crate) fn binding_from_config(
    config: Option<&Auth>,
    manifest: Option<&AuthManifest>,
    provider_name: &str,
    service: &Arc<CredentialService>,
) -> Result<Option<Arc<AuthBinding>>, InjectError> {
    let Some(auth) = config else {
        return Ok(None);
    };
    let binding = match auth {
        Auth::StaticToken(config) => {
            let manifest = manifest.ok_or(InjectError::ManifestRequired(AuthKind::StaticToken))?;
            let scheme = manifest
                .resolve_static_scheme(config.scheme.as_deref())
                .map_err(InjectError::from)?;
            let id = credential_id(provider_name, auth, &scheme.key)?;
            let header_name = scheme
                .header_name
                .clone()
                .unwrap_or_else(|| "Authorization".to_string());
            service.bind_static(
                id,
                scheme.inject_domains.clone(),
                header_name,
                scheme.value_prefix.clone(),
            )?
        },
        Auth::OAuth(config) => {
            let manifest = manifest.ok_or(InjectError::ManifestRequired(AuthKind::OAuth))?;
            let scheme = manifest
                .resolve_oauth_scheme(config.scheme.as_deref())
                .map_err(InjectError::from)?
                .clone();
            let id = credential_id(provider_name, auth, &scheme.key)?;
            let request = OAuthRequest::from_mount_config(Some(config), scheme)?;
            let header_name = request
                .scheme()
                .inject_header_name
                .clone()
                .unwrap_or_else(|| "Authorization".to_string());
            let domains = request.scheme().inject_domains.clone();
            let value_prefix = request.scheme().inject_value_prefix.clone();
            service.bind_oauth(id, request, domains, header_name, value_prefix)?
        },
    };
    Ok(Some(Arc::new(binding)))
}

fn credential_id(
    provider_name: &str,
    auth: &Auth,
    scheme: &str,
) -> Result<CredentialId, InjectError> {
    let provider = ProviderName::new(provider_name)
        .map_err(|error| InjectError::CredentialId(error.to_string()))?;
    CredentialId::for_mount(&provider, auth, scheme)
        .map_err(|error| InjectError::CredentialId(error.to_string()))
}

pub(crate) fn www_authenticate(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let values = headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(", "))
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
