//! Daemon entrypoint: argument surface and the blocking run loop.
//!
//! These are invoked by the `omnifs daemon` subcommand (the single-binary
//! entrypoint); there is no standalone `omnifsd` binary. The daemon still
//! runs as its own process and speaks the HTTP control API; it must stay
//! free of container assumptions so it can later run host-native (see
//! `docs/design/daemon-cli-split.md`).

use clap::{Args, ValueEnum};
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
    /// Directory to serve the FUSE filesystem at.
    #[arg(long)]
    pub mount_point: PathBuf,
    /// Filesystem frontend to serve.
    #[arg(long, value_enum, default_value_t = FrontendKind::Fuse)]
    pub frontend: FrontendKind,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum FrontendKind {
    Fuse,
    Nfs,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], omnifs_api::DEFAULT_PORT))
}

/// Bring up the registry, control API, and filesystem frontend, then serve
/// until unmounted. Blocks; expects to run on a tokio runtime (the caller
/// owns runtime and tracing setup).
pub fn run(args: DaemonArgs) -> anyhow::Result<()> {
    let paths = Paths::resolve(PathOverrides {
        config_dir: args.config_dir,
        cache_dir: args.cache_dir,
    })?;

    std::fs::create_dir_all(&args.mount_point)?;
    std::fs::create_dir_all(&paths.cache_dir)?;

    let cloner = Arc::new(GitCloner::new(paths.cache_dir.clone()));
    let dirs = Dirs::new(
        cloner.cache_dir(),
        &paths.config_dir,
        &paths.providers_dir,
        &paths.credentials_file,
    );

    info!(
        mount_point = %args.mount_point.display(),
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

    let frontends = match args.frontend {
        FrontendKind::Fuse => frontends::Frontends::fuse(
            args.mount_point.clone(),
            Arc::clone(&registry),
            omnifs_fuse::new_notifier_handle(),
        ),
        FrontendKind::Nfs => {
            let mut options = omnifs_nfs::NfsMountOptions::loopback(
                args.nfs_state_dir
                    .unwrap_or_else(|| paths.cache_dir.join("nfs")),
            );
            options.bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.nfs_port);
            options.trace_path = args.nfs_trace;
            options.config_dir = Some(paths.config_dir.clone());
            options.cache_dir = Some(paths.cache_dir.clone());
            frontends::Frontends::nfs(args.mount_point.clone(), Arc::clone(&registry), options)
        },
    };
    let daemon = Arc::new(server::Daemon::new(
        Arc::clone(&registry),
        sink,
        frontends,
        args.root_symlinks,
    ));
    match std::net::TcpListener::bind(args.listen) {
        Ok(listener) => daemon.spawn_control(listener, &rt)?,
        Err(error) => {
            warn!(%error, addr = %args.listen, "failed to bind control API listener");
        },
    }

    info!(
        frontend = ?args.frontend,
        mount_point = %daemon.mount_point().display(),
        "starting filesystem frontend"
    );
    daemon.serve(&rt)?;
    Ok(())
}
