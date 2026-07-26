//! FUSE runner command for `omnifs-thin`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::host_control::RunnerPhase;
use crate::lifecycle::{Lifecycle, LifecycleConfig, coordinate_mount, preflight};
use anyhow::Context as _;
use clap::Args as ClapArgs;
use omnifs_core::FrontendRuntime;
use omnifs_mtab::StateFile;
use omnifs_vfs::Namespace;
use omnifs_vfs::{AttachTarget, FrontendIdentity, FsType, WireNamespace, resolve_ready_vsock_port};
use tracing::info;

/// Arguments for the Linux FUSE frontend.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Runtime identity supplied by the launcher.
    #[arg(long, env = "OMNIFS_RUNTIME")]
    runtime: FrontendRuntime,
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
    #[command(flatten)]
    host_control: crate::HostControlArgs,
}

pub(crate) fn run(args: Args) -> anyhow::Result<()> {
    crate::init_tracing();
    let Args {
        runtime,
        mount_point,
        state_dir,
        attach,
        host_control,
    } = args;

    // Parsed (and platform-checked) before the attach dial, so a
    // misconfigured seed fails fast rather than after a 30s connect attempt.
    let ready_port =
        resolve_ready_vsock_port().context("resolve the readiness-beacon vsock port")?;
    let target = AttachTarget::resolve(attach).context("resolve the namespace attach target")?;
    let target_label = target.to_string();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the tokio runtime")?;
    let handle = rt.handle().clone();
    let runner_control = host_control.into_config()?;
    let mut lifecycle = {
        let _runtime_guard = rt.enter();
        Lifecycle::prepare(LifecycleConfig {
            runtime,
            filesystem: FsType::Fuse,
            mount_point: &mount_point,
            state_dir: state_dir.as_deref(),
            runner_control,
        })?
    };
    preflight(FsType::Fuse, &mount_point, state_dir.as_deref())
        .context("check the FUSE mount location")?;
    lifecycle.phase.send_replace(RunnerPhase::Attaching);

    let identity = FrontendIdentity {
        runtime,
        kind: FsType::Fuse,
        mount_point: mount_point.clone(),
    };
    let namespace = rt
        .block_on(WireNamespace::attach_with_teardown(
            target,
            identity,
            handle.clone(),
            lifecycle.wire_teardown_tx.clone(),
        ))
        .context("attach to the namespace")?;
    info!(
        target = %target_label,
        "attached to namespace"
    );

    if let Some(port) = ready_port {
        omnifs_vfs::spawn_ready_signal(&handle, mount_point.clone(), port);
    }

    let namespace_dyn = Arc::clone(&namespace) as Arc<dyn Namespace>;
    let notifier = omnifs_fuse::new_notifier_handle();
    let cancelled = Arc::clone(&lifecycle.cancelled);
    let mount_point_owned = mount_point.clone();
    let mount_handle = handle.clone();
    let (mount_done_tx, mount_done_rx) = tokio::sync::oneshot::channel();
    lifecycle.phase.send_replace(RunnerPhase::Mounting);
    let mount_thread = std::thread::Builder::new()
        .name("omnifs-fuse-mount".to_owned())
        .spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let _state_file = state_dir
                    .as_deref()
                    .map(|dir| StateFile::write_fuse(&mount_point_owned, dir))
                    .transpose()
                    .context("write FUSE mount discovery state")?;
                omnifs_fuse::mount::run_blocking_cancellable(
                    &mount_point_owned,
                    namespace_dyn,
                    &mount_handle,
                    &notifier,
                    &cancelled,
                )
                .context("serve the FUSE mount")
            })();
            let _ = mount_done_tx.send(result);
        })
        .context("start the FUSE mount owner")?;
    let result = rt.block_on(coordinate_mount(
        FsType::Fuse,
        mount_point.clone(),
        &mut lifecycle,
        mount_done_rx,
    ));
    mount_thread
        .join()
        .map_err(|_| anyhow::anyhow!("FUSE mount owner panicked"))?;
    result?;

    info!(mount = %mount_point.display(), "frontend exited");
    Ok(())
}
