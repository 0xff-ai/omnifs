//! Apply mount changes to a running daemon.
//!
//! `omnifs init` and `omnifs mounts rm` call these after updating host
//! config so changes take effect without a container restart. Both degrade
//! gracefully: no running daemon means config-only, exactly as before.

use crate::client::DaemonClient;
use crate::launch::DockerMountMaterializer;
use crate::launch_backend::LaunchBackend;
use crate::session::MountConfig;
use omnifs_creds::CredentialStore;
use omnifs_provider::Catalog;

pub(crate) enum LiveApply {
    /// No daemon answered on the control port; the change is config-only.
    NotRunning,
    /// The change is live on the running daemon.
    Applied,
    /// A daemon is running, but this change needs container-level changes
    /// that cannot be applied to a running container.
    RestartRequired(&'static str),
}

/// Reconcile the running daemon after the caller has written the new mount's
/// spec file. The daemon loads it from `mounts/` itself; no spec crosses the
/// wire. On Docker a mount that introduces new host preopen binds cannot be
/// added to a running container, so it is reported as restart-required and the
/// reconcile is left for the next `omnifs up`.
pub(crate) async fn add_mount(
    client: &DaemonClient,
    catalog: &Catalog,
    store: &dyn CredentialStore,
    config: MountConfig,
    backend: &LaunchBackend,
) -> anyhow::Result<LiveApply> {
    if client.compatible_status_optional().await?.is_none() {
        return Ok(LiveApply::NotRunning);
    }
    if backend.is_docker() {
        let mount = DockerMountMaterializer::new(catalog, store).materialize(&config)?;
        if !mount.preopen_binds().is_empty() {
            return Ok(LiveApply::RestartRequired("it needs new host binds"));
        }
    }
    let report = client.reconcile().await?;
    if let Some(failure) = report
        .failed
        .iter()
        .find(|failure| failure.mount == config.name.as_str())
    {
        anyhow::bail!("mount `{}` did not load: {}", config.name, failure.reason);
    }
    Ok(LiveApply::Applied)
}
