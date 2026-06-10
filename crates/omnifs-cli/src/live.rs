//! Apply mount changes to a running daemon.
//!
//! `omnifs init` and `omnifs mounts rm` call these after updating host
//! config so changes take effect without a container restart. Both degrade
//! gracefully: no running daemon means config-only, exactly as before.

use crate::catalog::ProviderCatalog;
use crate::client::{DaemonClient, DaemonProbe};
use crate::container_name::ContainerName;
use crate::session::{MountConfig, Session};
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

/// Materialize the new mount's credentials into the live session and load
/// it on the running daemon.
pub(crate) async fn add_mount(
    container_name: &ContainerName,
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
    config: MountConfig,
) -> anyhow::Result<LiveApply> {
    let client = DaemonClient::new();
    if matches!(client.probe().await?, DaemonProbe::Unreachable) {
        return Ok(LiveApply::NotRunning);
    }
    // The session secrets directory is bind-mounted into the running
    // container; files written here propagate immediately.
    let Some(session) = Session::attach(container_name) else {
        return Ok(LiveApply::NotRunning);
    };
    let (binds, payloads) = session.populate(std::slice::from_ref(&config), catalog, store)?;
    if !binds.is_empty() {
        // New host binds (user preopens) cannot be added to a running
        // container; the mount config is saved and `omnifs up` picks it up.
        return Ok(LiveApply::RestartRequired("it needs new host binds"));
    }
    let payload = payloads
        .first()
        .expect("populate returns one payload per config");
    if payload
        .spec
        .auth
        .iter()
        .any(omnifs_mount_schema::Auth::is_oauth)
    {
        // OAuth credentials live in the session `credentials.json`, which is
        // a single-file bind: it only exists in the container if it was
        // present at launch, and host-side atomic rewrites replace the inode
        // the bind pins. Only a relaunch rebinds it.
        return Ok(LiveApply::RestartRequired(
            "its OAuth credential must be bound at container start",
        ));
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
