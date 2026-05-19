//! Instance configuration parsing and validation.
//!
//! Defines `InstanceConfig` for provider instantiation, including
//! plugin path, mount point, authentication, and capability grants.

pub mod schema;

use serde::Deserialize;

/// Configuration for a provider instance.
///
/// Loaded from JSON files in the providers configuration directory.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceConfig {
    pub plugin: String,
    pub mount: String,
    #[serde(default)]
    pub root_mount: bool,
    #[serde(default, deserialize_with = "deserialize_auth")]
    pub auth: Vec<AuthConfig>,
    pub capabilities: Option<CapabilitiesConfig>,
    #[serde(rename = "config")]
    pub config_raw: Option<serde_json::Value>,
}

/// Accepts both `[auth]` (single table) and `[[auth]]` (array of tables).
fn deserialize_auth<'de, D>(deserializer: D) -> Result<Vec<AuthConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(AuthConfig),
        Many(Vec<AuthConfig>),
    }
    match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Ok(Vec::new()),
        Some(OneOrMany::One(single)) => Ok(vec![single]),
        Some(OneOrMany::Many(vec)) => Ok(vec),
    }
}

/// Authentication configuration for HTTP requests.
///
/// Supports bearer-token and api-key-header authentication types.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    #[serde(rename = "type")]
    pub auth_type: String,
    pub token_env: Option<String>,
    pub token_file: Option<String>,
    pub domain: Option<String>,
    pub header: Option<String>,
    pub scopes: Option<Vec<String>>,
}

/// Capability grants and runtime caps for one provider instance.
///
/// Domain and git grants constrain which external resources a provider
/// can ask the host to reach. Blob caps bound host disk fetches and
/// guest-visible `read-blob` responses independently.
#[derive(Debug, Clone, Deserialize)]
pub struct CapabilitiesConfig {
    /// HTTPS domains the provider may fetch.
    pub domains: Option<Vec<String>>,
    /// Git remotes the provider may open.
    pub git_repos: Option<Vec<String>>,
    /// Absolute unix socket paths the provider may open via `unix:`
    /// URLs.
    pub unix_sockets: Option<Vec<String>>,
    /// Host directories exposed to the provider component through
    /// Wasmtime's WASI preopens. Each entry maps an absolute host
    /// path onto a guest path with read-only or read-write
    /// permissions. The empty default grants no filesystem access.
    pub preopened_paths: Option<Vec<PreopenedPath>>,
    /// Maximum memory granted to the provider component, in MiB.
    pub max_memory_mb: Option<u32>,
    /// Maximum response-body bytes accepted by a `fetch-blob` callout.
    pub max_fetch_blob_bytes: Option<u64>,
    /// Maximum bytes returned by one `read-blob` callout.
    pub max_read_blob_bytes: Option<u64>,
}

/// A single preopen mapping host path -> guest path, with the
/// requested access mode. The host validates absolute-path and
/// no-parent-escape invariants at provider build time.
#[derive(Debug, Clone, Deserialize)]
pub struct PreopenedPath {
    /// Absolute host path to expose.
    pub host: String,
    /// Path that the guest sees (also absolute).
    pub guest: String,
    /// Access mode for both directory and file operations.
    #[serde(default)]
    pub mode: PreopenMode,
}

/// Access mode for a preopened path. `Ro` (read-only) is the default
/// and grants `DirPerms::READ + FilePerms::READ`. `Rw` grants
/// `READ | MUTATE` for both.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PreopenMode {
    #[default]
    Ro,
    Rw,
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
        self.config_raw.as_ref().map_or_else(
            || b"{}".to_vec(),
            |value| serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {0}: {1}")]
    ReadFailed(String, std::io::Error),
    #[error("failed to parse config file {0}: {1}")]
    ParseFailed(String, serde_json::Error),
}
