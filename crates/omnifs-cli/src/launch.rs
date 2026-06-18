//! Shared launch choreography for `omnifs up` and `omnifs dev`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use omnifs_creds::CredentialStore;

use crate::catalog::ProviderCatalog;
use crate::client::DaemonClient;
use crate::host_launch::HostDaemon;
use crate::runtime::{ContainerExtras, Runtime};
use crate::runtime_target::RuntimeTarget;
use crate::session::MountConfig;

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
    /// Run the daemon host-native (spawn `omnifs daemon`) instead of
    /// launching a Docker container.
    pub host_native: bool,
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
        cache_dir,
    } = spec;

    std::fs::create_dir_all(runtime_home)
        .with_context(|| format!("create runtime home {}", runtime_home.display()))?;

    // Host-native: the daemon loads and materializes mounts from `mounts/`
    // itself, so the CLI only spawns it and triggers a reconcile. No CLI-side
    // materialize-and-push on this path.
    if host_native {
        return launch_host_native(runtime_home, &cache_dir).await;
    }

    anstream::println!("Computing container binds for {} mount(s)", configs.len());
    let mut preopen_binds = Vec::new();
    for config in &configs {
        // Docker needs preopen binds present before `docker create`; the spec
        // itself is not pushed, only read to derive the binds.
        let binds = config.materialize(catalog, store.as_ref(), host_native)?;
        preopen_binds.extend(binds);
    }

    // Materialized preopen binds come first; caller extras append after.
    let mut binds = preopen_binds;
    binds.append(&mut extras.binds);
    extras.binds = binds;

    let rt = Runtime::connect_ready(runtime, verb).await?;
    rt.launch_container(runtime_home, extras).await?;

    if let Err(error) = finish_docker_launch(&rt).await {
        if let Err(teardown) = rt.remove().await {
            anstream::eprintln!("also failed to remove the container: {teardown:#}");
        }
        return Err(error);
    }

    Ok(())
}

/// Spawn a detached host-native daemon and wait for it to serve. The daemon
/// reconciles `mounts/` on start; the CLI triggers one more reconcile to
/// converge any change since and to surface per-mount failures. On failure
/// before detach the spawned daemon is terminated.
async fn launch_host_native(runtime_home: &Path, cache_dir: &Path) -> anyhow::Result<()> {
    anstream::println!("Starting omnifs daemon (host-native)");
    let mut daemon = HostDaemon::spawn(runtime_home, cache_dir)?;
    daemon.wait_ready().await?;

    let client = DaemonClient::new();
    match client.reconcile().await {
        Ok(report) => report_reconcile_failures(&report),
        Err(error) => {
            daemon.kill().await;
            return Err(error);
        },
    }
    // The daemon owns the mount point; read it back for the user-facing message.
    if let Ok(status) = client.status().await {
        anstream::println!("✓ Mount is serving at {}", status.mount_point.display());
        anstream::println!("✓ Runtime sees {} provider(s)", status.mounts.len());
    }
    daemon.detach();
    Ok(())
}

/// Print any mounts that failed to converge during reconcile as warnings; a
/// failed mount does not abort the launch, since the rest are serving.
fn report_reconcile_failures(report: &omnifs_api::ReconcileReport) {
    for failure in &report.failed {
        anstream::eprintln!(
            "warning: mount `{}` did not load: {}",
            failure.mount,
            failure.reason
        );
    }
}

/// Docker path: wait for the in-container daemon to serve (it reconciles from
/// `mounts/` on start), then converge once more over the control API to surface
/// any per-mount failure. No spec crosses the wire.
async fn finish_docker_launch(rt: &Runtime) -> anyhow::Result<()> {
    rt.wait_for_daemon_ready().await?;
    let client = DaemonClient::new();
    client.require_compatible().await?;
    let report = client.reconcile().await?;
    report_reconcile_failures(&report);
    if let Ok(status) = client.status().await {
        anstream::println!("✓ Runtime sees {} provider(s)", status.mounts.len());
    }
    Ok(())
}
