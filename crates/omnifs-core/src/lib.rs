//! Core omnifs protocol types.

mod auth_fingerprint;
mod content_type;
mod file;
pub mod filesystem;
mod mutation;
mod operation;
pub mod path;
mod provider;
mod provider_id;
pub mod resource;
mod state_version;

pub use auth_fingerprint::{AuthRuntimeFingerprint, AuthRuntimeFingerprintParseError};
pub use content_type::ContentType;
pub use file::{FileSize, ReadMode, Stability};
pub use filesystem::{
    FILESYSTEM_GUEST_LOCATION, FilesystemProtocol, FilesystemRuntime, FilesystemSpec,
    FilesystemSpecError, FilesystemVersion, ParseFilesystemProtocolError,
    ParseFilesystemRuntimeError, RuntimeInstanceId, RuntimeInstanceIdError,
    filesystem_pair_supported_on_current_host,
};
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
    CredentialGeneration, CredentialVersion, MountVersion, MountVersionParseError,
};
