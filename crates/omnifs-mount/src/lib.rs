//! The omnifs mount: the user-authored mount `Spec`, the runtime-ready
//! `Resolved` mount, the mount `Catalog`, and `Spec -> Resolved` resolution
//! against the provider contract in `omnifs-provider`. Plus the sparse user
//! `Auth` config.

#![forbid(unsafe_code)]

pub mod contract;
pub mod materialize;
mod mount_config;
pub mod mounts;

pub use contract::{
    AddedField, AuthDelta, CapabilityChange, CapabilityDirection, Contract, ContractCapability,
    ContractDelta, ContractField, FieldChange,
};
pub use mount_config::{
    Auth, AuthKind, OAuth, ProviderConfig, StaticToken, deserialize_auth as deserialize_mount_auth,
};
