//! Daemon-owned tracing sink.

use anyhow::Context as _;
use omnifs_bootstrap::{Bootstrap, Daemon};
use omnifs_engine::Inspector;
use std::sync::{Arc, OnceLock};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

/// The profile root this process resolved its log file against. Recorded so
/// `verify_resolved_profile` can catch a later independent resolution
/// (`DaemonContext::resolve`) landing on a different `OMNIFS_HOME` instead of
/// letting the log file and the control socket land in different profiles
/// silently.
static RESOLVED_PROFILE: OnceLock<Bootstrap<Daemon>> = OnceLock::new();

pub fn init(inspector: Option<&Arc<Inspector>>) -> anyhow::Result<()> {
    let endpoint = omnifs_bootstrap::Bootstrap::<Daemon>::for_daemon()?;
    let _ = RESOLVED_PROFILE.set(endpoint.clone());
    let log = omnifs_state::open_daemon_log(&endpoint)?;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("omnifs_inspector=off".parse().expect("static directive"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(log)
        .with_ansi(false)
        .with_target(false)
        .with_filter(filter);
    let inspector_layer = inspector.map(|inspector| {
        inspector.layer().with_filter(filter_fn(|metadata| {
            metadata.target() == "omnifs_inspector"
        }))
    });
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(inspector_layer)
        .try_init()
        .context("initialize daemon tracing")
}

/// Fail loudly if `context` resolved a different profile root than `init`
/// used for the log file. A no-op when `init` was never called in this
/// process (the daemon's own tests call `run` directly, bypassing the CLI
/// entrypoint that calls `init` first).
pub(crate) fn verify_resolved_profile(
    context: &crate::context::DaemonContext,
) -> anyhow::Result<()> {
    if let Some(logged) = RESOLVED_PROFILE.get()
        && logged != context.endpoint()
    {
        anyhow::bail!(
            "daemon log profile `{}` diverges from control profile `{}`; OMNIFS_HOME must not \
             change between logging::init and DaemonContext::resolve",
            logged.bootstrap_dir().display(),
            context.endpoint().bootstrap_dir().display(),
        );
    }
    Ok(())
}
