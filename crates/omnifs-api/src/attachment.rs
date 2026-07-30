use omnifs_core::{AttachmentSpec, ResourceKey, ResourceKind, ResourceName};
use serde::{Deserialize, Serialize};

/// Desired exposure of the complete shared namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentDefinition {
    pub name: ResourceName,
    pub spec: AttachmentSpec,
}

impl AttachmentDefinition {
    #[must_use]
    pub const fn name(&self) -> &ResourceName {
        &self.name
    }

    #[must_use]
    pub fn key(&self) -> ResourceKey {
        ResourceKey::new(ResourceKind::Attachment, self.name.clone())
    }
}
