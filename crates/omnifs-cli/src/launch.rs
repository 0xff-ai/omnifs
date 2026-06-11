//! Shared session launch choreography for `omnifs up` and `omnifs dev`.

use std::path::Path;

use anyhow::Context as _;
use omnifs_creds::CredentialStore;

use crate::catalog::ProviderCatalog;
use crate::client::DaemonClient;
use crate::runtime::{ContainerExtras, Runtime};
use crate::runtime_target::RuntimeTarget;
use crate::session::{MountConfig, Session};

/// Everything a caller must supply to run the full launch sequence.
pub(crate) struct LaunchSpec<'a> {
    pub runtime: &'a RuntimeTarget,
    pub runtime_home: &'a Path,
    pub credentials_file: &'a Path,
    pub store: Box<dyn CredentialStore>,
    /// Command name shown in Docker-readiness diagnostics, e.g. `"omnifs up"`.
    pub verb: &'static str,
    /// Mount configs to materialize and push to the daemon.
    pub configs: Vec<MountConfig>,
    /// Extra binds layered on top of the session-populated preopens.
    pub extras: ContainerExtras,
}

/// Run the full prepare → populate → connect → launch → wait → push
/// sequence.
///
/// The `cleanup` guard is armed on entry and disarmed only after every
/// mount is loaded, so the session directory is always removed on error —
/// and once the container is running, a failure also tears the container
/// down so it never outlives the session files its binds point at.
pub(crate) async fn launch_session(
    spec: LaunchSpec<'_>,
    catalog: &ProviderCatalog,
) -> anyhow::Result<()> {
    let LaunchSpec {
        runtime,
        runtime_home,
        credentials_file,
        store,
        verb,
        configs,
        mut extras,
    } = spec;

    let session = Session::prepare(runtime.container_name(), credentials_file)?;
    let mut cleanup = session.cleanup_on_drop();
    anstream::println!("Preparing session at {}", session.root().display());
    std::fs::create_dir_all(runtime_home)
        .with_context(|| format!("create runtime home {}", runtime_home.display()))?;

    anstream::println!("Materializing mount configs and credentials");
    let (preopen_binds, payloads) = session.populate(&configs, catalog, store.as_ref())?;
    anstream::println!("✓ Materialized {} mount(s)", configs.len());

    // Session-populated preopen binds come first; caller extras append after.
    let mut binds = preopen_binds;
    binds.append(&mut extras.binds);
    extras.binds = binds;

    let rt = Runtime::connect_ready(runtime, verb).await?;
    rt.launch_container(runtime_home, &session, extras).await?;

    if let Err(error) = push_mounts(&rt, &payloads).await {
        // The container's creds bind points into the session dir the armed
        // cleanup is about to delete; don't leave it running half-mounted.
        if let Err(teardown) = rt.remove().await {
            anstream::eprintln!("also failed to remove the container: {teardown:#}");
        }
        return Err(error);
    }
    cleanup.disarm();

    Ok(())
}

async fn push_mounts(
    rt: &Runtime,
    payloads: &[crate::session::MountPayload],
) -> anyhow::Result<()> {
    rt.wait_for_daemon_ready().await?;

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
