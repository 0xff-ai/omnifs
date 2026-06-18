//! Apply mount changes to a running daemon.
//!
//! `omnifs init` and `omnifs mounts rm` call these after updating host
//! config so changes take effect without a container restart. Both degrade
//! gracefully: no running daemon means config-only, exactly as before.

use crate::catalog::ProviderCatalog;
use crate::client::{DaemonClient, DaemonProbe};
use crate::session::MountConfig;
use omnifs_creds::CredentialStore;

pub(crate) enum LiveApply {
    /// No daemon answered on the control port; the change is config-only.
    NotRunning,
    /// The change is live on the running daemon.
    Applied,
    /// A daemon is running, but this change needs container-level changes
    /// that cannot be applied to a running container.
    RestartRequired(&'static str),
}

/// Materialize the new mount and load it on the running daemon.
pub(crate) async fn add_mount(
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
    config: MountConfig,
) -> anyhow::Result<LiveApply> {
    let client = DaemonClient::new();
    if matches!(client.probe().await?, DaemonProbe::Unreachable) {
        return Ok(LiveApply::NotRunning);
    }
    let (binds, payload) = config.materialize(catalog, store, false)?;
    if !binds.is_empty() {
        // New host binds (user preopens) cannot be added to a running
        // container; the mount config is saved and `omnifs up` picks it up.
        return Ok(LiveApply::RestartRequired("it needs new host binds"));
    }
    client.add_mount(&payload.spec).await?;
    Ok(LiveApply::Applied)
}

/// Unload a mount from the running daemon. A mount that is configured but
/// not loaded is not an error.
pub(crate) async fn remove_mount(name: &str) -> anyhow::Result<LiveApply> {
    let client = DaemonClient::new();
    if matches!(client.probe().await?, DaemonProbe::Unreachable) {
        return Ok(LiveApply::NotRunning);
    }
    client.remove_mount(name).await?;
    Ok(LiveApply::Applied)
}
