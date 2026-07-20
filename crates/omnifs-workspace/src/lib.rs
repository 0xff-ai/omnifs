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
pub mod metrics;
pub mod mounts;
pub mod provider;
pub mod workspace;

pub use workspace::{
    DaemonState, FrontendState, OMNIFS_HOME_ENV, OMNIFS_MOUNT_POINT_ENV, ResolveError,
    WarmupProgress, WarmupStore, Workspace, WorkspaceIdentity, display, resolve_mount_point,
    wasm_cache_dir,
};
