//! Provider contract snapshot: the surface a mount spec was built against.
//!
//! A `Contract` is derived from the provider manifest at `omnifs init` time and
//! stamped into the mount spec. On upgrade, `omnifs up` diffs the stamped
//! block against the live manifest contract to classify the change and route
//! appropriately: identical (nothing), additive config (auto-migrate),
//! breaking config (prompt), capability or auth delta (re-consent), or provider
//! removed (hard error).
//!
//! The contract hash is derived on demand from the `Contract` block, not stored,
//! because the block must survive the upgrade (once `install_embedded_bundle`
//! overwrites the provider WASM and manifest, the spec is the only place the
//! previous contract remains, making a structural diff possible).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use omnifs_provider::{CapabilityEntry, ConfigSchema, ProviderManifest};

/// Snapshot of the provider surface a mount spec was built against.
///
/// Stored in the mount spec so the pre-upgrade structural diff has the old
/// contract available even after the provider binary and manifest are replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Contract {
    /// Config fields declared by the provider at init time, with their
    /// required flag (true when the field has no default value).
    #[serde(default)]
    pub config_fields: Vec<ContractField>,
    /// Capability entries declared by the provider at init time.
    #[serde(default)]
    pub capabilities: Vec<ContractCapability>,
    /// Auth scheme id used at init time, if the provider declares auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scheme: Option<String>,
    /// Cargo version of the provider at init time. Provenance label only;
    /// bumps with every workspace release regardless of contract changes, so it
    /// cannot be used to determine whether the contract changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_version: Option<String>,
}

/// A single config field entry in the contract snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ContractField {
    pub name: String,
    /// True when the field has no default value and the user must supply it.
    pub required: bool,
}

/// A single capability entry in the contract snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ContractCapability {
    pub kind: String,
    pub value: String,
}

impl Contract {
    /// Derive a contract snapshot from a provider manifest.
    ///
    /// Config fields come from `manifest.config_schema.properties`; a field
    /// is required when it has no `default` value. Capabilities come from
    /// `manifest.capabilities`. Auth scheme is the manifest's `auth.default`.
    #[must_use]
    pub fn from_manifest(manifest: &ProviderManifest) -> Self {
        let config_fields = extract_config_fields(manifest);
        let capabilities = extract_capabilities(manifest);
        let auth_scheme = manifest.auth.as_ref().map(|auth| auth.default.clone());
        Self {
            config_fields,
            capabilities,
            auth_scheme,
            provider_version: None,
        }
    }

    /// Compute a SHA-256 hash over the contract surface (config fields,
    /// capabilities, auth scheme). The hash is a hex-encoded string of the
    /// first 16 bytes of SHA-256, giving 32 hex characters. The
    /// `provider_version` field is excluded because it is a provenance label
    /// only and does not describe the contract surface.
    #[must_use]
    pub fn hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();

        // Hash config fields in sorted order so field order in the manifest
        // does not affect the hash.
        let mut fields = self.config_fields.clone();
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        for field in &fields {
            hasher.update(field.name.as_bytes());
            hasher.update([u8::from(field.required)]);
        }

