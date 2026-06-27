//! The omnifs mount: the user-authored mount `Spec` (which bakes in its
//! provider-manifest defaults at creation), the `Registry` that owns specs on
//! disk, materialization against the provider manifest in `omnifs-provider`, and
//! provider upgrade classification. Plus the sparse user `Auth` config.

#![forbid(unsafe_code)]

pub mod materialize;
mod mount_config;
pub mod mounts;
pub mod upgrade;

pub use mount_config::{
    Auth, AuthKind, OAuth, ProviderConfig, StaticToken, deserialize_auth as deserialize_mount_auth,
    serialize_auth as serialize_mount_auth,
};
pub use upgrade::{
    AddedField, AuthDelta, CapabilityChange, CapabilityDirection, FieldChange, UpgradePlan,
};
