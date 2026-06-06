use std::fmt;

use serde::{Deserialize, Serialize};

/// Provider-specific configuration object from a mount JSON file's `"config"` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderConfig(serde_json::Value);

impl ProviderConfig {
    #[must_use]
    pub fn from_value(value: serde_json::Value) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    #[must_use]
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.0).unwrap_or_else(|_| b"{}".to_vec())
    }
}

/// Authentication configuration for HTTP requests.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Auth {
    StaticToken(StaticToken),
    #[serde(rename = "oauth")]
    OAuth(OAuth),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    StaticToken,
    OAuth,
}

impl fmt::Display for AuthKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaticToken => f.write_str("static-token"),
            Self::OAuth => f.write_str("oauth"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaticToken {
    /// Provider-declared auth scheme key for manifest-backed credentials.
    #[serde(default)]
    pub scheme: Option<String>,
    /// User-chosen account handle for host-managed credentials.
    #[serde(default)]
    pub account: Option<String>,
    pub token_env: Option<String>,
    pub token_file: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth {
    /// Provider-declared auth scheme key for manifest-backed credentials.
    #[serde(default)]
    pub scheme: Option<String>,
    /// User-chosen account handle for host-managed credentials.
    #[serde(default)]
    pub account: Option<String>,
    pub domain: Option<String>,
    pub header: Option<String>,
    /// OAuth public client id override for BYO OAuth applications.
    #[serde(default, alias = "clientId")]
    pub client_id: Option<String>,
    /// Environment variable containing an OAuth client secret.
    #[serde(default, alias = "clientSecretEnv")]
    pub client_secret_env: Option<String>,
    /// File containing an OAuth client secret.
    #[serde(default, alias = "clientSecretFile")]
    pub client_secret_file: Option<String>,
    /// OAuth redirect URI override for BYO apps that require an exact
    /// registered callback.
    #[serde(default, alias = "redirectUri")]
    pub redirect_uri: Option<String>,
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
    pub fn token_env(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.token_env.as_deref(),
            Self::OAuth(_) => None,
        }
    }

    #[must_use]
    pub fn token_file(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.token_file.as_deref(),
            Self::OAuth(_) => None,
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

pub fn deserialize_auth<'de, D>(deserializer: D) -> Result<Vec<Auth>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Box<Auth>),
        Many(Vec<Auth>),
    }
    match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Ok(Vec::new()),
        Some(OneOrMany::One(single)) => Ok(vec![*single]),
        Some(OneOrMany::Many(vec)) => Ok(vec),
    }
}
