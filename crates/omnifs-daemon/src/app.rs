//! Daemon entrypoint: argument surface and the blocking run loop.
//!
//! These are invoked by the `omnifs daemon` subcommand (the single-binary
//! entrypoint); there is no standalone `omnifsd` binary. The daemon still
//! runs as its own host-native process and speaks the HTTP control API.

use clap::Args;
use omnifs_engine::GitCloner;
use omnifs_engine::MountRuntimes;
use omnifs_engine::init_global_from_env;
use omnifs_workspace::runtime_record::RuntimeRecord;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::{info, warn};

use crate::{context::DaemonContext, frontends, server};

/// Arguments for the `omnifs daemon` subcommand (the runtime daemon).
#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// NFS loopback listen port. 0 asks the OS for an ephemeral port.
    #[arg(long, default_value_t = 0)]
    pub(crate) nfs_port: u16,
    /// Directory for NFS mount-state files. Defaults under the cache dir.
    #[arg(long)]
    pub(crate) nfs_state_dir: Option<PathBuf>,
    /// Optional NFS trace log.
    #[arg(long)]
    pub(crate) nfs_trace: Option<PathBuf>,
    /// Optional TCP control API listen address. The daemon always serves its
    /// Unix socket and adds TCP only for this debug/test path.
    #[arg(long)]
    pub(crate) listen: Option<SocketAddr>,
    /// Serve a specific frontend at a mount point, as `<kind>=<mount_point>`
    /// (`fuse` or `nfs`), repeatable. Absent: the platform-default frontend at
    /// the resolved mount point. Present: serve exactly the listed set, each at
    /// its own mount point (duplicate mount points are rejected). This is a
    /// daemon surface only; the CLI does not expose it yet.
    #[arg(long = "frontend", value_name = "KIND=MOUNT_POINT")]
    pub(crate) frontends: Vec<FrontendMount>,
    /// Serve the shared namespace over an attach socket at
    /// `$OMNIFS_HOME/frontends/<name>.sock`, repeatable. An out-of-process
    /// `omnifs-fuse` runner attaches to it. A daemon with an attach socket
    /// and no `--frontend` serves the namespace only (no in-process mount).
    /// `<name>` is a bare `[a-z0-9-]+` label.
    #[arg(long = "attach-socket", value_name = "NAME", value_parser = parse_attach_socket_name)]
    pub(crate) attach_sockets: Vec<String>,
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

/// Validate a `--attach-socket <name>` value: a non-empty `[a-z0-9-]+` label,
/// used as the stem of `frontends/<name>.sock`.
fn parse_attach_socket_name(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("attach socket name must not be empty".to_string());
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!(
            "invalid attach socket name `{value}`; use lowercase letters, digits, and `-`"
        ));
    }
    Ok(value.to_string())
}

/// One requested frontend: a protocol kind bound to a mount point. Built from the
/// `--frontend <kind>=<mount_point>` flag and by [`DaemonContext`] when it fills
/// in the platform default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrontendMount {
    pub(crate) kind: FrontendKind,
    pub(crate) mount_point: PathBuf,
}

