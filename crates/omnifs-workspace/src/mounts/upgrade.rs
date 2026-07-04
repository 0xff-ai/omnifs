//! Provider upgrade classification.
//!
//! When a newer provider artifact is installed for the same name, `omnifs up`
//! diffs the pinned artifact's manifest against the candidate's to classify the
//! change and route it: nothing (identical), auto-migrate (additive optional
//! config), or hard error (breaking config, or a capability/auth change that
//! needs explicit re-consent). The comparison is structural and operates on two
//! live [`ProviderManifest`]s loaded from the two artifacts, so no snapshot is
//! stamped into the mount spec.

use std::collections::{HashMap, HashSet};

use crate::provider::ProviderManifest;

/// How a candidate provider artifact differs from the pinned one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradePlan {
    /// No relevant change; keep serving the pinned artifact (or repin freely).
    Identical,
    /// New optional config fields with defaults; safe to auto-migrate by filling
    /// the defaults and repinning.
    AdditiveConfig { added: Vec<AddedField> },
    /// Config changed in a breaking way (removed field, new required field,
    /// became-required, rename); the user must re-init.
    BreakingConfig { changes: Vec<FieldChange> },
    /// A capability, scalar limit, or auth scheme changed; requires explicit
    /// re-consent.
    CapabilityLimitOrAuth {
        capabilities: Vec<CapabilityChange>,
        limits: Vec<LimitChange>,
        auth: Option<AuthDelta>,
    },
}

impl UpgradePlan {
    /// Classify the difference between the `old` (pinned) and `new` (candidate)
    /// provider manifests. Capability/auth changes dominate, then breaking
    /// config, then additive config.
    #[must_use]
    pub fn diff(old: &ProviderManifest, new: &ProviderManifest) -> Self {
        let old_caps = extract_capabilities(old);
        let new_caps = extract_capabilities(new);
        let old_limits = extract_limits(old);
        let new_limits = extract_limits(new);
        let old_auth = old.auth.as_ref().map(|auth| auth.default.clone());
        let new_auth = new.auth.as_ref().map(|auth| auth.default.clone());

        let caps_changed = normalize_caps(&old_caps) != normalize_caps(&new_caps);
        let limits_changed = normalize_limits(&old_limits) != normalize_limits(&new_limits);
        let auth_changed = old_auth != new_auth;
        if caps_changed || limits_changed || auth_changed {
            return Self::CapabilityLimitOrAuth {
                capabilities: if caps_changed {
                    diff_capabilities(&old_caps, &new_caps)
                } else {
                    Vec::new()
                },
                limits: if limits_changed {
                    diff_limits(&old_limits, &new_limits)
                } else {
                    Vec::new()
                },
                auth: auth_changed.then_some(AuthDelta {
                    old: old_auth,
                    new: new_auth,
                }),
            };
        }

        let changes = diff_fields(&extract_config_fields(old), &extract_config_fields(new));
        if changes.is_empty() {
            return Self::Identical;
        }
        let all_additive = changes.iter().all(|change| {
            matches!(
                change,
                FieldChange::Added {
                    required: false,
                    ..
                }
            )
        });
        if all_additive {
            let added = changes
                .into_iter()
                .filter_map(|change| match change {
                    FieldChange::Added { name, default, .. } => Some(AddedField { name, default }),
                    _ => None,
                })
                .collect();
            Self::AdditiveConfig { added }
        } else {
            Self::BreakingConfig { changes }
        }
    }
}

/// A new optional config field added by the candidate, with its default value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedField {
    pub name: String,
    pub default: Option<serde_json::Value>,
}

/// A single config-field change between two manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldChange {
    Added {
        name: String,
        required: bool,
        default: Option<serde_json::Value>,
    },
    Removed(String),
    BecameRequired(String),
    BecameOptional(String),
}

