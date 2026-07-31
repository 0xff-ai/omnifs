//! Core omnifs protocol types.

pub mod attachment;
mod auth_fingerprint;
mod client_owner_id;
mod content_type;
mod file;
pub mod fs;
mod mount_name;
mod mutation;
mod operation;
pub mod path;
mod provider;
mod provider_id;
pub mod resource;
mod state_version;

pub use attachment::{
    AttachmentSpec, AttachmentSpecError, AttachmentVersion, RuntimeInstanceId,
    RuntimeInstanceIdError,
};
pub use auth_fingerprint::{AuthRuntimeFingerprint, AuthRuntimeFingerprintParseError};
pub use client_owner_id::{ClientOwnerId, ClientOwnerIdError};
pub use content_type::ContentType;
pub use file::{FileSize, ReadMode, Stability};
pub use mount_name::{MountName, MountNameError};
pub use mutation::{MutationId, MutationIdError};
pub use operation::{ActionId, ActionIdError};
pub use path::{ParseError, Path, Segment};
pub use provider::{
    IdError, ProviderMeta, ProviderName, ProviderRef, ProviderVersion, validate_account,
    validate_key_part,
};
pub use provider_id::{ProviderId, ProviderIdHexError};
pub use resource::{
    ResourceDigest, ResourceDigestParseError, ResourceKey, ResourceKind, ResourceName,
    ResourceNameError, ResourceRevision, ResourceRevisionParseError,
};
pub use state_version::{
    CredentialGeneration, CredentialVersion, MountRevision, MountVersion, MountVersionParseError,
};
