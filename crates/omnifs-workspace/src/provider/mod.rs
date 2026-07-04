//! The provider contract: the `omnifs.provider-manifest.v1` and
//! `omnifs.provider-metadata.v1` custom sections, route resolution, the
//! provider capability model, and the config metadata. The auth-scheme wire
//! model lives in [`crate::authn`].
//!
//! The section is a concatenation of length-framed records. Each record is
//! `u32 length_le + u8 tag + u8 reserved + body_bytes`. `length_le` covers
//! the tag, reserved, and body bytes (not itself). `body_bytes` is UTF-8
//! JSON produced by `serde_json`.

pub(crate) mod authoring;
pub(crate) mod catalog;
pub(crate) mod config;
pub(crate) mod manifest;
pub(crate) mod records;
pub(crate) mod resolve;
pub(crate) mod sections;
pub(crate) mod store;
pub(crate) mod wasm;

pub use authoring::ProviderAuthBuilder;
pub use catalog::{Catalog, CatalogError, DirStatus, Provider};
pub use config::{ConfigError, ConfigField, ConfigMetadata, ConfigType, HostResourceBinding};
pub use manifest::{ProviderAuthManifest, ProviderManifest};
pub use records::{
    DecodeError, HandlerKindRecord, HandlerRecord, ManifestCaptureRecord, ManifestRecord,
    ManifestRecordIter, MutationRecord, SubtreeRouteRecord, TAG_HANDLER, TAG_MUTATION,
    TAG_SUBTREE_ROUTE, encode_handler, encode_mutation, encode_subtree_route, frame_record,
};
pub use resolve::{ResolveError, ResolvedManifest, resolve_manifest};
pub use sections::{
    MANIFEST_SECTION_NAME, ManifestSectionError, PROVIDER_METADATA_SECTION_NAME,
    ProviderMetadataError, embed_provider_metadata_section, provider_manifest_json,
    read_manifest_section, read_provider_metadata_section,
};
pub use store::{Index, IndexEntry, ProviderStore, StoreError};
pub use wasm::{Artifact, ArtifactError, ArtifactLoadError, ProviderWasm, ProviderWasmError};
