//! The omnifs mount: the user-authored mount `Spec` (which bakes in its
//! provider-manifest defaults at creation), the `Registry` that owns specs on
//! disk, materialization against the provider manifest in `omnifs-provider`, and
//! provider upgrade classification. Plus the sparse user `Auth` config.

#![forbid(unsafe_code)]

pub mod auth;
pub mod materialize;
pub mod mounts;
pub mod upgrade;

pub use auth::{Auth, AuthKind, OAuth, StaticToken};
pub use upgrade::{
    AddedField, AuthDelta, CapabilityChange, CapabilityDirection, FieldChange, UpgradePlan,
};
