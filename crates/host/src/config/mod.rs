//! Instance configuration parsing and validation.
//!
//! Defines `InstanceConfig` for provider instantiation, including
//! provider component, mount point, authentication, and capability grants.

pub mod mount_load;
pub mod schema;

use omnifs_mount_schema::ProviderCapabilities;
use serde::{Deserialize, Serialize};
use strum::Display;

/// Provider-specific configuration object from a mount JSON file's `"config"` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderConfigJson(serde_json::Value);

impl ProviderConfigJson {
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

/// Configuration for a provider instance.
///
/// Loaded from JSON files in the providers configuration directory.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstanceConfig {
    /// Filename of the provider WASM component this mount loads, looked
    /// up in `providers_dir`.
    pub provider: String,
    pub mount: String,
    /// Stable provider identity from the provider metadata custom section.
    /// This is runtime-derived, not a user-authored config field.
    #[serde(default, skip)]
    provider_id: Option<String>,
    /// Provider config schema from the provider metadata custom section.
    #[serde(default, skip)]
    provider_config_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub root_mount: bool,
    #[serde(default, deserialize_with = "deserialize_auth")]
    pub auth: Vec<AuthConfig>,
    pub capabilities: Option<ProviderCapabilities>,
    #[serde(rename = "config")]
    pub config_raw: Option<ProviderConfigJson>,
}

/// Runtime-ready provider configuration.
///
/// Unlike [`InstanceConfig`], this type has already incorporated provider
/// metadata and therefore always carries a stable provider id.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub provider: String,
    pub mount: String,
    pub provider_id: String,
    pub provider_config_schema: Option<serde_json::Value>,
    pub root_mount: bool,
    pub auth: Vec<AuthConfig>,
    pub capabilities: Option<ProviderCapabilities>,
    pub config_raw: Option<ProviderConfigJson>,
}

/// Accepts both `[auth]` (single table) and `[[auth]]` (array of tables).
fn deserialize_auth<'de, D>(deserializer: D) -> Result<Vec<AuthConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Box<AuthConfig>),
        Many(Vec<AuthConfig>),
    }
    match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Ok(Vec::new()),
        Some(OneOrMany::One(single)) => Ok(vec![*single]),
        Some(OneOrMany::Many(vec)) => Ok(vec),
    }
}

/// Authentication configuration for HTTP requests.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AuthConfig {
    StaticToken(StaticTokenConfig),
    #[serde(rename = "oauth")]
    OAuth(OAuthMountConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Display)]
#[strum(serialize_all = "kebab-case")]
pub enum AuthConfigKind {
    StaticToken,
    #[strum(serialize = "oauth")]
    OAuth,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaticTokenConfig {
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
pub struct OAuthMountConfig {
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

impl AuthConfig {
    pub fn kind(&self) -> AuthConfigKind {
        match self {
            Self::StaticToken(_) => AuthConfigKind::StaticToken,
            Self::OAuth(_) => AuthConfigKind::OAuth,
        }
    }

    pub fn scheme(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.scheme.as_deref(),
            Self::OAuth(config) => config.scheme.as_deref(),
        }
    }

    pub fn account(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.account.as_deref(),
            Self::OAuth(config) => config.account.as_deref(),
        }
    }

    pub fn token_env(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.token_env.as_deref(),
            Self::OAuth(_) => None,
        }
    }

    pub fn token_file(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.token_file.as_deref(),
            Self::OAuth(_) => None,
        }
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }

    pub fn as_oauth(&self) -> Option<&OAuthMountConfig> {
        match self {
            Self::OAuth(config) => Some(config),
            Self::StaticToken(_) => None,
        }
    }
}

