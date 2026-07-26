//! Every byte under `OMNIFS_HOME` has its format owned here: the directory
//! layout, mount specs, the provider index, and credential stores.

#![forbid(unsafe_code)]

pub mod authn;
pub mod config;
pub mod creds;
pub mod daemon_record;
pub mod filesystems;
pub mod ids;
mod io;
pub mod metrics;
pub mod mounts;
pub mod provider;
pub mod workspace;

pub use workspace::{
    DaemonState, FilesystemState, OMNIFS_HOME_ENV, ResolveError, WarmupProgress, WarmupStore,
    Workspace, WorkspaceIdentity, display, wasm_cache_dir,
};
