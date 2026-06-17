//! Library surface for the omnifs runtime daemon.
//!
//! The `omnifs daemon` subcommand and the `OpenAPI` generator both use these
//! modules so the control API document is produced from the same handler
//! implementation that serves the daemon. There is no standalone `omnifsd`
//! binary; the daemon entrypoint is [`run`].

pub mod app;
pub mod frontends;
pub mod mounts;
mod proc_mounts;
pub mod server;

pub use app::{DaemonArgs, FrontendKind, run};
