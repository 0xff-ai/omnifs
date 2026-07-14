//! Provider manifest declarations and mount-owned scalar resource limits.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Access mode for a preopened directory declared by a provider.
#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum PreopenMode {
    #[default]
    Ro,
    Rw,
}

/// A host directory exposed into the provider sandbox at a guest path.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
pub struct PreopenedPath {
    pub host: String,
    pub guest: String,
    #[serde(default)]
    pub mode: PreopenMode,
}

/// A scalar resource ceiling a provider declares in its manifest.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimit<T> {
    pub value: T,
    pub why: String,
}

/// Scalar resource limits declared by a provider manifest.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LimitDeclarations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<ResourceLimit<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fetch_blob_bytes: Option<ResourceLimit<u64>>,
}

impl LimitDeclarations {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_memory_mb.is_none() && self.max_fetch_blob_bytes.is_none()
    }
}

/// Runtime scalar resource ceilings owned by a mount spec.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fetch_blob_bytes: Option<u64>,
}

impl Limits {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_memory_mb.is_none() && self.max_fetch_blob_bytes.is_none()
    }

    #[must_use]
    pub fn from_declarations(declarations: &LimitDeclarations) -> Self {
        Self {
            max_memory_mb: declarations.max_memory_mb.as_ref().map(|limit| limit.value),
            max_fetch_blob_bytes: declarations
                .max_fetch_blob_bytes
                .as_ref()
                .map(|limit| limit.value),
        }
    }
}

/// One provider access declaration embedded in its manifest.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum AccessNeed {
    Domain {
        value: String,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    GitRepo {
        value: String,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    UnixSocket {
        value: String,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    PreopenedPath {
        value: PreopenedPath,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
}

impl AccessNeed {
    #[must_use]
    pub fn why(&self) -> &str {
        match self {
            Self::Domain { why, .. }
            | Self::GitRepo { why, .. }
            | Self::UnixSocket { why, .. }
            | Self::PreopenedPath { why, .. } => why,
        }
    }

    #[must_use]
    pub fn is_dynamic(&self) -> bool {
        match self {
            Self::Domain { dynamic, .. }
            | Self::GitRepo { dynamic, .. }
            | Self::UnixSocket { dynamic, .. }
            | Self::PreopenedPath { dynamic, .. } => *dynamic,
        }
    }

    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Domain { .. } => "domain",
            Self::GitRepo { .. } => "gitRepo",
            Self::UnixSocket { .. } => "unixSocket",
            Self::PreopenedPath { .. } => "preopenedPath",
        }
    }

    #[must_use]
    pub fn value(&self) -> String {
        match self {
            Self::Domain { value, .. }
            | Self::GitRepo { value, .. }
            | Self::UnixSocket { value, .. } => value.clone(),
            Self::PreopenedPath { value, .. } => format!("{} -> {}", value.host, value.guest),
        }
    }
}
