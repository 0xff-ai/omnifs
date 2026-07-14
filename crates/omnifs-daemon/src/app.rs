//! Daemon entrypoint: argument surface and startup handoff to the serving lifetime.
//!
//! These are invoked by the `omnifs daemon` subcommand (the single-binary
//! entrypoint); there is no standalone `omnifsd` binary. The daemon still
//! runs as its own host-native process and speaks the HTTP control API.

use anyhow::Context as _;
use clap::Args;
use omnifs_engine::GitCloner;
use omnifs_engine::MountRuntimes;
use omnifs_engine::init_global_from_env;
use omnifs_workspace::mounts::Registry;
use omnifs_workspace::mounts::Revision;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::info;

use crate::{context::DaemonContext, server};

/// Arguments for the `omnifs daemon` subcommand (the runtime daemon).
#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// Revision of the immutable mount snapshot served by this daemon start.
    #[arg(long, value_name = "REVISION")]
    pub(crate) mount_revision: Revision,
    /// Immutable mount snapshot directory to load before readiness.
    #[arg(long, value_name = "PATH")]
    pub(crate) mount_snapshot: PathBuf,
    /// Optional TCP control API listen address. The daemon always serves its
    /// Unix socket and adds TCP only for this debug/test path.
    #[arg(long)]
    pub(crate) listen: Option<SocketAddr>,
    /// Additionally serve the shared namespace over a TCP loopback listener at
    /// `127.0.0.1:<port>` (`0` asks the OS for an ephemeral port), guarded by a
    /// per-instance attach token instead of filesystem permissions. This is the
    /// Docker Desktop path: a containerized frontend cannot share a host Unix
    /// socket into the Linux VM it runs in, so it dials TCP instead. Absent:
    /// no TCP attach listener at start (one can still be bound later on a
    /// running daemon via `POST /v1/frontend/attach-target`).
    #[arg(long = "attach-tcp", value_name = "PORT")]
    pub(crate) attach_tcp: Option<u16>,
}

/// Bring up immutable runtime state, then hand the complete serving lifetime to
/// [`server::Daemon::run`]. The caller owns the tokio runtime and tracing setup.
pub async fn run(args: &DaemonArgs) -> anyhow::Result<()> {
    use omnifs_workspace::telemetry::{self, DaemonEvent, TelemetrySink};

    let context = DaemonContext::resolve(args)?;
    context.prepare_startup_dirs()?;

    // Local-only dogfood counters. The daemon's off-switch is the
    // `OMNIFS_TELEMETRY` env var (the CLI propagates
    // its `[telemetry] enabled = false` into it when launching the daemon).
    let telemetry_backend = telemetry::Backend::Native;
    let telemetry = TelemetrySink::new(context.config_dir(), telemetry::enabled_from_env());
    telemetry.daemon_event(DaemonEvent::DaemonStart, telemetry_backend, 0);

    let cloner = Arc::new(GitCloner::new(context.cache_dir().to_path_buf()));

    let registry = {
        let host_context = context.host_context();
        let desired = Registry::load(&args.mount_snapshot).with_context(|| {
            format!(
                "load selected mount revision {} from {}",
                args.mount_revision,
                args.mount_snapshot.display()
            )
        })?;
        info!(
            config = %host_context.config_dir().display(),
            cache = %cloner.cache_dir().display(),
            providers = %host_context.providers_dir().display(),
            "starting daemon"
        );
        Arc::new(MountRuntimes::load(
            host_context,
            Arc::clone(&cloner),
            &desired,
            &Handle::current(),
        )?)
    };

    // Proactively refreshes every registered OAuth credential before it enters
    // its refresh window, so a request-path authorization call almost never
    // has to await a live refresh. Spawned on the shared credential service
    // (not per-mount) so all startup credentials share one refresh owner.
    let refresh_loop = registry.credential_service().spawn_refresh_loop();

    let rt = Handle::current();
    let inspector = init_global_from_env();
    if let Some(inspector) = &inspector {
        if let Some(path) = inspector.tee_path() {
            info!(path = %path.display(), "inspector stream enabled (in-memory ring + file tee)");
        } else {
            info!("inspector stream enabled (in-memory ring only)");
        }
    }

    // Read the predecessor before publishing anything for this invocation. It
    // is restoration input, not an ownership claim: startup failure leaves it
    // in place as stale diagnostic evidence.
    let record_path = context.runtime_record_file();
    let previous = omnifs_workspace::runtime_record::RuntimeRecord::read(&record_path)
        .with_context(|| format!("read predecessor runtime record {}", record_path.display()))?;
    let runtime_record = context.runtime_record();
    let runtime_record = server::RuntimeRecordStore::new(record_path, runtime_record);
    let control_token = server::ControlToken::resolve()?;
    let daemon = Arc::new(server::Daemon::new(
        context,
        Arc::clone(&registry),
        inspector,
        Arc::clone(&runtime_record),
        control_token,
    ));
    // Build the one shared namespace after atomic startup loading, so its root
    // record reflects the complete mount set.
    let namespace = omnifs_engine::TreeNamespace::new(Arc::clone(&registry), rt.clone());
    // Give the daemon a handle to the namespace so `POST /v1/frontend/attach-target`
    // can bind a TCP attach listener on a running daemon without a restart.
    daemon.set_namespace(Arc::clone(&namespace));
    let result = daemon.run(previous, refresh_loop).await;
    let served_mounts = registry.runtime_entries().len();
    telemetry.daemon_event(
        DaemonEvent::FrontendStopped,
        telemetry_backend,
        served_mounts,
    );
    telemetry.daemon_event(DaemonEvent::DaemonStop, telemetry_backend, served_mounts);
    result
}
