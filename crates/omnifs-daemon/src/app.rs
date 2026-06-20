//! Daemon entrypoint: argument surface and the blocking run loop.
//!
//! These are invoked by the `omnifs daemon` subcommand (the single-binary
//! entrypoint); there is no standalone `omnifsd` binary. The daemon still
//! runs as its own process and speaks the HTTP control API; it must stay
//! free of container assumptions so it can later run host-native (see
//! `docs/design/daemon-cli-split.md`).

use clap::{Args, ValueEnum};
use omnifs_api::DaemonBackend;
use omnifs_home::{PathOverrides, Paths};
use omnifs_host::Dirs;
use omnifs_host::cloner::GitCloner;
use omnifs_host::inspector;
use omnifs_host::registry::ProviderRegistry;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::{info, warn};

use crate::{frontends, server};

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
    /// Config directory. Defaults through omnifs home resolution.
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
    /// Cache directory. Defaults through omnifs home resolution.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
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
    fn platform_default() -> Self {
        Self::Fuse
    }

    #[cfg(not(target_os = "linux"))]
    fn platform_default() -> Self {
        Self::Nfs
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], omnifs_api::DEFAULT_PORT))
}

impl DaemonArgs {
    /// Serialize to argv for `omnifs daemon …`. Includes the `daemon` subcommand
    /// token as the first element.
    pub fn to_argv(&self) -> Vec<String> {
        let mut args = vec!["daemon".to_string()];
        push_option_path(&mut args, "--config-dir", &self.config_dir);
        push_option_path(&mut args, "--cache-dir", &self.cache_dir);
        args.push("--listen".to_string());
        args.push(self.listen.to_string());
        if self.nfs_port != 0 {
            args.push("--nfs-port".to_string());
            args.push(self.nfs_port.to_string());
        }
        push_option_path(&mut args, "--nfs-state-dir", &self.nfs_state_dir);
        push_option_path(&mut args, "--nfs-trace", &self.nfs_trace);
        if self.root_symlinks {
            args.push("--root-symlinks".to_string());
        }
        if self.host_native {
            args.push("--host-native".to_string());
        }
        args
    }
}

fn push_option_path(args: &mut Vec<String>, flag: &str, value: &Option<PathBuf>) {
    if let Some(path) = value {
        args.push(flag.to_string());
        args.push(path.display().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_argv_native_launch_emits_expected_flags() {
        let args = DaemonArgs {
            config_dir: Some("/cfg".into()),
            cache_dir: Some("/cache".into()),
            listen: "127.0.0.1:7711".parse().expect("valid address"),
            host_native: true,
            nfs_port: 0,
            nfs_state_dir: None,
            nfs_trace: None,
            root_symlinks: false,
        };
        assert_eq!(
            args.to_argv(),
            vec![
                "daemon".to_string(),
                "--config-dir".to_string(),
                "/cfg".to_string(),
                "--cache-dir".to_string(),
                "/cache".to_string(),
                "--listen".to_string(),
                "127.0.0.1:7711".to_string(),
                "--host-native".to_string(),
            ]
        );
    }
}

/// Resolve the host-visible mount point the daemon serves at. The container
/// entrypoint exports `OMNIFS_MOUNT_POINT`; host-native falls back to
/// `$HOME/omnifs`, deliberately outside `OMNIFS_HOME` so the mounted tree lives
/// at a normal user-owned location.
fn resolve_mount_point() -> anyhow::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("OMNIFS_MOUNT_POINT") {
        return Ok(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow::anyhow!("cannot resolve mount point: set HOME or OMNIFS_MOUNT_POINT")
    })?;
    Ok(PathBuf::from(home).join("omnifs"))
}

/// Bring up the registry, control API, and filesystem frontend, then serve
/// until unmounted. Blocks; expects to run on a tokio runtime (the caller
/// owns runtime and tracing setup).
pub fn run(args: DaemonArgs) -> anyhow::Result<()> {
    let paths = Paths::resolve(PathOverrides {
        config_dir: args.config_dir,
        cache_dir: args.cache_dir,
    })?;
    let frontend = FrontendKind::platform_default();
    let mount_point = resolve_mount_point()?;

    std::fs::create_dir_all(&mount_point)?;
    std::fs::create_dir_all(&paths.cache_dir)?;

    let cloner = Arc::new(GitCloner::new(paths.cache_dir.clone()));
    let dirs = Dirs::new(
        cloner.cache_dir(),
        &paths.config_dir,
        &paths.providers_dir,
        &paths.credentials_file,
    );

    info!(
        mount_point = %mount_point.display(),
        config = %dirs.config_dir.display(),
        cache = %cloner.cache_dir().display(),
        providers = %dirs.providers_dir.display(),
        "starting daemon"
    );

    let registry = Arc::new(ProviderRegistry::new(dirs, Arc::clone(&cloner))?);
    let rt = Handle::current();
    let sink = inspector::init_global_from_env();
    if let Some(sink) = &sink {
        if let Some(path) = sink.tee_path() {
            info!(path = %path.display(), "inspector stream enabled (in-memory ring + file tee)");
        } else {
            info!("inspector stream enabled (in-memory ring only)");
        }
    }

    let frontends = match frontend {
        #[cfg(target_os = "linux")]
        FrontendKind::Fuse => frontends::Frontends::fuse(
            mount_point.clone(),
            Arc::clone(&registry),
            omnifs_fuse::new_notifier_handle(),
        ),
        #[cfg(not(target_os = "linux"))]
        FrontendKind::Fuse => anyhow::bail!(
            "the fuse frontend is only available on Linux; host-native uses the NFS loopback"
        ),
        FrontendKind::Nfs => {
            let mut options = omnifs_nfs::NfsMountOptions::loopback(
                args.nfs_state_dir.unwrap_or_else(|| paths.nfs_state_dir()),
            );
            options.bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.nfs_port);
            options.trace_path = args.nfs_trace;
            options.config_dir = Some(paths.config_dir.clone());
            options.cache_dir = Some(paths.cache_dir.clone());
            frontends::Frontends::nfs(mount_point.clone(), Arc::clone(&registry), options)
        },
    };
    // The native launcher passes `--host-native`; the container entrypoint does
    // not. This selects the preopen materialization mode (host-direct versus
    // container-rewritten) independently of `--root-symlinks`, which now means
    // only "maintain `/<mount>` convenience symlinks."
    let backend = if args.host_native {
        DaemonBackend::Native
    } else {
        DaemonBackend::Docker
    };
    let daemon = Arc::new(server::Daemon::new(
        Arc::clone(&registry),
        sink,
        frontends,
        args.root_symlinks,
        backend,
    ));
    let listener = std::net::TcpListener::bind(args.listen).map_err(|error| {
        anyhow::anyhow!(
            "cannot bind control API listener on {}: {error}\n\
             \n\
             Likely cause: another omnifs daemon is already running on that port.\n\
             Run `omnifs down` to stop it, then try again.",
            args.listen
        )
    })?;
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
    daemon.serve(&rt)?;

    // `serve` returns once the frontend is unmounted (externally, by a signal,
    // or by the daemon's own shutdown path). Drop every provider here so
    // teardown is symmetric across FUSE and NFS rather than living in one
    // frontend crate.
    registry.shutdown_all();
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
