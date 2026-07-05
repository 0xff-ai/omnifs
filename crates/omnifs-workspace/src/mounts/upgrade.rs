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

use serde::{Deserialize, Serialize};

use crate::provider::ProviderAuthManifest;
use crate::provider::ProviderManifest;
use crate::provider::config::ConfigType;

/// How a candidate provider artifact differs from the pinned one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
        let old_auth = old.auth.as_ref().map(AuthSurface::from_manifest);
        let new_auth = new.auth.as_ref().map(AuthSurface::from_manifest);

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

    #[must_use]
    pub fn requires_approval(&self) -> bool {
        matches!(
            self,
            Self::BreakingConfig { .. } | Self::CapabilityLimitOrAuth { .. }
        )
    }

    #[must_use]
    pub fn covers(&self, actual: &Self) -> bool {
        !actual.requires_approval() || self == actual
    }
}

/// A new optional config field added by the candidate, with its default value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedField {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// A single config-field change between two manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldChange {
    Added {
        name: String,
        required: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<serde_json::Value>,
    },
    Removed {
        name: String,
    },
    BecameRequired {
        name: String,
    },
    BecameOptional {
        name: String,
    },
    TypeChanged {
        name: String,
        old: serde_json::Value,
        new: serde_json::Value,
    },
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
            Self::Removed { name } => format!("removed field `{name}`"),
            Self::BecameRequired { name } => format!("`{name}` is now required"),
            Self::BecameOptional { name } => format!("`{name}` is now optional"),
            Self::TypeChanged { name, .. } => format!("`{name}` changed type"),
        }
    }
}

/// A capability change between two manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityChange {
    pub kind: String,
    pub value: String,
    pub direction: CapabilityDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDirection {
    Added,
    Removed,
}

/// A scalar runtime-limit change between two manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitChange {
    pub name: String,
    pub value: String,
    pub direction: LimitDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitDirection {
    Added,
    Removed,
}

/// An auth-scheme change between two manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<AuthSurface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<AuthSurface>,
}

impl AuthDelta {
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "auth surface changed from `{}` to `{}`",
            self.old.as_ref().map_or("none", AuthSurface::default_key),
            self.new.as_ref().map_or("none", AuthSurface::default_key)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSurface {
    pub default: String,
    pub schemes: Vec<AuthSchemeSurface>,
}

impl AuthSurface {
    fn from_manifest(manifest: &ProviderAuthManifest) -> Self {
        let mut schemes = manifest
            .schemes
            .iter()
            .filter_map(|scheme| {
                let key = scheme.key()?.to_string();
                Some(AuthSchemeSurface {
                    key,
                    scheme: serde_json::to_value(scheme)
                        .expect("auth schemes are serializable provider metadata"),
                })
            })
            .collect::<Vec<_>>();
        schemes.sort_by(|a, b| a.key.cmp(&b.key));
        Self {
            default: manifest.default.clone(),
            schemes,
        }
    }

