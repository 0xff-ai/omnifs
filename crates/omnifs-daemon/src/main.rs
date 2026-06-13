//! omnifsd: the omnifs runtime daemon.
//!
//! Loads the provider registry, serves the control API, and serves the FUSE
//! mount until unmounted. Runs as the runtime container's entrypoint today,
//! but must stay free of container assumptions so it can later run
//! host-native (see `docs/design/daemon-cli-split.md`).

use clap::{Parser, ValueEnum};
use omnifs_daemon::{frontends, server};
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

#[derive(Parser, Debug)]
#[command(name = "omnifsd", version, about = "omnifs runtime daemon")]
struct Args {
    /// Directory to serve the FUSE filesystem at.
    #[arg(long)]
    mount_point: PathBuf,
    /// Filesystem frontend to serve.
    #[arg(long, value_enum, default_value_t = FrontendKind::Fuse)]
    frontend: FrontendKind,
    /// NFS loopback listen port. 0 asks the OS for an ephemeral port.
    #[arg(long, default_value_t = 0)]
    nfs_port: u16,
    /// Directory for NFS mount-state files. Defaults under the cache dir.
    #[arg(long)]
    nfs_state_dir: Option<PathBuf>,
    /// Optional NFS trace log.
    #[arg(long)]
    nfs_trace: Option<PathBuf>,
    /// Config directory. Defaults through omnifs home resolution.
    #[arg(long)]
    config_dir: Option<PathBuf>,
    /// Cache directory. Defaults through omnifs home resolution.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Control API listen address. The container entrypoint passes
    /// `0.0.0.0` so Docker can publish the port on the host loopback.
    #[arg(long, default_value_t = default_listen())]
    listen: SocketAddr,
    /// Maintain `/<mount>` → `<mount-point>/<mount>` convenience symlinks
    /// as mounts come and go. Container-image nicety; off by default and
    /// meaningless when running host-native.
    #[arg(long)]
    root_symlinks: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum FrontendKind {
    Fuse,
    Nfs,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], omnifs_api::DEFAULT_PORT))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    run(Args::parse())
}

fn run(args: Args) -> anyhow::Result<()> {
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
        #[cfg(target_os = "linux")]
        FrontendKind::Fuse => frontends::Frontends::fuse(
            args.mount_point.clone(),
            Arc::clone(&registry),
            omnifs_fuse::new_notifier_handle(),
        ),
        #[cfg(not(target_os = "linux"))]
        FrontendKind::Fuse => anyhow::bail!(
            "the fuse frontend is only available on Linux; use --frontend nfs for host-native mounts"
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

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(filter)
        .init();
}