impl std::str::FromStr for FrontendMount {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (kind, mount) = value
            .split_once('=')
            .ok_or_else(|| format!("expected `<kind>=<mount_point>`, got `{value}`"))?;
        let kind = match kind {
            "fuse" => FrontendKind::Fuse,
            "nfs" => FrontendKind::Nfs,
            other => {
                return Err(format!(
                    "unknown frontend kind `{other}`; expected `fuse` or `nfs`"
                ));
            },
        };
        if mount.is_empty() {
            return Err("frontend mount point must not be empty".to_string());
        }
        Ok(Self {
            kind,
            mount_point: PathBuf::from(mount),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrontendKind {
    Fuse,
    Nfs,
}

impl FrontendKind {
    /// FUSE on Linux (native and inside the container), NFS loopback elsewhere
    /// (macOS host-native). The daemon owns this choice; the CLI does not pass a
    /// frontend flag.
    #[cfg(target_os = "linux")]
    pub(crate) fn platform_default() -> Self {
        Self::Fuse
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn platform_default() -> Self {
        Self::Nfs
    }

    /// The `--frontend` flag token for this kind (`fuse` or `nfs`).
    pub(crate) fn as_flag(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }
}

/// A `fuse=/a, nfs=/b` rendering of the served frontend set for the startup log.
fn frontend_summary(context: &DaemonContext) -> String {
    context
        .frontends()
        .iter()
        .map(|frontend| {
            format!(
                "{}={}",
                frontend.kind.as_flag(),
                frontend.mount_point.display()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Bring up the registry, control API, and filesystem frontend, then serve
/// until unmounted. Blocks; expects to run on a tokio runtime (the caller
/// owns runtime and tracing setup).
pub fn run(args: DaemonArgs) -> anyhow::Result<()> {
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
        info!(
            mount_point = %context.mount_point().display(),
            config = %host_context.config_dir().display(),
            cache = %cloner.cache_dir().display(),
            providers = %host_context.providers_dir().display(),
            "starting daemon"
        );
        Arc::new(MountRuntimes::new(host_context, Arc::clone(&cloner))?)
    };

    // Proactively refreshes every registered OAuth credential before it enters
    // its refresh window, so a request-path authorization call almost never
    // has to await a live refresh. Spawned on the shared credential service
    // (not per-mount) so a credential registered by any later reconcile is
    // picked up without restarting the loop.
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

    let frontend_summary = frontend_summary(&context);
    let frontends = frontends::Frontends::from_context(&context);

    // The Unix socket is always present; `--listen` adds the debug/test TCP
    // listener. Bind before moving the context into the daemon.
    let record_path = context.runtime_record_file();
    let runtime_record = context.runtime_record();
    let uds_listener = context.bind_control_socket()?;

    // Bind the namespace attach sockets (fail fast on a collision), capturing the
    // per-start id and socket paths before the context moves into the daemon. The
    // listeners are spawned post-reconcile so a client sees a populated tree.
    let attach_sockets = context.bind_attach_sockets()?;
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
        frontends,
        control_token,
    ));
    daemon.spawn_control_unix(uds_listener, &rt)?;
    if let Some(listener) = tcp_listener {
        daemon.spawn_control_tcp(listener, &rt)?;
    }

    if let Err(error) = runtime_record.write(&record_path) {
        warn!(%error, path = %record_path.display(), "failed to write runtime record");
    }

    // Load desired state from `mounts/*.json` before serving, so the tree is
    // populated when the frontend comes up.
    log_reconcile(&daemon.reconcile_blocking(&rt));

    // Build the one shared namespace after reconcile, so its root record reflects
    // the converged mount set (the identity table's root is installed at
    // construction). Both the in-process renderers and the attach-socket
    // listeners serve this same `TreeNamespace`.
    let namespace = omnifs_engine::TreeNamespace::new(Arc::clone(&registry), rt.clone());
    // Give the daemon a handle to the namespace so `POST /v1/frontend/attach-target`
    // can bind a TCP attach listener on a running daemon without a restart.
    daemon.set_namespace(Arc::clone(&namespace));

    // Serve every requested attach socket over the shared namespace, before
    // `serve` (which blocks) so a namespace-only daemon comes up. With the
    // listeners up and mounts reconciled, report ready.
    let attach_socket_paths = spawn_attach_listeners(
        attach_sockets,
        &namespace,
        &attach_instance_id,
        &rt,
        &daemon.attach_observer(omnifs_api::FrontendDelivery::Local),
    )?;
    if let Some(port) = attach_tcp_port
        && let Err(error) = daemon.ensure_attach_tcp(server::AttachBindAddr::loopback(), port, &rt)
    {
        warn!(%error, "failed to bind the requested TCP attach listener");
    }
    daemon.mark_attach_serving();

    // Arm signal-driven shutdown for the serving lifetime: a service `stop`, the
    // macOS teardown's `kill -TERM`, or `docker stop` all run the same clean
    // unmount the `POST /v1/shutdown` handler does, instead of hard-killing the
    // process and stranding the mount.
    install_signal_handler(&daemon, &rt);

    info!(
        frontends = %frontend_summary,
        mount_point = %daemon.mount_point().display(),
        "starting filesystem frontends"
    );
    telemetry.daemon_event(
        DaemonEvent::FrontendServing,
        telemetry_backend,
        registry.runtime_entries().len(),
    );
    let serve_result = daemon.serve(&namespace, &rt);

    // `serve` returns once the frontend is unmounted (externally, by a signal,
    // or by the daemon's own shutdown path). Drop every provider here so
    // teardown is symmetric across FUSE and NFS rather than living in one
    // frontend crate. Record the stop counters before dropping providers so the
    // session's mount count is preserved even on a serve error.
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
    if let Err(error) = RuntimeRecord::remove(&record_path) {
        warn!(%error, path = %record_path.display(), "failed to remove runtime record");
    }
    remove_attach_sockets(&attach_socket_paths);
    serve_result?;
    Ok(())
}

/// Log the outcome of the startup reconcile: a warning per dark mount, then a
/// one-line summary.
fn log_reconcile(report: &omnifs_api::ReconcileReport) {
    for failure in &report.failed {
        warn!(mount = %failure.mount, reason = %failure.reason, "mount did not converge");
    }
    info!(
        added = report.added.len(),
        updated = report.updated.len(),
        removed = report.removed.len(),
        failed = report.failed.len(),
        "reconciled mounts on start"
    );
}

/// Serve each bound attach socket over the shared namespace on the runtime.
/// `observer` is shared by every socket: each is a plain, filesystem-permission
/// `--attach-socket <name>` listener, so they all carry the same
/// [`omnifs_api::FrontendDelivery::Local`] label.
fn spawn_attach_listeners(
    sockets: Vec<crate::context::AttachSocket>,
    namespace: &Arc<omnifs_engine::TreeNamespace>,
    instance_id: &str,
    rt: &Handle,
    observer: &Arc<dyn omnifs_vfs_wire::AttachObserver>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(sockets.len());
    for crate::context::AttachSocket {
        name,
        path,
        listener,
    } in sockets
    {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        let ns = Arc::clone(namespace) as Arc<dyn omnifs_engine::Namespace>;
        info!(%name, path = %path.display(), "serving namespace attach socket");
        rt.spawn(omnifs_vfs_wire::serve_listener(
            ns,
            listener,
            instance_id.to_string(),
            None,
            Some(Arc::clone(observer)),
        ));
        paths.push(path);
    }
    Ok(paths)
}

/// Remove the attach sockets on a graceful exit; a crash leaves them stale and
/// the next daemon unlinks them after a refused connect probe.
fn remove_attach_sockets(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(%error, path = %path.display(), "failed to remove attach socket");
        }
    }
}

/// On `SIGTERM`/`SIGINT`, run the same self-unmount the control API's
/// `POST /v1/shutdown` triggers. The unmount unblocks `serve`, which then drops
/// providers and exits. Armed for the serving lifetime; a signal during the
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
        info!(signal, "received shutdown signal; unmounting");
        daemon.trigger_shutdown();
    });
}

#[cfg(not(unix))]
fn install_signal_handler(_daemon: &Arc<server::Daemon>, _rt: &Handle) {}