    #[must_use]
    pub fn default_key(&self) -> &str {
        self.default.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSchemeSurface {
    pub key: String,
    pub scheme: serde_json::Value,
}

// ── Extraction ──────────────────────────────────────────────────────────────

struct Field {
    name: String,
    required: bool,
    value_type: serde_json::Value,
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
            value_type: type_value(&field.value_type),
            default: field.default.clone(),
        })
        .collect()
}

fn type_value(value_type: &ConfigType) -> serde_json::Value {
    serde_json::to_value(value_type).expect("config field types are serializable metadata")
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
    let old_map: HashMap<&str, &Field> = old.iter().map(|f| (f.name.as_str(), f)).collect();
    let new_map: HashMap<&str, &Field> = new.iter().map(|f| (f.name.as_str(), f)).collect();

    let mut changes = Vec::new();
    for (name, old_field) in &old_map {
        match new_map.get(name) {
            Some(new_field) => {
                if old_field.value_type != new_field.value_type {
                    changes.push(FieldChange::TypeChanged {
                        name: (*name).to_string(),
                        old: old_field.value_type.clone(),
                        new: new_field.value_type.clone(),
                    });
                }
                if old_field.required && !new_field.required {
                    changes.push(FieldChange::BecameOptional {
                        name: (*name).to_string(),
                    });
                } else if !old_field.required && new_field.required {
                    changes.push(FieldChange::BecameRequired {
                        name: (*name).to_string(),
                    });
                }
            },
            None => changes.push(FieldChange::Removed {
                name: (*name).to_string(),
            }),
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

    fn manifest_with_auth(auth: &serde_json::Value) -> ProviderManifest {
        let json = serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "provider": "omnifs_provider_demo.wasm",
            "defaultMount": "demo",
            "capabilities": [
                { "kind": "domain", "value": "api.example.com", "why": "api" },
                { "kind": "domain", "value": "uploads.example.com", "why": "uploads" }
            ],
            "auth": auth,
        });
        ProviderManifest::from_bytes(json.to_string().as_bytes()).expect("manifest parses")
    }

    fn oauth_auth(mut scheme: serde_json::Value) -> serde_json::Value {
        if scheme["flow"].is_null() {
            scheme["flow"] = serde_json::json!({
                "pkceLoopback": { "redirectUriTemplate": "http://127.0.0.1:{port}/callback" }
            });
        }
        serde_json::json!({
            "default": "oauth",
            "schemes": [{ "oauth": scheme }],
        })
    }

    fn oauth_scheme() -> serde_json::Value {
        serde_json::json!({
            "key": "oauth",
            "displayName": "OAuth",
            "authorizationEndpoint": "https://auth.example.com/authorize",
            "tokenEndpoint": "https://auth.example.com/token",
            "revocationEndpoint": "https://auth.example.com/revoke",
            "defaultClientId": "client",
            "defaultScopes": ["read"],
            "flow": {
                "pkceLoopback": { "redirectUriTemplate": "http://127.0.0.1:{port}/callback" }
            },
            "injectDomains": ["api.example.com"],
            "injectHeaderName": "Authorization",
            "injectValuePrefix": "Bearer "
        })
    }

    fn assert_auth_requires_consent(old_auth: &serde_json::Value, new_auth: &serde_json::Value) {
        let old = manifest_with_auth(old_auth);
        let new = manifest_with_auth(new_auth);
        match UpgradePlan::diff(&old, &new) {
            UpgradePlan::CapabilityLimitOrAuth {
                capabilities,
                limits,
                auth: Some(_),
            } => {
                assert!(capabilities.is_empty());
                assert!(limits.is_empty());
            },
            other => panic!("expected auth consent delta, got {other:?}"),
        }
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
    fn oauth_endpoint_only_change_requires_consent() {
        let old = oauth_scheme();
        let mut new = old.clone();
        new["authorizationEndpoint"] = serde_json::json!("https://auth.example.com/v2/authorize");

        assert_auth_requires_consent(&oauth_auth(old), &oauth_auth(new));
    }

    #[test]
    fn oauth_scope_change_requires_consent() {
        let old = oauth_scheme();
        let mut new = old.clone();
        new["defaultScopes"] = serde_json::json!(["read", "write"]);

        assert_auth_requires_consent(&oauth_auth(old), &oauth_auth(new));
    }

    #[test]
    fn oauth_inject_domain_change_requires_consent() {
        let old = oauth_scheme();
        let mut new = old.clone();
        new["injectDomains"] = serde_json::json!(["api.example.com", "uploads.example.com"]);

        assert_auth_requires_consent(&oauth_auth(old), &oauth_auth(new));
    }

    #[test]
    fn oauth_inject_header_change_requires_consent() {
        let old = oauth_scheme();
        let mut new = old.clone();
        new["injectHeaderName"] = serde_json::json!("X-Omnifs-Token");

        assert_auth_requires_consent(&oauth_auth(old), &oauth_auth(new));
    }

    #[test]
    fn oauth_flow_kind_change_requires_consent() {
        let old = oauth_scheme();
        let mut new = old.clone();
        new["flow"] = serde_json::json!({
            "deviceCode": { "deviceAuthorizationEndpoint": "https://auth.example.com/device" }
        });

        assert_auth_requires_consent(&oauth_auth(old), &oauth_auth(new));
    }

    #[test]
    fn config_type_change_is_breaking() {
        let old_fields = serde_json::json!([
            { "name": "endpoint", "type": { "kind": "string" }, "required": true }
        ]);
        let new_fields = serde_json::json!([
            { "name": "endpoint", "type": { "kind": "object", "fields": [] }, "required": true }
        ]);
        let old = manifest(&old_fields);
        let new = manifest(&new_fields);

        match UpgradePlan::diff(&old, &new) {
            UpgradePlan::BreakingConfig { changes } => {
                assert!(matches!(
                    changes.as_slice(),
                    [FieldChange::TypeChanged { name, .. }] if name == "endpoint"
                ));
            },
            other => panic!("expected type change to be breaking, got {other:?}"),
        }
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