impl FieldChange {
    /// Human-readable description for upgrade prompts.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Added {
                name,
                required: true,
                ..
            } => format!("new required field `{name}`"),
            Self::Added {
                name,
                required: false,
                ..
            } => format!("new optional field `{name}`"),
            Self::Removed(name) => format!("removed field `{name}`"),
            Self::BecameRequired(name) => format!("`{name}` is now required"),
            Self::BecameOptional(name) => format!("`{name}` is now optional"),
        }
    }
}

/// A capability change between two manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityChange {
    pub kind: String,
    pub value: String,
    pub direction: CapabilityDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityDirection {
    Added,
    Removed,
}

/// A scalar runtime-limit change between two manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitChange {
    pub name: String,
    pub value: String,
    pub direction: LimitDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitDirection {
    Added,
    Removed,
}

/// An auth-scheme change between two manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthDelta {
    pub old: Option<String>,
    pub new: Option<String>,
}

// ── Extraction ──────────────────────────────────────────────────────────────

struct Field {
    name: String,
    required: bool,
    default: Option<serde_json::Value>,
}

fn extract_config_fields(manifest: &ProviderManifest) -> Vec<Field> {
    let Some(config) = manifest.config.as_ref() else {
        return Vec::new();
    };
    config
        .fields
        .iter()
        .map(|field| Field {
            name: field.name.clone(),
            required: field.required,
            default: field.default.clone(),
        })
        .collect()
}

/// A capability flattened for diffing: kind, value, and whether it resolves
/// dynamically. The dynamic flag is part of the security surface: flipping a
/// need between static and dynamic changes how its value is resolved, so it must
/// route to re-consent like any other capability change.
type FlatCapability = (String, String, bool);
type FlatLimit = (String, String);

fn extract_capabilities(manifest: &ProviderManifest) -> Vec<FlatCapability> {
    manifest
        .capabilities
        .iter()
        .map(|entry| (entry.kind().to_string(), entry.value(), entry.is_dynamic()))
        .collect()
}

fn extract_limits(manifest: &ProviderManifest) -> Vec<FlatLimit> {
    let mut limits = Vec::new();
    if let Some(limit) = &manifest.limits.max_memory_mb {
        limits.push(("maxMemoryMb".to_string(), format!("{} MiB", limit.value)));
    }
    if let Some(limit) = &manifest.limits.max_fetch_blob_bytes {
        limits.push(("maxFetchBlobBytes".to_string(), limit.value.to_string()));
    }
    if let Some(limit) = &manifest.limits.max_read_blob_bytes {
        limits.push(("maxReadBlobBytes".to_string(), limit.value.to_string()));
    }
    limits
}

// ── Diff helpers ────────────────────────────────────────────────────────────

fn normalize_caps(caps: &[FlatCapability]) -> Vec<FlatCapability> {
    let mut sorted = caps.to_vec();
    sorted.sort();
    sorted
}

fn normalize_limits(limits: &[FlatLimit]) -> Vec<FlatLimit> {
    let mut sorted = limits.to_vec();
    sorted.sort();
    sorted
}

fn diff_capabilities(old: &[FlatCapability], new: &[FlatCapability]) -> Vec<CapabilityChange> {
    let old_set: HashSet<&FlatCapability> = old.iter().collect();
    let new_set: HashSet<&FlatCapability> = new.iter().collect();
    let mut changes: Vec<CapabilityChange> = old
        .iter()
        .filter(|cap| !new_set.contains(cap))
        .map(|cap| change(cap, CapabilityDirection::Removed))
        .chain(
            new.iter()
                .filter(|cap| !old_set.contains(cap))
                .map(|cap| change(cap, CapabilityDirection::Added)),
        )
        .collect();
    changes.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.value.cmp(&b.value)));
    changes
}

