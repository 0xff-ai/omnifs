use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PreopenedPath {
    pub host: String,
    pub guest: String,
    #[serde(default)]
    pub mode: PreopenMode,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PreopenMode {
    #[default]
    Ro,
    Rw,
}

/// Runtime capability grants for a mounted provider instance.
///
/// This type is user-authored in mount JSON configs and controls what
/// sandbox capabilities are granted. It is distinct from `CapabilityEntry`
/// which is the provider-manifest declaration of what a provider needs.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ProviderCapabilities {
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub git_repos: Option<Vec<String>>,
    #[serde(default)]
    pub unix_sockets: Option<Vec<String>>,
    #[serde(default)]
    pub preopened_paths: Option<Vec<PreopenedPath>>,
    #[serde(default)]
    pub max_memory_mb: Option<u32>,
    #[serde(default)]
    pub max_fetch_blob_bytes: Option<u64>,
    #[serde(default)]
    pub max_read_blob_bytes: Option<u64>,
}
