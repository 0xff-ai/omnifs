//! Daemon entrypoint: argument surface and the blocking run loop.
//!
//! These are invoked by the `omnifs daemon` subcommand (the single-binary
//! entrypoint); there is no standalone `omnifsd` binary. The daemon still
//! runs as its own process and speaks the HTTP control API; it must stay
//! free of container assumptions so it can later run host-native (see
//! `docs/contracts/50-control-plane.md`).

use clap::{Args, ValueEnum};
use omnifs_engine::GitCloner;
use omnifs_engine::MountRuntimes;
use omnifs_engine::init_global_from_env;
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
    pub nfs_port: u16,
    /// Directory for NFS mount-state files. Defaults under the cache dir.
    #[arg(long)]
    pub nfs_state_dir: Option<PathBuf>,
    /// Optional NFS trace log.
    #[arg(long)]
    pub nfs_trace: Option<PathBuf>,
    /// Control API listen address. The container entrypoint passes
    /// `0.0.0.0` so Docker can publish the port on the host loopback.
    #[arg(long, default_value_t = default_listen())]
    pub listen: SocketAddr,
    /// Maintain `/<mount>` → `<mount-point>/<mount>` convenience symlinks
    /// as mounts come and go. Container-image nicety; off by default and
    /// meaningless when running host-native.
    #[arg(long)]
    pub root_symlinks: bool,
    /// Serve a host-native mount: open preopen directories directly instead of
    /// rewriting them to container bind paths. The native launcher sets this;
    /// the container entrypoint does not (it runs in container/rewrite mode).
    #[arg(long)]
    pub host_native: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum FrontendKind {
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
}

fn default_listen() -> SocketAddr {
    omnifs_api::default_listen_addr()
}

impl DaemonArgs {
    pub fn host_native(listen: SocketAddr) -> Self {
        Self {
            listen,
            host_native: true,
            nfs_port: 0,
            nfs_state_dir: None,
            nfs_trace: None,
            root_symlinks: false,
        }
    }

    /// Serialize to argv for `omnifs daemon …`. Includes the `daemon` subcommand
    /// token as the first element.
    pub fn to_argv(&self) -> Vec<String> {
        let mut args = vec!["daemon".to_string()];
        args.push("--listen".to_string());
        args.push(self.listen.to_string());
        if self.nfs_port != 0 {
            args.push("--nfs-port".to_string());
            args.push(self.nfs_port.to_string());
        }
        push_option_path(&mut args, "--nfs-state-dir", self.nfs_state_dir.as_ref());
        push_option_path(&mut args, "--nfs-trace", self.nfs_trace.as_ref());
        if self.root_symlinks {
            args.push("--root-symlinks".to_string());
        }
        if self.host_native {
            args.push("--host-native".to_string());
        }
        args
    }
}

fn push_option_path(args: &mut Vec<String>, flag: &str, value: Option<&PathBuf>) {
    if let Some(path) = value {
        args.push(flag.to_string());
        args.push(path.display().to_string());
    }
}

/// Bring up the registry, control API, and filesystem frontend, then serve
/// until unmounted. Blocks; expects to run on a tokio runtime (the caller
/// owns runtime and tracing setup).
pub fn run(args: DaemonArgs) -> anyhow::Result<()> {
    use omnifs_workspace::telemetry::{self, DaemonEvent, TelemetrySink};

    let context = DaemonContext::resolve(args)?;
    context.prepare_startup_dirs()?;

    // Local-only dogfood counters. No config channel reaches the daemon today,
    // so the off-switch is the `OMNIFS_TELEMETRY` env var (the CLI propagates
    // its `[telemetry] enabled = false` into it when launching the daemon).
    let telemetry_backend = context.telemetry_backend();
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

    let frontend = context.frontend();
    let frontends = frontends::Frontend::from_context(&context, Arc::clone(&registry));
    let listener = context.bind_control_listener()?;
    let daemon = Arc::new(server::Daemon::new(
        context,
        Arc::clone(&registry),
        sink,
        frontends,
    ));
    daemon.spawn_control(listener, &rt)?;

    // Load desired state from `mounts/*.json` before serving, so the tree is
    // populated when the frontend comes up. Both the native and Docker launch
    // paths reconcile here; the daemon backend selects host-direct versus
    // container-rewritten preopens.
    let report = daemon.reconcile_blocking(&rt);
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

    // Arm signal-driven shutdown for the serving lifetime: a service `stop`, the
    // macOS teardown's `kill -TERM`, or `docker stop` all run the same clean
    // unmount the `POST /v1/shutdown` handler does, instead of hard-killing the
    // process and stranding the mount.
    install_signal_handler(&daemon, &rt);

    info!(
        frontend = ?frontend,
        mount_point = %daemon.mount_point().display(),
        "starting filesystem frontend"
    );
    telemetry.daemon_event(
        DaemonEvent::FrontendServing,
        telemetry_backend,
        registry.runtime_entries().len(),
    );
    let serve_result = daemon.serve(&rt);

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
    serve_result?;
    Ok(())
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