        // Capabilities sorted for stability.
        let mut caps = self.capabilities.clone();
        caps.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.value.cmp(&b.value)));
        for cap in &caps {
            hasher.update(cap.kind.as_bytes());
            hasher.update(b"\x00");
            hasher.update(cap.value.as_bytes());
            hasher.update(b"\x01");
        }

        // Auth scheme.
        if let Some(scheme) = &self.auth_scheme {
            hasher.update(scheme.as_bytes());
        }

        let digest = hasher.finalize();
        hex::encode(&digest[..16])
    }

    /// Classify the difference between `self` (the stamped contract in the spec)
    /// and `live` (the contract derived from the current provider manifest).
    ///
    /// Returns the most severe change class: if any capability or auth changes
    /// are present they dominate, then breaking config, then additive config.
    #[must_use]
    pub fn classify_against(&self, live: &Contract) -> ContractDelta {
        // Fast path: identical contracts.
        if self == live {
            return ContractDelta::Identical;
        }

        // Check capabilities and auth first: they require re-consent.
        let caps_changed = normalize_caps(&self.capabilities) != normalize_caps(&live.capabilities);
        let auth_changed = self.auth_scheme != live.auth_scheme;
        if caps_changed || auth_changed {
            let mut capability_delta = Vec::new();
            let mut auth_delta = None;
            if caps_changed {
                capability_delta = diff_capabilities(&self.capabilities, &live.capabilities);
            }
            if auth_changed {
                auth_delta = Some(AuthDelta {
                    old: self.auth_scheme.clone(),
                    new: live.auth_scheme.clone(),
                });
            }
            return ContractDelta::CapabilityOrAuth {
                capability_delta,
                auth_delta,
            };
        }

        // Config fields: classify as additive-only or breaking.
        let field_changes = diff_fields(&self.config_fields, &live.config_fields);
        if field_changes.is_empty() {
            // Only provider_version changed; treat as identical.
            return ContractDelta::Identical;
        }

        let has_breaking = field_changes.iter().any(|c| {
            matches!(
                c,
                FieldChange::Removed(_)
                    | FieldChange::BecameRequired(_)
                    | FieldChange::Renamed { .. }
            )
        });
        let all_additive = field_changes.iter().all(|c| {
            matches!(
                c,
                FieldChange::Added {
                    required: false,
                    ..
                }
            )
        });

        if all_additive {
            let added: Vec<_> = field_changes
                .into_iter()
                .filter_map(|c| {
                    if let FieldChange::Added { name, default, .. } = c {
                        Some(AddedField { name, default })
                    } else {
                        None
                    }
                })
                .collect();
            ContractDelta::AdditiveConfig { added }
        } else if has_breaking {
            ContractDelta::BreakingConfig {
                changes: field_changes,
            }
        } else {
            // Has-required fields added; that is breaking.
            ContractDelta::BreakingConfig {
                changes: field_changes,
            }
        }
    }
}

/// The classification of a contract diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractDelta {
    /// No contract change; nothing to do.
    Identical,
    /// New optional config fields with defaults; auto-migrate by filling
    /// defaults and re-stamping.
    AdditiveConfig { added: Vec<AddedField> },
    /// Config changed in a breaking way (removed field, new required field,
    /// rename); prompt the user for updated values.
    BreakingConfig { changes: Vec<FieldChange> },
    /// A capability or auth scheme changed; requires explicit re-consent.
    CapabilityOrAuth {
        capability_delta: Vec<CapabilityChange>,
        auth_delta: Option<AuthDelta>,
    },
}

/// A new optional config field added in the live contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedField {
    pub name: String,
    /// Default value from the live manifest's config schema.
    pub default: Option<serde_json::Value>,
}

/// A single config-field change between two contracts.
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
    Renamed {
        old: String,
        new: String,
    },
}

impl FieldChange {
    /// Human-readable description for display in prompts.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Added {
                name,
                required: true,
                ..
            } => {
                format!("new required field `{name}`")
            },
            Self::Added {
                name,
                required: false,
                ..
            } => {
                format!("new optional field `{name}`")
            },
            Self::Removed(name) => format!("removed field `{name}`"),
            Self::BecameRequired(name) => format!("`{name}` is now required"),
            Self::BecameOptional(name) => format!("`{name}` is now optional"),
            Self::Renamed { old, new } => format!("field `{old}` renamed to `{new}`"),
        }
    }
}

/// A capability change between two contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityChange {
    pub kind: ContractCapability,
    pub direction: CapabilityDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityDirection {
    Added,
    Removed,
}

/// An auth scheme change between two contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthDelta {
    pub old: Option<String>,
    pub new: Option<String>,
}

// ---------------------------------------------------------------------------
// Extraction helpers
// ---------------------------------------------------------------------------

fn extract_config_fields(manifest: &ProviderManifest) -> Vec<ContractField> {
    let Some(schema) = manifest.config_schema.as_ref() else {
        return Vec::new();
    };
    let Ok(parsed) = ConfigSchema::parse(schema) else {
        return Vec::new();
    };
    parsed
        .properties
        .iter()
        .map(|(name, prop)| ContractField {
            name: name.clone(),
            required: prop.default.is_none(),
        })
        .collect()
}

