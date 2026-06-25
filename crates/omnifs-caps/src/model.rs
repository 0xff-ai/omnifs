//! The capability data model: what a provider needs, what a mount grants, and
//! the literal-or-dynamic grant shape they share.

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
    pub fn new() -> Self {
        Self { dynamic: true }
    }
}

impl Default for DynamicMarker {
    fn default() -> Self {
        Self::new()
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

/// The capabilities a mount spec grants a provider. The spec, not the manifest,
/// is the runtime grant authority; the manifest declares [`Need`]s only. Each
/// list-valued field is a [`Grant`] (literal or dynamic); scalar resource
/// limits are plain values.
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
    #[serde(default)]
    pub max_memory_mb: Option<u32>,
    #[serde(default)]
    pub max_fetch_blob_bytes: Option<u64>,
    #[serde(default)]
    pub max_read_blob_bytes: Option<u64>,
}

/// One capability a provider *needs*, declared via `capabilities(..)` in
/// `#[omnifs_sdk::provider]` and embedded in the `omnifs.provider-metadata.v1`
/// section. A need is never a grant: the host checks a mount's [`Grants`]
/// against these, it never grants from them at runtime.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Need {
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
    MemoryMb {
        value: u32,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    FetchBlobBytes {
        value: u64,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    ReadBlobBytes {
        value: u64,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
}

impl Need {
    /// The provider's justification for needing this capability.
    #[must_use]
    pub fn why(&self) -> &str {
        match self {
            Self::Domain { why, .. }
            | Self::GitRepo { why, .. }
            | Self::UnixSocket { why, .. }
            | Self::PreopenedPath { why, .. }
            | Self::MemoryMb { why, .. }
            | Self::FetchBlobBytes { why, .. }
            | Self::ReadBlobBytes { why, .. } => why,
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
            | Self::PreopenedPath { dynamic, .. }
            | Self::MemoryMb { dynamic, .. }
            | Self::FetchBlobBytes { dynamic, .. }
            | Self::ReadBlobBytes { dynamic, .. } => *dynamic,
        }
    }
}

/// A capability a provider's manifest needs that a mount's [`Grants`] do not
/// satisfy, surfaced when an under-granted mount is rejected at provider start.
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

        let omitted: Need = serde_json::from_str(
            r#"{"kind":"domain","value":"api.example.com","why":"fetch data"}"#,
        )
        .unwrap();
        assert!(!omitted.is_dynamic());

        let explicit: Need = serde_json::from_str(
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
