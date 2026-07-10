//! The capability and limit data model: what provider access a mount grants,
//! what scalar resource ceilings a mount applies, and the literal-or-dynamic
//! grant shape access declarations share.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A grant for one list-valued capability: either an explicit list of allowed
/// values, or a marker that the grant is resolved dynamically at mount
/// initialization (e.g. a unix socket derived from the mount's endpoint). A
/// field is wholly literal or wholly dynamic, never a mix of the two.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(untagged)]
pub enum Grant<T> {
    Literal(Vec<T>),
    Dynamic(DynamicMarker),
}

impl<T> Grant<T> {
    /// Whether this grant is resolved dynamically at init.
    #[must_use]
    pub fn is_dynamic(&self) -> bool {
        matches!(self, Self::Dynamic(_))
    }

    /// The explicit values, or an empty slice when the grant is dynamic.
    #[must_use]
    pub fn literal(&self) -> &[T] {
        match self {
            Self::Literal(values) => values,
            Self::Dynamic(_) => &[],
        }
    }
}

/// The `{ "dynamic": true }` marker for a dynamically-resolved grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DynamicMarker {
    /// Always `true`; the field exists so the wire form reads `{ "dynamic": true }`.
    pub dynamic: bool,
}

impl DynamicMarker {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self { dynamic: true }
    }
}

/// Access mode for a preopened directory grant.
#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum PreopenMode {
    #[default]
    Ro,
    Rw,
}

impl PreopenMode {
    /// Whether a grant of `self` mode covers a need for `required` mode: `Rw`
    /// covers both, `Ro` covers only `Ro`.
    #[must_use]
    pub fn covers(self, required: PreopenMode) -> bool {
        matches!(
            (self, required),
            (PreopenMode::Rw, _) | (PreopenMode::Ro, PreopenMode::Ro)
        )
    }
}

/// A host directory exposed into the provider sandbox at a guest path.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
pub struct PreopenedPath {
    pub host: String,
    pub guest: String,
    #[serde(default)]
    pub mode: PreopenMode,
}

/// The access capabilities a mount spec grants a provider. The spec, not the
/// manifest, is the runtime grant authority; the manifest declares
/// [`AccessNeed`]s only. Each field is a [`Grant`] (literal or dynamic).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
pub struct Grants {
    #[serde(default)]
    pub domains: Option<Grant<String>>,
    #[serde(default)]
    pub git_repos: Option<Grant<String>>,
    #[serde(default)]
    pub unix_sockets: Option<Grant<String>>,
    #[serde(default)]
    pub preopened_paths: Option<Grant<PreopenedPath>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_read_blob_bytes: Option<ResourceLimit<u64>>,
}

impl LimitDeclarations {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_memory_mb.is_none()
            && self.max_fetch_blob_bytes.is_none()
            && self.max_read_blob_bytes.is_none()
    }
}

/// Runtime scalar resource ceilings owned by a mount spec.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fetch_blob_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_read_blob_bytes: Option<u64>,
}

impl Limits {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_memory_mb.is_none()
            && self.max_fetch_blob_bytes.is_none()
            && self.max_read_blob_bytes.is_none()
    }

    #[must_use]
    pub fn from_declarations(declarations: &LimitDeclarations) -> Self {
        Self {
            max_memory_mb: declarations.max_memory_mb.as_ref().map(|limit| limit.value),
            max_fetch_blob_bytes: declarations
                .max_fetch_blob_bytes
                .as_ref()
                .map(|limit| limit.value),
            max_read_blob_bytes: declarations
                .max_read_blob_bytes
                .as_ref()
                .map(|limit| limit.value),
        }
    }
}

/// One access capability a provider *needs*, declared via `capabilities(..)` in
/// `#[omnifs_sdk::provider]` and embedded in the `omnifs.provider-metadata.v1`
/// section. A need is never a grant: the host checks a mount's [`Grants`]
/// against these, it never grants from them at runtime.
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
    /// The provider's justification for needing this capability.
    #[must_use]
    pub fn why(&self) -> &str {
        match self {
            Self::Domain { why, .. }
            | Self::GitRepo { why, .. }
            | Self::UnixSocket { why, .. }
            | Self::PreopenedPath { why, .. } => why,
        }
    }

    /// Whether the concrete value is resolved at mount init (e.g. a socket from
    /// the endpoint) rather than declared statically.
    #[must_use]
    pub fn is_dynamic(&self) -> bool {
        match self {
            Self::Domain { dynamic, .. }
            | Self::GitRepo { dynamic, .. }
            | Self::UnixSocket { dynamic, .. }
            | Self::PreopenedPath { dynamic, .. } => *dynamic,
        }
    }

    /// The capability-kind discriminant, identical to the manifest wire tag.
    /// The single source of truth for the kind string across the under-grant
    /// check ([`Missing`]) and upgrade-diff surfaces; keep it aligned with the
    /// serde `tag` rename on this enum.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Domain { .. } => "domain",
            Self::GitRepo { .. } => "gitRepo",
            Self::UnixSocket { .. } => "unixSocket",
            Self::PreopenedPath { .. } => "preopenedPath",
        }
    }

    /// The capability value rendered for display: the literal value for access
    /// kinds and `host -> guest` for a preopen. The dynamic marker is not
    /// included; callers that surface dynamic-ness append it. Shared so a
    /// preopen renders the same way in every host-facing message.
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

/// An access capability a provider's manifest needs that a mount's [`Grants`]
/// do not satisfy, surfaced when an under-granted mount is rejected at provider
/// start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missing {
    pub kind: &'static str,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_and_need_json_shapes() {
        let literal: Grant<String> =
            serde_json::from_str(r#"["api.github.com","github.com"]"#).unwrap();
        assert_eq!(
            literal,
            Grant::Literal(vec!["api.github.com".into(), "github.com".into()])
        );
        assert!(!literal.is_dynamic());

        let dynamic: Grant<String> = serde_json::from_str(r#"{"dynamic":true}"#).unwrap();
        assert!(dynamic.is_dynamic());
        assert!(dynamic.literal().is_empty());

        let omitted: AccessNeed = serde_json::from_str(
            r#"{"kind":"domain","value":"api.example.com","why":"fetch data"}"#,
        )
        .unwrap();
        assert!(!omitted.is_dynamic());

        let explicit: AccessNeed = serde_json::from_str(
            r#"{"kind":"unixSocket","value":"configured socket","dynamic":true,"why":"connect"}"#,
        )
        .unwrap();
        assert!(explicit.is_dynamic());
        assert_eq!(serde_json::to_value(&explicit).unwrap()["dynamic"], true);
    }

    #[test]
    fn rw_covers_ro_but_not_the_reverse() {
        assert!(PreopenMode::Rw.covers(PreopenMode::Ro));
        assert!(PreopenMode::Rw.covers(PreopenMode::Rw));
        assert!(PreopenMode::Ro.covers(PreopenMode::Ro));
        assert!(!PreopenMode::Ro.covers(PreopenMode::Rw));
    }
}