/// Map of config-field name to its default value, from the manifest's config
/// schema. Only fields that declare a default appear. The additive-migration
/// pre-flight uses this to fill new optional fields, which the contract block
/// itself cannot carry (it records required-ness, not values).
#[must_use]
pub(crate) fn config_field_defaults(
    manifest: &ProviderManifest,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut defaults = std::collections::HashMap::new();
    let Some(schema) = manifest.config_schema.as_ref() else {
        return defaults;
    };
    let Ok(parsed) = ConfigSchema::parse(schema) else {
        return defaults;
    };
    for (name, prop) in &parsed.properties {
        if let Some(default) = &prop.default {
            defaults.insert(name.clone(), default.clone());
        }
    }
    defaults
}

fn extract_capabilities(manifest: &ProviderManifest) -> Vec<ContractCapability> {
    manifest
        .capabilities
        .iter()
        .map(|entry| ContractCapability {
            kind: capability_kind(entry),
            value: capability_value_str(entry),
        })
        .collect()
}

fn capability_kind(entry: &CapabilityEntry) -> String {
    match entry {
        CapabilityEntry::Domain { .. } => "domain".to_string(),
        CapabilityEntry::GitRepo { .. } => "gitRepo".to_string(),
        CapabilityEntry::UnixSocket { .. } => "unixSocket".to_string(),
        CapabilityEntry::PreopenedPath { .. } => "preopenedPath".to_string(),
        CapabilityEntry::MemoryMb { .. } => "memoryMb".to_string(),
        CapabilityEntry::FetchBlobBytes { .. } => "fetchBlobBytes".to_string(),
        CapabilityEntry::ReadBlobBytes { .. } => "readBlobBytes".to_string(),
    }
}

