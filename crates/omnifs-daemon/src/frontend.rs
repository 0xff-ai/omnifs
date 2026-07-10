//! The out-of-process frontend runner: `omnifs frontend`.
//!
//! It attaches a [`WireNamespace`] to a daemon-served namespace socket and runs
//! the same renderer entry the daemon uses (`omnifs_fuse::mount::run_blocking` /
//! `omnifs_nfs::mount_blocking`) over it, blocking until unmount. This is the
//! runtime-redesign phase-3 proof that a renderer can serve the projected tree
//! from a different process than the one that owns the projection.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use omnifs_engine::{Namespace, NsAttachEvent};
use omnifs_namespace_wire::{AttachTarget, WireNamespace};
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::app::FrontendKind;

/// Attach-event fan-out capacity. Reattach events are rare; a small ring is
/// plenty and a lagging consumer re-syncs on the next reattach.
const ATTACH_CAPACITY: usize = 16;

/// Arguments for the hidden `omnifs frontend` subcommand.
#[derive(Args, Debug)]
pub struct FrontendArgs {
    /// Path to the daemon's namespace attach socket to connect to
    /// (`$OMNIFS_HOME/frontends/<name>.sock`).
    #[arg(long)]
    pub attach: PathBuf,
    /// Renderer protocol to mount: `fuse` (Linux only) or `nfs`.
    #[arg(long, value_enum)]
    pub kind: FrontendKind,
    /// Host-visible mount point to serve the projected tree at.
    #[arg(long)]
    pub mount_point: PathBuf,
    /// NFS mount-state directory. Defaults under the workspace cache dir.
    #[arg(long)]
    pub nfs_state_dir: Option<PathBuf>,
    /// NFS server port to bind. A restarted runner must rebind the SAME port the
    /// kernel client is connected to, so a restartable frontend pins it here; `0`
    /// (the default) binds an ephemeral port.
    #[arg(long, default_value_t = 0)]
    pub nfs_port: u16,
}

/// Attach to the namespace socket and run the requested renderer, blocking until
/// the mount is torn down. Expects to run on a tokio runtime (the CLI's), like
/// [`run`](crate::run); it never nests a runtime.
pub fn run_frontend(args: FrontendArgs) -> anyhow::Result<()> {
    #[cfg(not(target_os = "linux"))]
    if args.kind == FrontendKind::Fuse {
        anyhow::bail!("the fuse frontend is only available on Linux");
    }

    let rt = Handle::current();
    let namespace = attach_blocking(&rt, args.attach.clone())?;
    info!(
        socket = %args.attach.display(),
        instance = %namespace.instance_id(),
        "attached to namespace socket"
    );

    // Bridge the wire attach events onto the engine-owned `NsAttachEvent` a
    // frontend acts on, so the NFS renderer re-resolves on a daemon restart
    // without omnifs-nfs depending on the wire crate. This also logs every
    // reattach and keeps the wire broadcast drained.
    let attach_tx = spawn_reattach_bridge(&rt, &namespace);

    let namespace_dyn = Arc::clone(&namespace) as Arc<dyn Namespace>;
    install_signal_handler(&rt, args.kind, args.mount_point.clone());

    match args.kind {
        #[cfg(target_os = "linux")]
        FrontendKind::Fuse => {
            let notifier = omnifs_fuse::new_notifier_handle();
            omnifs_fuse::mount::run_blocking(&args.mount_point, namespace_dyn, &rt, &notifier)?;
        },
        #[cfg(not(target_os = "linux"))]
        FrontendKind::Fuse => unreachable!("the fuse frontend is rejected off Linux above"),
        FrontendKind::Nfs => {
            let state_dir = match args.nfs_state_dir {
                Some(dir) => dir,
                None => omnifs_workspace::layout::WorkspaceLayout::resolve()?.nfs_state_dir(),
            };
            let mut options = omnifs_nfs::NfsMountOptions::loopback(state_dir);
            // The out-of-process runner is the restartable unit: persist the
            // filehandle table and pin the server port so a restart decodes the
            // handles the kernel client still holds.
            options.persist_filehandles = true;
            options.bind = std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                args.nfs_port,
            );
            omnifs_nfs::mount_blocking(
                &args.mount_point,
                namespace_dyn,
                rt.clone(),
                &options,
                Some(attach_tx.subscribe()),
            )?;
        },
    }
    info!(mount = %args.mount_point.display(), "frontend exited");
    Ok(())
}

/// Attach on the runtime, blocking this thread on the result. Mirrors the
/// daemon's blocking-thread pattern: the attach future runs on a worker while the
/// calling thread waits, so no nested runtime is created.
fn attach_blocking(rt: &Handle, socket: PathBuf) -> anyhow::Result<Arc<WireNamespace>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let rt_for_task = rt.clone();
    rt.spawn(async move {
        let _ = tx.send(WireNamespace::attach(AttachTarget::Unix(socket), rt_for_task).await);
    });
    rx.recv()
        .map_err(|_| anyhow::anyhow!("attach task dropped before completing"))?
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Log every reattach (a reconnect that lands on a restarted daemon), keep the
/// wire attach-event broadcast drained, and forward each reattach as an engine
/// [`NsAttachEvent`] a renderer acts on. Returns the sender the renderer
/// subscribes to.
fn spawn_reattach_bridge(
    rt: &Handle,
    namespace: &Arc<WireNamespace>,
) -> broadcast::Sender<NsAttachEvent> {
    let (tx, _) = broadcast::channel(ATTACH_CAPACITY);
    let mut receiver = namespace.subscribe_attach_events();
    let forward = tx.clone();
    rt.spawn(async move {
        while let Ok(event) = receiver.recv().await {
            let omnifs_namespace_wire::AttachEvent::Reattached {
                old_instance,
                new_instance,
            } = event;
            warn!(
                %old_instance, %new_instance,
                "daemon restarted under the frontend; re-resolving cached node ids"
            );
            // A closed channel means the renderer never subscribed (a non-NFS
            // kind) or has exited; the log above is still useful.
            let _ = forward.send(NsAttachEvent::Reattached);
        }
    });
    tx
}

/// On `SIGTERM`/`SIGINT`, unmount the mount point so the blocking renderer loop
/// unblocks and the runner exits. Mirrors the daemon's signal handling.
#[cfg(unix)]
fn install_signal_handler(rt: &Handle, kind: FrontendKind, mount_point: PathBuf) {
    use tokio::signal::unix::{SignalKind, signal};

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
        info!(signal, "received shutdown signal; unmounting frontend");
        let result = match kind {
            #[cfg(target_os = "linux")]
            FrontendKind::Fuse => {
                omnifs_fuse::mount::unmount(&mount_point).map_err(|error| error.to_string())
            },
            #[cfg(not(target_os = "linux"))]
            FrontendKind::Fuse => Ok(()),
            FrontendKind::Nfs => {
                omnifs_nfs::unmount(&mount_point).map_err(|error| error.to_string())
            },
        };
        if let Err(error) = result {
            warn!(%error, "frontend self-unmount failed");
        }
    });
}

#[cfg(not(unix))]
fn install_signal_handler(_rt: &Handle, _kind: FrontendKind, _mount_point: PathBuf) {}
