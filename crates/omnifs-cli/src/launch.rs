//! Shared launch choreography for `omnifs up` and `omnifs dev`.

use std::path::Path;

use anyhow::Context as _;
use omnifs_creds::CredentialStore;

use crate::catalog::ProviderCatalog;
use crate::client::DaemonClient;
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
    } = spec;

    std::fs::create_dir_all(runtime_home)
        .with_context(|| format!("create runtime home {}", runtime_home.display()))?;

    anstream::println!("Materializing mount configs");
    let mut preopen_binds = Vec::new();
    let mut payloads = Vec::new();
    for config in &configs {
        let (binds, payload) = config.materialize(catalog, store.as_ref())?;
        preopen_binds.extend(binds);
        payloads.push(payload);
    }
    anstream::println!("✓ Materialized {} mount(s)", configs.len());

    // Materialized preopen binds come first; caller extras append after.
    let mut binds = preopen_binds;
    binds.append(&mut extras.binds);
    extras.binds = binds;

    let rt = Runtime::connect_ready(runtime, verb).await?;
    rt.launch_container(runtime_home, extras).await?;

    if let Err(error) = push_mounts(&rt, &payloads).await {
        if let Err(teardown) = rt.remove().await {
            anstream::eprintln!("also failed to remove the container: {teardown:#}");
        }
        return Err(error);
    }

    Ok(())
}

async fn push_mounts(rt: &Runtime, payloads: &[MountPayload]) -> anyhow::Result<()> {
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
