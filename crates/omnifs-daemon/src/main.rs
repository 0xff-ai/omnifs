//! omnifsd: the omnifs runtime daemon.
//!
//! Loads the provider registry, serves the control API, and serves the FUSE
//! mount until unmounted. Runs as the runtime container's entrypoint today,
//! but must stay free of container assumptions so it can later run
//! host-native (see `docs/design/daemon-cli-split.md`).

use clap::Parser;
use omnifs_daemon::{frontends, server};
use omnifs_home::{PathOverrides, Paths};
use omnifs_host::Dirs;
use omnifs_host::cloner::GitCloner;
use omnifs_host::inspector;
use omnifs_host::registry::ProviderRegistry;
use std::net::SocketAddr;
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
    /// Config directory. Defaults to `$OMNIFS_CONFIG_DIR`, then
    /// `$OMNIFS_HOME`, then `~/.omnifs`.
    #[arg(long)]
    config_dir: Option<PathBuf>,
    /// Cache directory. Defaults to `$OMNIFS_CACHE_DIR`, then
    /// `$OMNIFS_HOME/cache`, then `~/.omnifs/cache`.
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
        ..PathOverrides::default()
    });

    std::fs::create_dir_all(&args.mount_point)?;
    std::fs::create_dir_all(&paths.cache_dir)?;

    let cloner = Arc::new(GitCloner::new(paths.cache_dir.clone()));
    let dirs = Dirs::new(cloner.cache_dir(), &paths.config_dir, &paths.providers_dir);

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

    let frontends = frontends::Frontends::fuse(
        args.mount_point.clone(),
        Arc::clone(&registry),
        omnifs_fuse::new_notifier_handle(),
    );
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

    info!(mount_point = %daemon.mount_point().display(), "starting FUSE mount");
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
