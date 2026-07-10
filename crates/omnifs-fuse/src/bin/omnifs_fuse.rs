//! `omnifs-fuse`: the out-of-process FUSE frontend runner.
//!
//! Attaches a wire-backed namespace to a host-native daemon's shared
//! namespace (a Unix socket for the Docker-hosted frontend, or vsock for the
//! krunkit guest) and serves a FUSE mount over it, blocking until unmount.
//! This is the whole content of the Docker frontend image's `ENTRYPOINT` and
//! the krunkit guest's `omnifs-frontend.service`: it runs no provider and
//! needs no engine, so it ships without Wasmtime, the provider bundle, or any
//! of the daemon's control-plane surface (contrast the full `omnifs` binary:
//! CLI + daemon + engine).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use omnifs_engine::Namespace;
use omnifs_namespace_wire::{
    AttachEvent, AttachTarget, WireNamespace, resolve_attach_target, resolve_ready_vsock_port,
};
use tokio::runtime::Handle;
use tracing::{info, warn};

/// Attach a wire-backed namespace and serve a FUSE mount over it.
#[derive(Parser, Debug)]
#[command(
    name = "omnifs-fuse",
    version,
    about = "The out-of-process omnifs FUSE frontend runner"
)]
struct Args {
    /// Host-visible mount point to serve the projected tree at.
    #[arg(long)]
    mount_point: PathBuf,
    /// Path to the daemon's namespace attach socket to connect to. Mutually
    /// exclusive with the TCP/vsock attach path: when absent,
    /// `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN` in the environment select it
    /// instead (the Docker-hosted and krunkit frontends' only option, since
    /// neither can share a host Unix socket into its guest).
    #[arg(long)]
    attach: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();

    // Parsed (and platform-checked) before the attach dial, so a
    // misconfigured seed fails fast rather than after a 30s connect attempt.
    let ready_port =
        resolve_ready_vsock_port().context("resolve the readiness-beacon vsock port")?;
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

    spawn_reattach_logger(&handle, &namespace);

    #[cfg(target_os = "linux")]
    if let Some(port) = ready_port {
        omnifs_namespace_wire::spawn_ready_signal(&handle, args.mount_point.clone(), port);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = ready_port;

    install_signal_handler(&handle, args.mount_point.clone());

    let namespace_dyn = Arc::clone(&namespace) as Arc<dyn Namespace>;
    let notifier = omnifs_fuse::new_notifier_handle();
    omnifs_fuse::mount::run_blocking(&args.mount_point, namespace_dyn, &handle, &notifier)
        .context("serve the FUSE mount")?;

    info!(mount = %args.mount_point.display(), "frontend exited");
    Ok(())
}

/// Foreground logging: `RUST_LOG` if set, `info` otherwise. Mirrors the CLI's
/// own `init_tracing`, minus the `-v`/`-vv` verbosity flags this
/// single-purpose runner has no use for.
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

/// Log every reattach (a reconnect that lands on a restarted daemon). FUSE's
/// inode table is in-memory, per-mount kernel state a daemon restart cannot
/// invalidate out from under it the way NFS's on-disk filehandle table can
/// (see `docs/architecture/50-nfs-frontend.md`), so there is nothing to
/// re-resolve here; this is observability only.
fn spawn_reattach_logger(rt: &Handle, namespace: &Arc<WireNamespace>) {
    let mut receiver = namespace.subscribe_attach_events();
    rt.spawn(async move {
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
    });
}

/// On `SIGTERM`/`SIGINT`, unmount the mount point so the blocking FUSE loop
/// unblocks and the runner exits. Mirrors the daemon's own signal handling.
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
        if let Err(error) = omnifs_fuse::mount::unmount(&mount_point) {
            warn!(%error, "frontend self-unmount failed");
        }
    });
}

#[cfg(not(unix))]
fn install_signal_handler(_rt: &Handle, _mount_point: PathBuf) {}
