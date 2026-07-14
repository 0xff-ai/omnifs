//! The provider contract: the `omnifs.provider-metadata.v1` custom section,
//! the provider capability model, and config metadata. The auth-scheme wire
//! model lives in [`crate::authn`].

pub(crate) mod authoring;
pub(crate) mod catalog;
pub(crate) mod config;
pub(crate) mod manifest;
pub(crate) mod sections;
pub(crate) mod store;
pub(crate) mod wasm;

pub use authoring::ProviderAuthBuilder;
pub use catalog::{Catalog, CatalogError, DirStatus, Provider};
pub use config::{ConfigError, ConfigField, ConfigMetadata, ConfigType, HostResourceBinding};
pub use manifest::{ProviderAuthManifest, ProviderManifest};
pub use sections::{
    PROVIDER_METADATA_SECTION_NAME, ProviderMetadataError, embed_provider_metadata_section,
    is_hostname_only, provider_manifest_json, read_provider_metadata_section,
};
pub use store::{Index, IndexEntry, ProviderStore, StoreError};
pub use wasm::{Artifact, ArtifactError, ArtifactLoadError, ProviderWasm};