impl InstanceConfig {
    pub fn parse(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::ReadFailed(path.display().to_string(), e))?;
        Self::parse(&content).map_err(|e| ConfigError::ParseFailed(path.display().to_string(), e))
    }

    pub fn config_bytes(&self) -> Vec<u8> {
        self.config_raw
            .as_ref()
            .map_or_else(|| b"{}".to_vec(), ProviderConfigJson::to_bytes)
    }

    pub fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }

    pub fn provider_config_schema(&self) -> Option<&serde_json::Value> {
        self.provider_config_schema.as_ref()
    }

    pub fn apply_provider_metadata(
        &mut self,
        manifest: &omnifs_mount_schema::ProviderManifest,
    ) -> Result<(), serde_json::Error> {
        self.provider_id = Some(manifest.id.clone());
        if self.auth.is_empty()
            && let Some(auth) = &manifest.auth
            && let Some(default_scheme) = auth.schemes.get(&auth.default)
        {
            let auth = match default_scheme {
                omnifs_mount_schema::ManifestAuthScheme::StaticToken(_) => {
                    AuthConfig::StaticToken(StaticTokenConfig {
                        scheme: Some(auth.default.clone()),
                        ..StaticTokenConfig::default()
                    })
                },
                omnifs_mount_schema::ManifestAuthScheme::Oauth(_) => {
                    AuthConfig::OAuth(OAuthMountConfig {
                        scheme: Some(auth.default.clone()),
                        ..OAuthMountConfig::default()
                    })
                },
            };
            self.auth.push(auth);
        }
        if self.capabilities.is_none() && !manifest.capabilities.is_empty() {
            self.capabilities = Some(manifest.provider_capabilities());
        }
        if let Some(schema) = manifest.config_schema.as_ref() {
            if self.config_raw.is_none() {
                let config = omnifs_mount_schema::ConfigSchema::parse(schema)
                    .map_err(serde::de::Error::custom)?
                    .defaults();
                self.config_raw = Some(ProviderConfigJson::from_value(config));
            }
            self.provider_config_schema = Some(schema.as_value().clone());
        }
        Ok(())
    }

    pub fn into_effective(
        mut self,
        fallback_provider_id: impl Into<String>,
        manifest: Option<&omnifs_mount_schema::ProviderManifest>,
    ) -> Result<EffectiveConfig, serde_json::Error> {
        if let Some(manifest) = manifest {
            self.apply_provider_metadata(manifest)?;
        }
        let provider_id = self
            .provider_id
            .clone()
            .unwrap_or_else(|| fallback_provider_id.into());
        Ok(EffectiveConfig {
            provider: self.provider,
            mount: self.mount,
            provider_id,
            provider_config_schema: self.provider_config_schema,
            root_mount: self.root_mount,
            auth: self.auth,
            capabilities: self.capabilities,
            config_raw: self.config_raw,
        })
    }
}

impl EffectiveConfig {
    pub fn config_bytes(&self) -> Vec<u8> {
        self.config_raw
            .as_ref()
            .map_or_else(|| b"{}".to_vec(), ProviderConfigJson::to_bytes)
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn provider_config_schema(&self) -> Option<&serde_json::Value> {
        self.provider_config_schema.as_ref()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {0}: {1}")]
    ReadFailed(String, std::io::Error),
    #[error("failed to parse config file {0}: {1}")]
    ParseFailed(String, serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR_METADATA_JSON: &str =
        include_str!("../../../../providers/linear/omnifs.provider.json");
    const GITHUB_METADATA_JSON: &str =
        include_str!("../../../../providers/github/omnifs.provider.json");

    fn linear_manifest() -> omnifs_mount_schema::ProviderManifest {
        omnifs_mount_schema::ProviderManifest::from_bytes(LINEAR_METADATA_JSON.as_bytes())
            .expect("linear manifest must parse")
    }

    fn github_manifest() -> omnifs_mount_schema::ProviderManifest {
        omnifs_mount_schema::ProviderManifest::from_bytes(GITHUB_METADATA_JSON.as_bytes())
            .expect("github manifest must parse")
    }

    #[test]
    fn linear_manifest_parses_with_static_token_scheme() {
        let manifest = linear_manifest();
        let auth = manifest.auth.as_ref().expect("linear auth block");
        let pat = auth.schemes.get("pat").expect("linear pat scheme");
        assert!(matches!(
            pat,
            omnifs_mount_schema::ManifestAuthScheme::StaticToken(_)
        ));
        let omnifs_mount_schema::ManifestAuthScheme::StaticToken(static_token) = pat else {
            unreachable!()
        };
        assert!(static_token.creation_url.is_some());
        let val = static_token.validation.as_ref().expect("validation");
        assert_eq!(val.expect_status, 200);
        assert_eq!(val.json_pointer.as_deref(), Some("/data/viewer/id"));
    }

    #[test]
    fn github_manifest_parses_with_static_token_scheme() {
        let manifest = github_manifest();
        let auth = manifest.auth.as_ref().expect("github auth block");
        let pat = auth.schemes.get("pat").expect("github pat scheme");
        let omnifs_mount_schema::ManifestAuthScheme::StaticToken(static_token) = pat else {
            panic!("expected static token");
        };
        assert_eq!(auth.inject.prefix, "Bearer ");
        let val = static_token.validation.as_ref().expect("validation");
        assert_eq!(val.method, "GET");
        assert_eq!(val.expect_status, 200);
    }

    #[test]
    fn thin_config_inherits_provider_metadata_defaults() {
        let manifest = linear_manifest();
        let cfg = InstanceConfig::parse(
            r#"{
                "provider": "omnifs_provider_linear.wasm",
                "mount": "linear"
            }"#,
        )
        .expect("minimal config must parse");

        let cfg = cfg
            .into_effective("omnifs_provider_linear", Some(&manifest))
            .unwrap();

        assert_eq!(cfg.provider_id(), "linear");
        assert_eq!(cfg.auth.len(), 1);
        assert!(cfg.auth[0].is_oauth());
        assert_eq!(cfg.auth[0].scheme(), Some("oauth"));
        assert_eq!(
            cfg.capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.max_memory_mb),
            Some(128),
        );
    }
}
