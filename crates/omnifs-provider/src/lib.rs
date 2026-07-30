//! The provider contract: the `omnifs.provider-metadata.v1` custom section,
//! the provider capability model, and config metadata. The auth-scheme wire
//! model lives in [`omnifs_auth`].

mod config;
mod manifest;
mod sections;
mod store;
mod wasm;

pub use config::{
    ConfigError, ConfigField, ConfigMetadata, ConfigType, HostResourceBinding, PreopenMode,
    PreopenedPath,
};
pub use manifest::{
    AccessNeed, LimitDeclarations, ProviderAuthManifest, ProviderManifest, ResourceLimit,
};
pub use sections::{
    PROVIDER_METADATA_SECTION_NAME, ProviderMetadataError, is_hostname_only,
    provider_manifest_json, read_provider_metadata_section,
};
pub use store::{Index, IndexEntry, ProviderStore, StoreError};
pub use wasm::{Artifact, ArtifactError, ArtifactLoadError};
