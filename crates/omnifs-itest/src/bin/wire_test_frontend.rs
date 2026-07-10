//! `wire-test-frontend`: the out-of-process NFS wire-protocol test double.
//!
//! Attaches a wire-backed namespace to a daemon-served attach socket (or the
//! `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN` env target) and runs the NFS
//! renderer over it, blocking until unmount, exactly as the retired
//! `omnifs frontend run --kind nfs` runner did. Test-crate-owned on purpose:
//! the shipped runner binary (`omnifs-fuse`) is FUSE-only, while the wire
//! reattach/perf/parity acceptance lanes (`tests/wire_reattach`,
//! `tests/wire_perf`, `tests/multi_frontend`, `tests/frontend_docker`) need a
//! spawnable out-of-process NFS leg: a restartable unit that pins its NFS port
//! and reloads the persisted filehandle table, proving the ESTALE row of the
//! NFS quirk catalog (`docs/architecture/50-nfs-frontend.md`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use omnifs_engine::{Namespace, NsAttachEvent};
use omnifs_namespace_wire::{AttachEvent, AttachTarget, WireNamespace, resolve_attach_target};
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Attach-event fan-out capacity. Reattach events are rare; a small ring is
/// plenty and a lagging consumer re-syncs on the next reattach.
const ATTACH_CAPACITY: usize = 16;

/// Attach a wire-backed namespace and serve an NFS mount over it.
#[derive(Parser, Debug)]
#[command(
    name = "wire-test-frontend",
    about = "The out-of-process omnifs NFS wire-protocol test double"
)]
struct Args {
    /// Path to the daemon's namespace attach socket to connect to
    /// (`$OMNIFS_HOME/frontends/<name>.sock`). Mutually exclusive with the
    /// TCP attach path: when absent, `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN`
    /// in the environment select the attach target instead.
    #[arg(long)]
    attach: Option<PathBuf>,
    /// Host-visible mount point to serve the projected tree at.
    #[arg(long)]
    mount_point: PathBuf,
    /// NFS mount-state directory. Defaults under the workspace cache dir.
    #[arg(long)]
    nfs_state_dir: Option<PathBuf>,
    /// NFS server port to bind. A restarted runner must rebind the SAME port
    /// the kernel client is connected to, so a restartable frontend pins it
    /// here; `0` (the default) binds an ephemeral port.
    #[arg(long, default_value_t = 0)]
    nfs_port: u16,
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();

    let target =
        resolve_attach_target(args.attach).context("resolve the namespace attach target")?;
    let target_label = target.to_string();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the tokio runtime")?;
    let handle = rt.handle().clone();

    let namespace = attach_blocking(&handle, target)?;
    info!(
        target = %target_label,
        instance = %namespace.instance_id(),
        "attached to namespace"
    );

    // Bridge the wire attach events onto the engine-owned `NsAttachEvent` the
    // NFS renderer acts on, so it re-resolves on a daemon restart without
    // omnifs-nfs depending on the wire crate. This also logs every reattach
    // and keeps the wire broadcast drained.
    let attach_tx = spawn_reattach_bridge(&handle, &namespace);

    install_signal_handler(&handle, args.mount_point.clone());

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

    let namespace_dyn = Arc::clone(&namespace) as Arc<dyn Namespace>;
    omnifs_nfs::mount_blocking(
        &args.mount_point,
        namespace_dyn,
        handle,
        &options,
        Some(attach_tx.subscribe()),
    )
    .context("serve the NFS mount")?;

    info!(mount = %args.mount_point.display(), "frontend exited");
    Ok(())
}

/// Foreground logging: `RUST_LOG` if set (the test harness passes `warn`),
/// `info` otherwise.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(filter)
        .init();
}

/// Attach on the runtime, blocking this thread on the result: the attach
/// future runs on a worker while `main`'s thread waits, so no nested runtime
/// is created.
fn attach_blocking(rt: &Handle, target: AttachTarget) -> anyhow::Result<Arc<WireNamespace>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let rt_for_task = rt.clone();
    rt.spawn(async move {
        let _ = tx.send(WireNamespace::attach(target, rt_for_task).await);
    });
    rx.recv()
        .map_err(|_| anyhow::anyhow!("attach task dropped before completing"))?
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Log every reattach (a reconnect that lands on a restarted daemon), keep the
/// wire attach-event broadcast drained, and forward each reattach as an engine
/// [`NsAttachEvent`] the NFS renderer acts on. Returns the sender the renderer
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
            let AttachEvent::Reattached {
                old_instance,
                new_instance,
            } = event;
            warn!(
                %old_instance, %new_instance,
                "daemon restarted under the frontend; re-resolving cached node ids"
            );
            // A closed channel means the renderer has exited; the log above is
            // still useful.
            let _ = forward.send(NsAttachEvent::Reattached);
        }
    });
    tx
}

/// On `SIGTERM`/`SIGINT`, unmount the mount point so the blocking renderer
/// loop unblocks and the runner exits. Mirrors the daemon's signal handling.
#[cfg(unix)]
fn install_signal_handler(rt: &Handle, mount_point: PathBuf) {
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
        if let Err(error) = omnifs_nfs::unmount(&mount_point) {
            warn!(%error, "frontend self-unmount failed");
        }
    });
}

#[cfg(not(unix))]
fn install_signal_handler(_rt: &Handle, _mount_point: PathBuf) {}
