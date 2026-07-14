//! Daemon entrypoint: argument surface and the blocking run loop.
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
use tracing::{info, warn};

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

/// Bring up the registry, control API, and namespace listeners, then serve
/// until shutdown. Blocks; expects to run on a tokio runtime (the caller
/// owns runtime and tracing setup).
pub fn run(args: &DaemonArgs) -> anyhow::Result<()> {
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
    let sink = init_global_from_env();
    if let Some(sink) = &sink {
        if let Some(path) = sink.tee_path() {
            info!(path = %path.display(), "inspector stream enabled (in-memory ring + file tee)");
        } else {
            info!("inspector stream enabled (in-memory ring only)");
        }
    }

    // The Unix socket is always present; `--listen` adds the debug/test TCP
    // listener. Bind before moving the context into the daemon.
    let record_path = context.runtime_record_file();
    let control_socket_path = context.control_socket();
    let vsock_attach_socket_path = context.vsock_attach_socket();
    let runtime_record = context.runtime_record();
    let runtime_record = server::RuntimeRecordStore::new(record_path, runtime_record);
    let uds_listener = context.bind_control_socket()?;

    // Bind the namespace attach sockets (fail fast on a collision), capturing the
    // per-start id and socket paths before the context moves into the daemon. The
    // listeners are spawned after atomic mount loading so a client sees a populated tree.
    let local_attach_socket = context.bind_local_attach_socket()?;
    let attach_instance_id = context.instance_id().to_string();
    let attach_tcp_port = context.attach_tcp_port();
    let tcp_listener = context.bind_control_listener()?;

    // The bearer token guards the optional TCP listener only. The Unix socket
    // relies on filesystem permissions and never checks it.
    let control_token = server::ControlToken::resolve()?;
    let daemon = Arc::new(server::Daemon::new(
        context,
        Arc::clone(&registry),
        sink,
        Arc::clone(&runtime_record),
        control_token,
    ));
    daemon.spawn_control_unix(uds_listener, &rt)?;
    if let Some(listener) = tcp_listener {
        daemon.spawn_control_tcp(listener, &rt)?;
    }

    runtime_record.write();

    // Build the one shared namespace after atomic startup loading, so its root
    // record reflects the complete mount set.
    let namespace = omnifs_engine::TreeNamespace::new(Arc::clone(&registry), rt.clone());
    // Give the daemon a handle to the namespace so `POST /v1/frontend/attach-target`
    // can bind a TCP attach listener on a running daemon without a restart.
    daemon.set_namespace(Arc::clone(&namespace));

    // Serve the fixed local attach socket over the shared namespace. With every
    // startup listener bound and mounts loaded, report ready.
    let attach_socket_path = spawn_attach_listener(
        local_attach_socket,
        &namespace,
        &attach_instance_id,
        &rt,
        &daemon.attach_observer(omnifs_api::FrontendDelivery::Local),
    )?;
    if let Some(port) = attach_tcp_port {
        let _ = daemon.ensure_attach_tcp(server::AttachBindAddr::loopback(), port, &rt)?;
    }
    daemon.mark_attach_serving();

    // Arm signal-driven shutdown for the serving lifetime.
    install_signal_handler(&daemon, &rt);

    info!("namespace listeners ready");
    telemetry.daemon_event(
        DaemonEvent::FrontendServing,
        telemetry_backend,
        registry.runtime_entries().len(),
    );
    daemon.serve();

    // Drop every provider after the shutdown latch releases.
    let served_mounts = registry.runtime_entries().len();
    telemetry.daemon_event(
        DaemonEvent::FrontendStopped,
        telemetry_backend,
        served_mounts,
    );
    refresh_loop.abort();
    registry.shutdown_all();
    telemetry.daemon_event(DaemonEvent::DaemonStop, telemetry_backend, served_mounts);
    // Graceful exit removes the record; a crash leaves it stale and the client
    // cleans it up on the next connect attempt.
    runtime_record.remove();
    remove_socket(&attach_socket_path);
    remove_socket(&vsock_attach_socket_path);
    remove_socket(&control_socket_path);
    Ok(())
}

/// Serve the fixed local attach socket over the shared namespace.
fn spawn_attach_listener(
    socket: crate::context::AttachSocket,
    namespace: &Arc<omnifs_engine::TreeNamespace>,
    instance_id: &str,
    rt: &Handle,
    observer: &Arc<dyn omnifs_vfs_wire::AttachObserver>,
) -> anyhow::Result<PathBuf> {
    let crate::context::AttachSocket { path, listener } = socket;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let ns = Arc::clone(namespace) as Arc<dyn omnifs_engine::Namespace>;
    info!(path = %path.display(), "serving local namespace attach socket");
    rt.spawn(omnifs_vfs_wire::serve_listener(
        ns,
        listener,
        instance_id.to_string(),
        None,
        Some(Arc::clone(observer)),
    ));
    Ok(path)
}

/// Remove the attach sockets on a graceful exit; a crash leaves them stale and
/// the next daemon unlinks them after a refused connect probe.
fn remove_socket(path: &PathBuf) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(%error, path = %path.display(), "failed to remove attach socket");
    }
}

/// On `SIGTERM`/`SIGINT`, signal the same shutdown latch as the control API.
/// Armed for the serving lifetime; a signal during the
/// brief pre-serve startup window is not handled (the steady-state stop, service
/// and teardown paths all signal a serving daemon).
#[cfg(unix)]
fn install_signal_handler(daemon: &Arc<server::Daemon>, rt: &Handle) {
    use tokio::signal::unix::{SignalKind, signal};

    let daemon = Arc::clone(daemon);
    rt.spawn(async move {
        let (mut term, mut interrupt) = match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(term), Ok(interrupt)) => (term, interrupt),
            (term, interrupt) => {
                if let Err(error) = term {
                    warn!(%error, "failed to install SIGTERM handler");
                }
                if let Err(error) = interrupt {
                    warn!(%error, "failed to install SIGINT handler");
                }
                return;
            },
        };
        let signal = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        };
        info!(signal, "received shutdown signal");
        daemon.trigger_shutdown();
    });
}

#[cfg(not(unix))]
fn install_signal_handler(_daemon: &Arc<server::Daemon>, _rt: &Handle) {}
