//! The sparse user-authored auth block for a mount `Spec`: a single [`Auth`]
//! selection (static token or OAuth) plus typed read accessors. The provider
//! config is intentionally not modeled here: the provider owns its meaning, so
//! it stays an opaque `serde_json::Value` on the spec.

use serde::{Deserialize, Serialize};

pub use crate::authn::AuthKind;

/// Authentication configuration for HTTP requests.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Auth {
    StaticToken(StaticToken),
    #[serde(rename = "oauth")]
    OAuth(OAuth),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaticToken {
    /// Provider-declared auth scheme key for manifest-backed credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// User-chosen account handle for host-managed credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth {
    /// Provider-declared auth scheme key for manifest-backed credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// User-chosen account handle for host-managed credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// OAuth public client id override for BYO OAuth applications.
    #[serde(default, alias = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Environment variable containing an OAuth client secret.
    #[serde(
        default,
        alias = "clientSecretEnv",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret_env: Option<String>,
    /// File containing an OAuth client secret.
    #[serde(
        default,
        alias = "clientSecretFile",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret_file: Option<String>,
    /// OAuth redirect URI override for BYO apps that require an exact
    /// registered callback.
    #[serde(
        default,
        alias = "redirectUri",
        skip_serializing_if = "Option::is_none"
    )]
    pub redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

impl Auth {
    #[must_use]
    pub fn kind(&self) -> AuthKind {
        match self {
            Self::StaticToken(_) => AuthKind::StaticToken,
            Self::OAuth(_) => AuthKind::OAuth,
        }
    }

    #[must_use]
    pub fn scheme(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.scheme.as_deref(),
            Self::OAuth(config) => config.scheme.as_deref(),
        }
    }

    #[must_use]
    pub fn account(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.account.as_deref(),
            Self::OAuth(config) => config.account.as_deref(),
        }
    }

    #[must_use]
    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }

    #[must_use]
    pub fn as_oauth(&self) -> Option<&OAuth> {
        match self {
            Self::OAuth(config) => Some(config),
            Self::StaticToken(_) => None,
        }
    }
}
