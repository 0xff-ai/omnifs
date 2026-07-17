//! Every byte under `OMNIFS_HOME` has its format owned here: the directory
//! layout, mount specs, the provider index, and credential stores.

#![forbid(unsafe_code)]

pub mod attach;
pub mod authn;
pub mod config;
pub mod creds;
pub mod daemon_record;
pub mod ids;
mod io;
mod layout;
pub mod metrics;
pub mod mounts;
pub mod provider;
pub mod workspace;

pub use layout::{
    OMNIFS_HOME_ENV, OMNIFS_MOUNT_POINT_ENV, ResolveError, display, resolve_mount_point,
    wasm_cache_dir,
};
pub use workspace::{
    DaemonState, FrontendState, WarmupProgress, WarmupStore, Workspace, WorkspaceIdentity,
};
