//! `omnifs-nfs`: the out-of-process NFS frontend runner.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use omnifs_engine::{Namespace, NsAttachEvent};
use omnifs_vfs_wire::{
    AttachEvent, AttachTarget, FrontendIdentity, FrontendKind, WireNamespace,
    resolve_ready_vsock_port,
};
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tracing::{info, warn};

const ATTACH_CAPACITY: usize = 16;

#[derive(Parser, Debug)]
#[command(
    name = "omnifs-nfs",
    version,
    about = "The out-of-process omnifs NFS frontend runner"
)]
struct Args {
    /// Host-visible mount point to serve.
    #[arg(long)]
    mount_point: PathBuf,
    /// Directory for mount discovery and persistent filehandle state.
    #[arg(long)]
    state_dir: PathBuf,
    /// Path to the daemon's local VFS attach socket. When absent, the attach
    /// target comes from `OMNIFS_ATTACH_ADDR` and `OMNIFS_ATTACH_TOKEN`.
    #[arg(long)]
    attach: Option<PathBuf>,
    /// Loopback NFS server port. Zero asks the OS for an ephemeral port.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();
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
        instance = %namespace.instance_id(),
        "attached to namespace"
    );

    let attach_events = attach_events(&handle, &namespace);
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
        Some(attach_events),
    )
    .context("serve the NFS mount")?;

    info!(mount = %args.mount_point.display(), "frontend exited");
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

fn attach_events(
    runtime: &Handle,
    namespace: &Arc<WireNamespace>,
) -> broadcast::Receiver<NsAttachEvent> {
    let (events, receiver) = broadcast::channel(ATTACH_CAPACITY);
    let mut wire_events = namespace.subscribe_attach_events();
    drop(runtime.spawn(async move {
        while let Ok(AttachEvent::Reattached {
            old_instance,
            new_instance,
        }) = wire_events.recv().await
        {
            warn!(
                %old_instance,
                %new_instance,
                "daemon restarted under the frontend; invalidating cached node ids"
            );
            let _ = events.send(NsAttachEvent::Reattached);
        }
    }));
    receiver
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