fn capability_value_str(entry: &CapabilityEntry) -> String {
    match entry {
        CapabilityEntry::Domain { value, .. }
        | CapabilityEntry::GitRepo { value, .. }
        | CapabilityEntry::UnixSocket { value, .. } => value.clone(),
        CapabilityEntry::PreopenedPath { value, .. } => {
            serde_json::to_string(value).unwrap_or_default()
        },
        CapabilityEntry::MemoryMb { value, .. } => value.to_string(),
        CapabilityEntry::FetchBlobBytes { value, .. }
        | CapabilityEntry::ReadBlobBytes { value, .. } => value.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Diff helpers
// ---------------------------------------------------------------------------

fn normalize_caps(caps: &[ContractCapability]) -> Vec<(String, String)> {
    let mut sorted: Vec<_> = caps
        .iter()
        .map(|c| (c.kind.clone(), c.value.clone()))
        .collect();
    sorted.sort();
    sorted
}

fn diff_capabilities(
    old: &[ContractCapability],
    new: &[ContractCapability],
) -> Vec<CapabilityChange> {
    use std::collections::HashSet;
    let old_set: HashSet<_> = old
        .iter()
        .map(|c| (c.kind.clone(), c.value.clone()))
        .collect();
    let new_set: HashSet<_> = new
        .iter()
        .map(|c| (c.kind.clone(), c.value.clone()))
        .collect();

    let mut changes = Vec::new();
    for (kind, value) in &old_set {
        if !new_set.contains(&(kind.clone(), value.clone())) {
            changes.push(CapabilityChange {
                kind: ContractCapability {
                    kind: kind.clone(),
                    value: value.clone(),
                },
                direction: CapabilityDirection::Removed,
            });
        }
    }
    for (kind, value) in &new_set {
        if !old_set.contains(&(kind.clone(), value.clone())) {
            changes.push(CapabilityChange {
                kind: ContractCapability {
                    kind: kind.clone(),
                    value: value.clone(),
                },
                direction: CapabilityDirection::Added,
            });
        }
    }
    changes.sort_by(|a, b| {
        a.kind
            .kind
            .cmp(&b.kind.kind)
            .then(a.kind.value.cmp(&b.kind.value))
    });
    changes
}

fn diff_fields(old: &[ContractField], new: &[ContractField]) -> Vec<FieldChange> {
    use std::collections::HashMap;
    let old_map: HashMap<&str, bool> = old.iter().map(|f| (f.name.as_str(), f.required)).collect();
    let new_map: HashMap<&str, bool> = new.iter().map(|f| (f.name.as_str(), f.required)).collect();

    let mut changes = Vec::new();

    for (name, required) in &old_map {
        if let Some(&new_required) = new_map.get(name) {
            if *required && !new_required {
                changes.push(FieldChange::BecameOptional((*name).to_string()));
            } else if !required && new_required {
                changes.push(FieldChange::BecameRequired((*name).to_string()));
            }
            // Same required flag: no change to this field.
        } else {
            changes.push(FieldChange::Removed((*name).to_string()));
        }
    }

    for field in new {
        if !old_map.contains_key(field.name.as_str()) {
            // Retrieve the default from the old_map context is not possible here;
            // the caller must supply it. We store None for now; the pre-flight
            // enriches it from the live manifest.
            changes.push(FieldChange::Added {
                name: field.name.clone(),
                required: field.required,
                default: None,
            });
        }
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, required: bool) -> ContractField {
        ContractField {
            name: name.to_string(),
            required,
        }
    }

    fn cap(kind: &str, value: &str) -> ContractCapability {
        ContractCapability {
            kind: kind.to_string(),
            value: value.to_string(),
        }
    }

    fn contract_with(
        fields: Vec<ContractField>,
        caps: Vec<ContractCapability>,
        auth: Option<&str>,
    ) -> Contract {
        Contract {
            config_fields: fields,
            capabilities: caps,
            auth_scheme: auth.map(str::to_string),
            provider_version: None,
        }
    }

    #[test]
    fn identical_contracts_classify_as_identical() {
        let a = contract_with(
            vec![field("endpoint", false)],
            vec![cap("domain", "api.example.com")],
            Some("oauth"),
        );
        let b = a.clone();
        assert_eq!(a.classify_against(&b), ContractDelta::Identical);
    }

    #[test]
    fn additive_optional_field_auto_migrates() {
        let old = contract_with(vec![field("endpoint", false)], vec![], None);
        let new = contract_with(
            vec![field("endpoint", false), field("timeout_secs", false)],
            vec![],
            None,
        );
        let delta = old.classify_against(&new);
        assert!(
            matches!(delta, ContractDelta::AdditiveConfig { .. }),
            "expected AdditiveConfig, got {delta:?}"
        );
    }

    #[test]
    fn new_required_field_is_breaking() {
        let old = contract_with(vec![field("endpoint", false)], vec![], None);
        let new = contract_with(
            vec![field("endpoint", false), field("api_key", true)],
            vec![],
            None,
        );
        let delta = old.classify_against(&new);
        assert!(
            matches!(delta, ContractDelta::BreakingConfig { .. }),
            "expected BreakingConfig, got {delta:?}"
        );
    }

    #[test]
    fn removed_field_is_breaking() {
        let old = contract_with(
            vec![field("endpoint", false), field("timeout_secs", false)],
            vec![],
            None,
        );
        let new = contract_with(vec![field("endpoint", false)], vec![], None);
        let delta = old.classify_against(&new);
        assert!(
            matches!(delta, ContractDelta::BreakingConfig { .. }),
            "expected BreakingConfig, got {delta:?}"
        );
    }

    #[test]
    fn capability_change_requires_reconsent() {
        let old = contract_with(vec![], vec![cap("domain", "api.old.com")], None);
        let new = contract_with(vec![], vec![cap("domain", "api.new.com")], None);
        let delta = old.classify_against(&new);
        assert!(
            matches!(delta, ContractDelta::CapabilityOrAuth { .. }),
            "expected CapabilityOrAuth, got {delta:?}"
        );
    }

    #[test]
    fn auth_scheme_change_requires_reconsent() {
        let old = contract_with(vec![], vec![], Some("oauth"));
        let new = contract_with(vec![], vec![], Some("pat"));
        let delta = old.classify_against(&new);
        assert!(
            matches!(delta, ContractDelta::CapabilityOrAuth { .. }),
            "expected CapabilityOrAuth, got {delta:?}"
        );
    }

    #[test]
    fn hash_is_stable_across_field_order() {
        let a = Contract {
            config_fields: vec![field("a", false), field("b", true)],
            capabilities: vec![cap("domain", "x")],
            auth_scheme: None,
            provider_version: None,
        };
        let b = Contract {
            config_fields: vec![field("b", true), field("a", false)],
            capabilities: vec![cap("domain", "x")],
            auth_scheme: None,
            provider_version: None,
        };
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn hash_differs_for_different_contracts() {
        let a = contract_with(vec![field("endpoint", false)], vec![], None);
        let b = contract_with(vec![field("endpoint", true)], vec![], None);
        assert_ne!(a.hash(), b.hash());
    }
}
