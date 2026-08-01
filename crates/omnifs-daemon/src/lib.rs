//! Host-native Omnifs daemon runtime.
//!
//! The `omnifs` binary starts this runtime through its hidden `daemon`
//! subcommand. This crate owns daemon startup, serving, and control handling.

mod app;
mod auth_fingerprint;
mod context;
mod control;
mod credential_codec;
mod credential_document;
mod daemon;
mod doctor;
mod filesystem_supervisor;
mod fs_runtime;
mod generation_builder;
mod log_stream;
mod logging;
mod progress;
mod provider_bundle;
mod provider_preparer;
mod resource_control;
mod serving_reconciler;

pub use app::run;

/// Return the first failure and log the rest.
///
/// Shutdown runs every step even after one fails, so several results can be
/// `Err` at once. `.and()` would keep only the first and silently drop the
/// others; a post-mortem needs the whole sequence, not its first casualty.
pub(crate) fn first_error(
    results: impl IntoIterator<Item = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let mut first = Ok(());
    for result in results {
        if let Err(error) = result {
            if first.is_ok() {
                first = Err(error);
            } else {
                tracing::warn!(%error, "shutdown step failed after an earlier failure");
            }
        }
    }
    first
}

#[cfg(test)]
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
