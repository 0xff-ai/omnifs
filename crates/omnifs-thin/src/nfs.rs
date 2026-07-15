//! NFS runner command for `omnifs-thin`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Args as ClapArgs;
use omnifs_engine::Namespace;
use omnifs_vfs_wire::{
    AttachTarget, FrontendIdentity, FrontendKind, WireNamespace, resolve_ready_vsock_port,
};
use tokio::runtime::Handle;
use tracing::{info, warn};

/// Arguments for the `NFSv4` loopback frontend.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Host-visible mount point to serve.
    #[arg(long)]
    mount_point: PathBuf,
    /// Directory for mount discovery and persistent filehandle state.
    #[arg(long)]
    state_dir: PathBuf,
    /// Path to the daemon's local VFS attach socket. When absent, the attach
    /// target comes from the environment.
    #[arg(long)]
    attach: Option<PathBuf>,
    /// Loopback NFS server port. Zero asks the OS for an ephemeral port.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

pub(crate) fn run(args: Args) -> anyhow::Result<()> {
    crate::init_tracing();
    let ready_port =
        resolve_ready_vsock_port().context("resolve the readiness-beacon vsock port")?;
    let target = AttachTarget::resolve(args.attach).context("resolve the VFS attach target")?;
    let target_label = target.to_string();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the tokio runtime")?;
    let handle = runtime.handle().clone();
    let identity = FrontendIdentity {
        kind: FrontendKind::Nfs,
        mount_point: args.mount_point.clone(),
    };
    let namespace = runtime
        .block_on(WireNamespace::attach(target, identity, handle.clone()))
        .context("attach to the namespace")?;
    info!(
        target = %target_label,
        "attached to namespace"
    );

    #[cfg(target_os = "linux")]
    if let Some(port) = ready_port {
        omnifs_vfs_wire::spawn_ready_signal(&handle, args.mount_point.clone(), port);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = ready_port;
    install_signal_handler(&handle, args.mount_point.clone());
    let mut options = omnifs_nfs::NfsMountOptions::loopback(args.state_dir);
    options.persist_filehandles = true;
    options.bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.port);
    omnifs_nfs::mount_blocking(
        &args.mount_point,
        Arc::clone(&namespace) as Arc<dyn Namespace>,
        handle,
        &options,
    )
    .context("serve the NFS mount")?;

    info!(mount = %args.mount_point.display(), "frontend exited");
    Ok(())
}

#[cfg(unix)]
fn install_signal_handler(runtime: &Handle, mount_point: PathBuf) {
    use tokio::signal::unix::{SignalKind, signal};

    drop(runtime.spawn(async move {
        let (mut terminate, mut interrupt) = match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(terminate), Ok(interrupt)) => (terminate, interrupt),
            (terminate, interrupt) => {
                if let Err(error) = terminate {
                    warn!(%error, "failed to install SIGTERM handler");
                }
                if let Err(error) = interrupt {
                    warn!(%error, "failed to install SIGINT handler");
                }
                return;
            },
        };
        loop {
            let signal = tokio::select! {
                signal = terminate.recv() => signal.map(|()| "SIGTERM"),
                signal = interrupt.recv() => signal.map(|()| "SIGINT"),
            };
            let Some(signal) = signal else {
                return;
            };
            info!(signal, "received shutdown signal; unmounting frontend");
            match omnifs_nfs::unmount(&mount_point) {
                Ok(()) => return,
                Err(error) => {
                    warn!(%error, "frontend self-unmount failed; waiting for another signal");
                },
            }
        }
    }));
}

#[cfg(not(unix))]
fn install_signal_handler(_runtime: &Handle, _mount_point: PathBuf) {}
