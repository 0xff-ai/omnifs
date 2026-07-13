//! FUSE runner command for `omnifs-thin`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Args as ClapArgs;
use omnifs_engine::Namespace;
use omnifs_mtab::StateFile;
use omnifs_vfs_wire::{
    AttachEvent, AttachTarget, FrontendIdentity, FrontendKind, WireNamespace,
    resolve_ready_vsock_port,
};
use tokio::runtime::Handle;
use tracing::{info, warn};

/// Arguments for the Linux FUSE frontend.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Host-visible mount point to serve the projected tree at.
    #[arg(long)]
    mount_point: PathBuf,
    /// Directory for local-process mount discovery. Omit for guest/container
    /// delivery, whose runtime owns process discovery and teardown.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Path to the daemon's namespace attach socket to connect to. When
    /// absent, the attach target is resolved from the environment.
    #[arg(long)]
    attach: Option<PathBuf>,
}

pub(crate) fn run(args: Args) -> anyhow::Result<()> {
    crate::init_tracing();

    // Parsed (and platform-checked) before the attach dial, so a
    // misconfigured seed fails fast rather than after a 30s connect attempt.
    let ready_port =
        resolve_ready_vsock_port().context("resolve the readiness-beacon vsock port")?;
    let target =
        AttachTarget::resolve(args.attach).context("resolve the namespace attach target")?;
    let target_label = target.to_string();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the tokio runtime")?;
    let handle = rt.handle().clone();

    let identity = FrontendIdentity {
        kind: FrontendKind::Fuse,
        mount_point: args.mount_point.clone(),
    };
    let namespace = rt
        .block_on(WireNamespace::attach(target, identity, handle.clone()))
        .context("attach to the namespace")?;
    info!(
        target = %target_label,
        instance = %namespace.instance_id(),
        "attached to namespace"
    );

    spawn_reattach_logger(&handle, &namespace);

    if let Some(port) = ready_port {
        omnifs_vfs_wire::spawn_ready_signal(&handle, args.mount_point.clone(), port);
    }

    install_signal_handler(&handle, args.mount_point.clone());

    let namespace_dyn = Arc::clone(&namespace) as Arc<dyn Namespace>;
    let notifier = omnifs_fuse::new_notifier_handle();
    let _state_file = args
        .state_dir
        .as_deref()
        .map(|state_dir| StateFile::write_fuse(&args.mount_point, state_dir))
        .transpose()
        .context("write FUSE mount discovery state")?;
    omnifs_fuse::mount::run_blocking(&args.mount_point, namespace_dyn, &handle, &notifier)
        .context("serve the FUSE mount")?;

    info!(mount = %args.mount_point.display(), "frontend exited");
    Ok(())
}

/// Log every reattach. FUSE's inode table is in-memory, per-mount kernel state
/// a daemon restart cannot invalidate out from under it, so there is nothing
/// to re-resolve here; this is observability only.
fn spawn_reattach_logger(rt: &Handle, namespace: &Arc<WireNamespace>) {
    let mut receiver = namespace.subscribe_attach_events();
    drop(rt.spawn(async move {
        while let Ok(event) = receiver.recv().await {
            let AttachEvent::Reattached {
                old_instance,
                new_instance,
            } = event;
            warn!(
                %old_instance, %new_instance,
                "daemon restarted under the frontend; reconnected"
            );
        }
    }));
}

/// On `SIGTERM`/`SIGINT`, unmount the mount point so the blocking FUSE loop
/// unblocks and the runner exits.
#[cfg(unix)]
fn install_signal_handler(rt: &Handle, mount_point: PathBuf) {
    use tokio::signal::unix::{SignalKind, signal};

    drop(rt.spawn(async move {
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
        if let Err(error) = omnifs_fuse::mount::unmount(&mount_point) {
            warn!(%error, "frontend self-unmount failed");
        }
    }));
}

#[cfg(not(unix))]
fn install_signal_handler(_rt: &Handle, _mount_point: PathBuf) {}
