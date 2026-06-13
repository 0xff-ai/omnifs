//! Library surface for `omnifsd` binaries.
//!
//! The runtime entrypoint and the `OpenAPI` generator both use these modules so
//! the control API document is produced from the same handler implementation
//! that serves the daemon.

pub mod frontends;
pub mod mounts;
#[cfg(target_os = "linux")]
mod proc_mounts;
pub mod server;