fn diff_limits(old: &[FlatLimit], new: &[FlatLimit]) -> Vec<LimitChange> {
    let old_set: HashSet<&FlatLimit> = old.iter().collect();
    let new_set: HashSet<&FlatLimit> = new.iter().collect();
    let mut changes: Vec<LimitChange> = old
        .iter()
        .filter(|limit| !new_set.contains(limit))
        .map(|limit| limit_change(limit, LimitDirection::Removed))
        .chain(
            new.iter()
                .filter(|limit| !old_set.contains(limit))
                .map(|limit| limit_change(limit, LimitDirection::Added)),
        )
        .collect();
    changes.sort_by(|a, b| a.name.cmp(&b.name).then(a.value.cmp(&b.value)));
    changes
}

fn change(cap: &FlatCapability, direction: CapabilityDirection) -> CapabilityChange {
    let (kind, value, dynamic) = cap;
    CapabilityChange {
        kind: kind.clone(),
        // Surface the dynamic state in the value so a static↔dynamic flip reads
        // clearly in the re-consent prompt rather than as an identical value
        // appearing on both sides.
        value: if *dynamic {
            format!("{value} (dynamic)")
        } else {
            value.clone()
        },
        direction,
    }
}

fn limit_change(limit: &FlatLimit, direction: LimitDirection) -> LimitChange {
    let (name, value) = limit;
    LimitChange {
        name: name.clone(),
        value: value.clone(),
        direction,
    }
}

