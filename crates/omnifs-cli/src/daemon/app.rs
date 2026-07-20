//! Daemon entrypoint: argument surface and startup handoff to the serving lifetime.
//!
//! These are invoked by the `omnifs daemon` subcommand (the single-binary
//! entrypoint); there is no standalone `omnifsd` binary. The daemon still
//! runs as its own host-native process and speaks the local typed control protocol.

use anyhow::Context as _;
use clap::Args;
use omnifs_engine::GitCloner;
use omnifs_engine::MountTable;
use omnifs_engine::init_global_from_env;
use omnifs_workspace::mounts::Registry;
use omnifs_workspace::mounts::Revision;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::info;

use super::{context::DaemonContext, server};

/// Arguments for the `omnifs daemon` subcommand (the runtime daemon).
#[derive(Args, Debug)]
pub(crate) struct DaemonArgs {
    /// Revision of the immutable mount snapshot served by this daemon start.
    #[arg(long, value_name = "REVISION")]
    pub(crate) mount_revision: Revision,
    /// Immutable mount snapshot directory to load before readiness.
    #[arg(long, value_name = "PATH")]
    pub(crate) mount_snapshot: PathBuf,
    /// Serve only validated durable projection facts, without provider startup.
    #[arg(long)]
    pub(crate) offline: bool,
    /// Additionally serve the shared namespace over a TCP loopback listener at
    /// `127.0.0.1:<port>` (`0` asks the OS for an ephemeral port). This is the
    /// Docker Desktop path: a containerized frontend cannot share a host Unix
    /// socket into the Linux VM it runs in, so it dials TCP instead. Absent:
    /// no TCP attach listener at start (one can still be bound later on a
    /// running daemon through the local control socket).
    #[arg(long = "attach-tcp", value_name = "PORT")]
    pub(crate) attach_tcp: Option<u16>,
}

/// Bring up immutable runtime state, then hand the complete serving lifetime to
/// [`server::Daemon::run`]. The caller owns the tokio runtime and tracing setup.
pub(crate) async fn run(args: &DaemonArgs) -> anyhow::Result<()> {
    use omnifs_workspace::metrics::{self, DaemonEvent};

    let context = DaemonContext::resolve(args)?;
    context.prepare_startup_dirs(args.offline)?;

    // Local-only dogfood counters. The daemon's off-switch is the
    // `OMNIFS_METRICS` env var (the CLI propagates
    // its `[metrics] enabled = false` into it when launching the daemon).
    let metrics = context.metrics().sink(metrics::enabled_from_env());
    metrics.daemon_event(DaemonEvent::DaemonStart, 0);

    let desired = Registry::load(&args.mount_snapshot).with_context(|| {
        format!(
            "load selected mount revision {} from {}",
            args.mount_revision,
            args.mount_snapshot.display()
        )
    })?;
    let registry = if args.offline {
        Arc::new(MountTable::load_offline(context.host_context(), &desired)?)
    } else {
        let cloner = Arc::new(GitCloner::new(context.clone_cache())?);
        let host_context = context.host_context();
        info!(
            config = %host_context.config_dir().display(),
            cache = %cloner.cache_dir().display(),
            providers = %host_context.providers_dir().display(),
            "starting daemon"
        );
        Arc::new(MountTable::load_online(
            host_context,
            &cloner,
            &desired,
            &Handle::current(),
        )?)
    };
    let rt = Handle::current();
    let inspector = init_global_from_env();
    if let Some(inspector) = &inspector {
        if let Some(path) = inspector.tee_path() {
            info!(path = %path.display(), "inspector stream enabled (in-memory ring + file tee)");
        } else {
            info!("inspector stream enabled (in-memory ring only)");
        }
    }

    let daemon_record = context.daemon_record();
    let daemon_record =
        server::DaemonRecordStore::new(context.daemon_state().clone(), daemon_record);
    let attach_store = Arc::new(
        context
            .attach_store()
            .with_context(|| "open durable frontend attach targets")?,
    );
    let daemon = Arc::new(server::Daemon::new(
        context,
        Arc::clone(&registry),
        inspector,
        Arc::clone(&daemon_record),
        attach_store,
    ));
    // Build the one shared namespace after atomic startup loading, so its root
    // record reflects the complete mount set.
    let namespace = if args.offline {
        omnifs_engine::TreeNamespace::offline(Arc::clone(&registry), rt.clone())
    } else {
        omnifs_engine::TreeNamespace::online(Arc::clone(&registry), rt.clone())
    };
    // Give the daemon's VfsServer a handle to the namespace so typed attach
    // requests can bind a TCP listener on a running daemon without a restart.
    daemon.set_namespace(Arc::clone(&namespace));
    let result = daemon.run().await;
    let served_mounts = registry.mounts().len();
    metrics.daemon_event(DaemonEvent::FrontendStopped, served_mounts);
    metrics.daemon_event(DaemonEvent::DaemonStop, served_mounts);
    result
}
