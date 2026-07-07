//! Library surface for the omnifs runtime daemon.
//!
//! The `omnifs daemon` subcommand and the `OpenAPI` generator both use these
//! modules so the control API document is produced from the same handler
//! implementation that serves the daemon. There is no standalone `omnifsd`
//! binary; the daemon entrypoint is [`run`].

mod app;
mod context;
mod frontend;
mod frontends;
mod server;

pub use app::{DaemonArgs, FrontendKind, run};
pub use frontend::{FrontendArgs, run_frontend};
pub use server::openapi_json;
