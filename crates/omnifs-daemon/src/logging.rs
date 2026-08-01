//! Daemon-owned tracing sink.

use anyhow::Context as _;
use omnifs_engine::Inspector;
use omnifs_state::DaemonStatePaths;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

pub fn init(paths: &DaemonStatePaths, inspector: Option<&Arc<Inspector>>) -> anyhow::Result<()> {
    let log = omnifs_state::open_daemon_log(paths)?;
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
