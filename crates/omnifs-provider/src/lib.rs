//! The provider contract: the `omnifs.provider-manifest.v1` and
//! `omnifs.provider-metadata.v1` custom sections, route resolution, the
//! provider capability model, the config schema, and the single
//! auth-scheme model.
//!
//! The section is a concatenation of length-framed records. Each record is
//! `u32 length_le + u8 tag + u8 reserved + body_bytes`. `length_le` covers
//! the tag, reserved, and body bytes (not itself). `body_bytes` is UTF-8
//! JSON produced by `serde_json`.

#![forbid(unsafe_code)]

mod auth_resolve;
mod auth_wire;
mod config;
mod manifest;
mod records;
mod resolve;
mod runtime_grants;
mod sections;
mod validation;
mod wasm;

pub use auth_resolve::SchemeResolveError;
pub use auth_wire::{
    AuthManifest, AuthScheme, DeviceCodeConfig, KeyValue, OAuthFlow, OauthScheme,
    PkceLoopbackConfig, PkceManualCodeConfig, StaticTokenScheme, TokenEndpointAuthMethod,
    TokenValidation,
};
pub use config::{
    ConfigProperty, ConfigSchema, ConfigSchemaType, InitHint, InitInput, PreopenStrategy,
};
pub use manifest::{AuthInject, CapabilityEntry, ProviderAuthManifest, ProviderManifest};
pub use records::{
    DecodeError, HandlerKindRecord, HandlerRecord, ManifestCaptureRecord, ManifestRecord,
    ManifestRecordIter, MutationRecord, SubtreeRouteRecord, TAG_HANDLER, TAG_MUTATION,
    TAG_SUBTREE_ROUTE, encode_handler, encode_mutation, encode_subtree_route, frame_record,
};
pub use resolve::{ResolveError, ResolvedManifest, resolve_manifest};
pub use runtime_grants::{
    PreopenMode, PreopenedPath, ProviderCapabilities, UnixSocketEndpointError,
};
pub use sections::{
    MANIFEST_SECTION_NAME, ManifestSectionError, PROVIDER_METADATA_SECTION_NAME,
    ProviderMetadataError, provider_manifest_json, read_manifest_section,
    read_provider_metadata_section,
};
pub use validation::{SchemaError, validate_config};
pub use wasm::{ProviderWasm, ProviderWasmError};