fn diff_fields(old: &[Field], new: &[Field]) -> Vec<FieldChange> {
    let old_map: HashMap<&str, bool> = old.iter().map(|f| (f.name.as_str(), f.required)).collect();
    let new_map: HashMap<&str, bool> = new.iter().map(|f| (f.name.as_str(), f.required)).collect();

    let mut changes = Vec::new();
    for (name, required) in &old_map {
        match new_map.get(name) {
            Some(&new_required) if *required && !new_required => {
                changes.push(FieldChange::BecameOptional((*name).to_string()));
            },
            Some(&new_required) if !required && new_required => {
                changes.push(FieldChange::BecameRequired((*name).to_string()));
            },
            Some(_) => {},
            None => changes.push(FieldChange::Removed((*name).to_string())),
        }
    }
    for field in new {
        if !old_map.contains_key(field.name.as_str()) {
            changes.push(FieldChange::Added {
                name: field.name.clone(),
                required: field.required,
                default: field.default.clone(),
            });
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a manifest with the given config fields for diff classification tests.
    fn manifest(fields: &serde_json::Value) -> ProviderManifest {
        let json = serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "provider": "omnifs_provider_demo.wasm",
            "defaultMount": "demo",
            "capabilities": [],
            "config": {
                "fields": fields,
            }
        });
        ProviderManifest::from_bytes(json.to_string().as_bytes()).expect("manifest parses")
    }

    fn manifest_with_caps(capabilities: &serde_json::Value) -> ProviderManifest {
        let json = serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "provider": "omnifs_provider_demo.wasm",
            "defaultMount": "demo",
            "capabilities": capabilities,
        });
        ProviderManifest::from_bytes(json.to_string().as_bytes()).expect("manifest parses")
    }

    fn manifest_with_limits(limits: &serde_json::Value) -> ProviderManifest {
        let json = serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "provider": "omnifs_provider_demo.wasm",
            "defaultMount": "demo",
            "limits": limits,
        });
        ProviderManifest::from_bytes(json.to_string().as_bytes()).expect("manifest parses")
    }

    #[test]
    fn capability_upgrade_diff() {
        let docker_sock = serde_json::json!([
            {"kind": "unixSocket", "value": "/var/run/docker.sock", "why": "docker", "dynamic": false}
        ]);
        let docker_sock_dynamic = serde_json::json!([
            {"kind": "unixSocket", "value": "/var/run/docker.sock", "why": "docker", "dynamic": true}
        ]);

        for (label, old_caps, new_caps) in [
            ("static to dynamic", &docker_sock, &docker_sock_dynamic),
            ("dynamic to static", &docker_sock_dynamic, &docker_sock),
        ] {
            let old = manifest_with_caps(old_caps);
            let new = manifest_with_caps(new_caps);
            assert!(
                matches!(
                    UpgradePlan::diff(&old, &new),
                    UpgradePlan::CapabilityLimitOrAuth { .. }
                ),
                "{label}"
            );
        }
    }

    #[test]
    fn limit_upgrade_diff() {
        let old = manifest_with_limits(&serde_json::json!({
            "maxMemoryMb": { "value": 64, "why": "memory" }
        }));
        let new = manifest_with_limits(&serde_json::json!({
            "maxMemoryMb": { "value": 128, "why": "memory" }
        }));

        match UpgradePlan::diff(&old, &new) {
            UpgradePlan::CapabilityLimitOrAuth {
                capabilities,
                limits,
                auth,
            } => {
                assert!(capabilities.is_empty());
                assert_eq!(
                    limits,
                    vec![
                        LimitChange {
                            name: "maxMemoryMb".to_string(),
                            value: "128 MiB".to_string(),
                            direction: LimitDirection::Added,
                        },
                        LimitChange {
                            name: "maxMemoryMb".to_string(),
                            value: "64 MiB".to_string(),
                            direction: LimitDirection::Removed,
                        },
                    ]
                );
                assert_eq!(auth, None);
            },
            other => panic!("expected CapabilityLimitOrAuth, got {other:?}"),
        }
    }

    #[test]
    fn config_metadata_upgrade_diff() {
        let base_fields = serde_json::json!([
            { "name": "endpoint", "type": { "kind": "string" }, "required": true, "default": "x" }
        ]);
        let base = manifest(&base_fields);

        let optional_fields = serde_json::json!([
            { "name": "endpoint", "type": { "kind": "string" }, "required": true, "default": "x" },
            { "name": "timeout_secs", "type": { "kind": "integer" }, "default": 30 }
        ]);
        let with_optional = manifest(&optional_fields);
        match UpgradePlan::diff(&base, &with_optional) {
            UpgradePlan::AdditiveConfig { added } => {
                assert_eq!(added.len(), 1);
                assert_eq!(added[0].name, "timeout_secs");
                assert_eq!(added[0].default, Some(serde_json::json!(30)));
            },
            other => panic!("expected AdditiveConfig, got {other:?}"),
        }

        let required_fields = serde_json::json!([
            { "name": "endpoint", "type": { "kind": "string" }, "required": true, "default": "x" },
            { "name": "api_key", "type": { "kind": "string" }, "required": true }
        ]);
        let with_required = manifest(&required_fields);
        assert!(matches!(
            UpgradePlan::diff(&base, &with_required),
            UpgradePlan::BreakingConfig { .. }
        ));

        let required_default_fields = serde_json::json!([
            { "name": "endpoint", "type": { "kind": "string" }, "required": true, "default": "x" },
            { "name": "region", "type": { "kind": "string" }, "required": true, "default": "us-east-1" }
        ]);
        let with_required_default = manifest(&required_default_fields);
        assert!(matches!(
            UpgradePlan::diff(&base, &with_required_default),
            UpgradePlan::BreakingConfig { .. }
        ));
    }

    #[test]
    fn removed_field_is_breaking() {
        let old_fields = serde_json::json!([
            { "name": "endpoint", "type": { "kind": "string" }, "required": true, "default": "x" },
            { "name": "timeout_secs", "type": { "kind": "integer" }, "default": 30 }
        ]);
        let old = manifest(&old_fields);
        let new_fields = serde_json::json!([
            { "name": "endpoint", "type": { "kind": "string" }, "required": true, "default": "x" }
        ]);
        let new = manifest(&new_fields);
        assert!(matches!(
            UpgradePlan::diff(&old, &new),
            UpgradePlan::BreakingConfig { .. }
        ));
    }
}
