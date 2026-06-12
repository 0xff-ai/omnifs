//! The omnifs mount: the user-authored mount `Spec`, the runtime-ready
//! `Resolved` mount, the mount `Catalog`, and `Spec -> Resolved` resolution
//! against the provider contract in `omnifs-provider`. Plus the sparse user
//! `Auth` config.

#![forbid(unsafe_code)]

mod mount_config;
pub mod mounts;

pub use mount_config::{
    Auth, AuthKind, OAuth, ProviderConfig, StaticToken, deserialize_auth as deserialize_mount_auth,
};
