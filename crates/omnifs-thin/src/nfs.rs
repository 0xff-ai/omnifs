//! NFS runner command for `omnifs-thin`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::host_control::RunnerPhase;
use crate::lifecycle::{Lifecycle, LifecycleConfig, coordinate_mount, preflight};
use anyhow::Context as _;
use omnifs_vfs::Namespace;
use omnifs_vfs::{AttachTarget, WireNamespace, resolve_ready_vsock_port};
use tracing::info;

pub(crate) fn run(args: crate::RunnerArgs) -> anyhow::Result<()> {
    crate::init_tracing();
    let crate::RunnerArgs {
        attachment,
        spec,
        runtime_instance,
        state_dir,
        attach,
        port,
        host_control,
    } = args;
    let state_dir = state_dir.context("--state-dir is required with --protocol nfs")?;
    let mount_point = spec.location().to_path_buf();
    let ready_port =
        resolve_ready_vsock_port().context("resolve the readiness-beacon vsock port")?;
    let target = AttachTarget::resolve(attach).context("resolve the VFS attach target")?;
    let target_label = target.to_string();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the tokio runtime")?;
    let handle = runtime.handle().clone();
    let runner_control = host_control.into_config()?;
    let mut lifecycle = {
        let _runtime_guard = runtime.enter();
        Lifecycle::prepare(LifecycleConfig {
            attachment: &attachment,
            spec: &spec,
            state_dir: Some(&state_dir),
            runner_control,
        })?
    };
    preflight(&spec, Some(&state_dir)).context("check the NFS mount location")?;
    lifecycle.phase.send_replace(RunnerPhase::Attaching);
    let namespace = runtime
        .block_on(WireNamespace::attach_with_teardown(
            target,
            attachment,
            spec.clone(),
            runtime_instance,
            handle.clone(),
            lifecycle.wire_teardown_tx.clone(),
        ))
        .context("attach to the namespace")?;
    info!(
        target = %target_label,
        "attached to namespace"
    );

    #[cfg(target_os = "linux")]
    if let Some(port) = ready_port {
        omnifs_vfs::spawn_ready_signal(&handle, mount_point.clone(), port);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = ready_port;
    let mut options = omnifs_nfs::NfsMountOptions::loopback(state_dir);
    options.persist_filehandles = true;
    options.bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let cancelled = Arc::clone(&lifecycle.cancelled);
    let mount_point_owned = mount_point.clone();
    let namespace_dyn = Arc::clone(&namespace) as Arc<dyn Namespace>;
    let (mount_done_tx, mount_done_rx) = tokio::sync::oneshot::channel();
    lifecycle.phase.send_replace(RunnerPhase::Mounting);
    let mount_thread = std::thread::Builder::new()
        .name("omnifs-nfs-mount".to_owned())
        .spawn(move || {
            let result = omnifs_nfs::mount_blocking_cancellable(
                &mount_point_owned,
                namespace_dyn,
                handle,
                &options,
                &cancelled,
            )
            .context("serve the NFS mount");
            let _ = mount_done_tx.send(result);
        })
        .context("start the NFS mount owner")?;
    let result = runtime.block_on(coordinate_mount(&spec, &mut lifecycle, mount_done_rx));
    mount_thread
        .join()
        .map_err(|_| anyhow::anyhow!("NFS mount owner panicked"))?;
    result?;

    info!(mount = %mount_point.display(), "filesystem exited");
    Ok(())
}
