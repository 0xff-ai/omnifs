//! Shared launch choreography for `omnifs up` and `omnifs dev`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use omnifs_creds::CredentialStore;

use crate::catalog::ProviderCatalog;
use crate::client::DaemonClient;
use crate::host_launch::HostDaemon;
use crate::runtime::{ContainerExtras, Runtime};
use crate::runtime_target::RuntimeTarget;
use crate::session::{MountConfig, MountPayload};

/// Everything a caller must supply to run the full launch sequence.
pub(crate) struct LaunchSpec<'a> {
    pub runtime: &'a RuntimeTarget,
    pub runtime_home: &'a Path,
    pub store: Box<dyn CredentialStore>,
    /// Command name shown in Docker-readiness diagnostics, e.g. `"omnifs up"`.
    pub verb: &'static str,
    /// Mount configs to materialize and push to the daemon.
    pub configs: Vec<MountConfig>,
    /// Extra binds layered on top of materialized preopens.
    pub extras: ContainerExtras,
    /// Run the daemon host-native (spawn `omnifs daemon` over NFS) instead of
    /// launching a Docker container.
    pub host_native: bool,
    /// Host directory the host-native daemon serves the mount at.
    pub mount_point: PathBuf,
    /// Cache directory passed to the host-native daemon (also holds its log).
    pub cache_dir: PathBuf,
}

/// Run the full materialize → connect → launch → wait → push sequence.
pub(crate) async fn launch_runtime(
    spec: LaunchSpec<'_>,
    catalog: &ProviderCatalog,
) -> anyhow::Result<()> {
    let LaunchSpec {
        runtime,
        runtime_home,
        store,
        verb,
        configs,
        mut extras,
        host_native,
        mount_point,
        cache_dir,
    } = spec;

    std::fs::create_dir_all(runtime_home)
        .with_context(|| format!("create runtime home {}", runtime_home.display()))?;

    anstream::println!("Materializing mount configs");
    let mut preopen_binds = Vec::new();
    let mut payloads = Vec::new();
    for config in &configs {
        let (binds, payload) = config.materialize(catalog, store.as_ref(), host_native)?;
        preopen_binds.extend(binds);
        payloads.push(payload);
    }
    anstream::println!("✓ Materialized {} mount(s)", configs.len());

    if host_native {
        anyhow::ensure!(
            preopen_binds.is_empty(),
            "host-native launch produced container binds; preopens should be opened directly"
        );
        return launch_host_native(runtime_home, &cache_dir, &mount_point, &payloads).await;
    }

    // Materialized preopen binds come first; caller extras append after.
    let mut binds = preopen_binds;
    binds.append(&mut extras.binds);
    extras.binds = binds;

    let rt = Runtime::connect_ready(runtime, verb).await?;
    rt.launch_container(runtime_home, extras).await?;

    if let Err(error) = push_mounts_docker(&rt, &payloads).await {
        if let Err(teardown) = rt.remove().await {
            anstream::eprintln!("also failed to remove the container: {teardown:#}");
        }
        return Err(error);
    }

    Ok(())
}

/// Spawn a detached host-native daemon over NFS, wait for it to serve, then
/// push the mounts. On failure the spawned daemon is terminated.
async fn launch_host_native(
    runtime_home: &Path,
    cache_dir: &Path,
    mount_point: &Path,
    payloads: &[MountPayload],
) -> anyhow::Result<()> {
    anstream::println!(
        "Starting omnifs daemon (host-native, NFS) at {}",
        mount_point.display()
    );
    let mut daemon = HostDaemon::spawn(runtime_home, cache_dir, mount_point)?;
    daemon.wait_ready().await?;
    anstream::println!("✓ Mount is serving at {}", mount_point.display());

    if let Err(error) = push_mounts(payloads).await {
        daemon.kill().await;
        return Err(error);
    }
    daemon.detach();
    Ok(())
}

/// Docker path: wait for the container's daemon to publish the mount, then push
/// the mounts over the shared control-API path.
async fn push_mounts_docker(rt: &Runtime, payloads: &[MountPayload]) -> anyhow::Result<()> {
    rt.wait_for_daemon_ready().await?;
    push_mounts(payloads).await
}

/// Verify control-API compatibility and load every mount on the running daemon.
/// Shared by the Docker and host-native paths; the caller ensures the daemon is
/// already serving.
async fn push_mounts(payloads: &[MountPayload]) -> anyhow::Result<()> {
    let client = DaemonClient::new();
    client.require_compatible().await?;
    anstream::println!("Loading {} mount(s) into the daemon", payloads.len());
    futures_util::future::try_join_all(payloads.iter().map(|payload| {
        let client = &client;
        async move {
            client
                .add_mount(&payload.spec)
                .await
                .with_context(|| format!("load mount `{}`", payload.name))
        }
    }))
    .await?;
    anstream::println!("✓ Runtime sees {} provider(s)", payloads.len());
    Ok(())
}
