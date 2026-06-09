//! Shared session launch choreography for `omnifs up` and `omnifs dev`.

use std::path::Path;

use omnifs_creds::CredentialStore;

use crate::catalog::ProviderCatalog;
use crate::runtime::{ContainerExtras, GUEST_INSPECTOR_PORT, Runtime};
use crate::runtime_target::RuntimeTarget;
use crate::session::{MountConfig, Session};

/// Everything a caller must supply to run the full launch sequence.
pub(crate) struct LaunchSpec<'a> {
    pub runtime: &'a RuntimeTarget,
    pub credentials_file: &'a Path,
    pub store: Box<dyn CredentialStore>,
    /// Command name shown in Docker-readiness diagnostics, e.g. `"omnifs up"`.
    pub verb: &'static str,
    /// Extra binds, env vars, and ports layered on top of the session-populated
    /// preopens. `launch_session` always exposes the inspector port.
    pub extras: ContainerExtras,
}

/// Run the full prepare → populate → connect → launch → wait → verify →
/// disarm sequence.
///
/// `make_configs` receives the freshly-prepared `Session` and returns the
/// mount configs to materialise. It runs after `Session::prepare` so that
/// dev-only config installation (writing configs into the session dir) can
/// happen before populate.
///
/// The `cleanup` guard is armed on entry and disarmed only after
/// `verify_status` succeeds, so the session directory is always removed on
/// error or early return.
pub(crate) async fn launch_session(
    spec: LaunchSpec<'_>,
    catalog: &ProviderCatalog,
    make_configs: impl FnOnce(&Session) -> anyhow::Result<Vec<MountConfig>>,
) -> anyhow::Result<()> {
    let LaunchSpec {
        runtime,
        credentials_file,
        store,
        verb,
        mut extras,
    } = spec;

    let session = Session::prepare(runtime.container_name(), credentials_file)?;
    let mut cleanup = session.cleanup_on_drop();
    anstream::println!("Preparing session at {}", session.root().display());

    let configs = make_configs(&session)?;

    anstream::println!("Materializing mount configs and credentials");
    let preopen_binds = session.populate(&configs, catalog, store.as_ref())?;
    anstream::println!("✓ Materialized {} mount(s)", configs.len());

    // Session-populated preopen binds come first; caller extras append after.
    let mut binds = preopen_binds;
    binds.append(&mut extras.binds);
    extras.binds = binds;

    // Inspector port is always exposed.
    if !extras.tcp_ports.contains(&GUEST_INSPECTOR_PORT) {
        extras.tcp_ports.push(GUEST_INSPECTOR_PORT);
    }

    let rt = Runtime::connect_ready(runtime, verb).await?;
    rt.launch_container(&session, extras).await?;
    rt.wait_for_fuse_mount().await?;
    rt.verify_status(&configs).await?;
    cleanup.disarm();

    Ok(())
}
