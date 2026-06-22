//! The omnifs mount: the user-authored mount `Spec`, the runtime-ready
//! `Resolved` mount, the mount `Catalog`, `Spec -> Resolved` resolution against
//! the provider manifest in `omnifs-provider`, and provider upgrade
//! classification. Plus the sparse user `Auth` config.

#![forbid(unsafe_code)]

pub mod materialize;
mod mount_config;
pub mod mounts;
pub mod upgrade;

pub use mount_config::{
    Auth, AuthKind, OAuth, ProviderConfig, StaticToken, deserialize_auth as deserialize_mount_auth,
};
pub use upgrade::{
    AddedField, AuthDelta, CapabilityChange, CapabilityDirection, FieldChange, UpgradePlan,
};
