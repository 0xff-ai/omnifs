//! Library surface for the omnifs runtime daemon.
//!
//! The `omnifs daemon` subcommand uses these modules. There is no standalone
//! `omnifsd` binary; the daemon entrypoint is [`run`].

mod app;
mod context;
mod server;

pub use app::{DaemonArgs, run};
