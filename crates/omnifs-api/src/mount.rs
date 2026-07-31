use omnifs_core::{MountVersion, ProviderId, ResourceName, ResourceRevision};
use serde::{Deserialize, Serialize};

/// Client-authored fields of one daemon-owned mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountDefinition {
    pub name: ResourceName,
    pub provider: ProviderId,
    pub auth: Option<MountCredential>,
    pub limits: Option<MountLimits>,
    /// Opaque provider config encoded as one strict JSON value.
    pub config: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountCredential {
    pub scheme: String,
    pub account_label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountLimits {
    pub max_memory_mb: Option<u32>,
    pub max_fetch_blob_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountRecord {
    pub definition: MountDefinition,
    pub provider: crate::ProviderReference,
    pub version: MountVersion,
    pub revision: ResourceRevision,
    pub health: MountHealth,
    /// Non-secret credential readiness for this mount, when it has a
    /// credential binding. This is separate from serving/provider health.
    pub auth_health: Option<crate::CredentialHealth>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountHealth {
    Active,
    AuthRequired,
    ProviderUnavailable { reason: String },
    Failed { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_definition_round_trips_through_json_with_empty_fields() {
        let definition = MountDefinition {
            name: ResourceName::new("demo").unwrap(),
            provider: ProviderId::from_wasm_bytes(b"demo"),
            auth: None,
            limits: Some(MountLimits {
                max_memory_mb: None,
                max_fetch_blob_bytes: Some(1024),
            }),
            config: br#"{"enabled":true}"#.to_vec(),
        };
        let encoded = serde_json::to_vec(&definition).unwrap();
        assert_eq!(
            serde_json::from_slice::<MountDefinition>(&encoded).unwrap(),
            definition
        );
    }
}
