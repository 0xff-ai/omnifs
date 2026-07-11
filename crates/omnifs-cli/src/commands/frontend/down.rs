//! `omnifs frontend down`: tear down the optional virtualized FUSE frontend,
//! whichever backend it was launched with (Docker container or krunkit
//! microVM).
//!
//! Neither backend's attach listener has a close route on the daemon side
//! (`POST /v1/frontend/attach-target`/`/v1/frontend/attach-target/vsock` only
//! ever bind, idempotently): the listener stays bound until the daemon itself
//! restarts. This command says so rather than implying it closed something it
//! did not.
//!
//! [`teardown`] is shared with `omnifs down`, which tears down a running
//! frontend before stopping the daemon.

use clap::Args;
use omnifs_workspace::layout::WorkspaceLayout;

use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
use crate::krunkit_backend::KrunkitBackend;
use crate::launch_backend::DockerTarget;
use crate::runtime::Runtime;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendDownArgs {}

impl FrontendDownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let found = teardown(workspace.layout(), false).await?;
        if found {
            anstream::eprintln!(
                "note: the daemon's namespace attach listener is not closed by this command; \
                 it stays bound until the daemon restarts"
            );
        } else {
            anstream::println!("No frontend found.");
        }
        Ok(())
    }
}

/// Remove every frontend discoverable for this workspace.
pub(crate) async fn teardown(paths: &WorkspaceLayout, force: bool) -> anyhow::Result<bool> {
    let mut failures = Vec::new();
    let krunkit = KrunkitBackend::new(paths.config_dir.clone());
    let krunkit_found = match krunkit.is_running().await {
        Ok(Some(_)) => match krunkit.tear_down().await {
            Ok(()) => {
                anstream::println!("✓ krunkit frontend removed");
                true
            },
            Err(error) => {
                failures.push(format!("remove krunkit frontend: {error:#}"));
                false
            },
        },
        Ok(None) => false,
        Err(error) => {
            failures.push(format!("inspect krunkit frontend: {error:#}"));
            false
        },
    };

    let found = match teardown_docker(paths).await {
        Ok(found) => found,
        Err(error) => {
            failures.push(format!("inspect or remove Docker frontend: {error:#}"));
            false
        },
    };

    #[cfg(all(feature = "daemon", not(target_os = "linux")))]
    let nfs_found = {
        let summary =
            crate::host_teardown::teardown_host_native_nfs(&paths.nfs_state_dir(), force)?;
        if summary.unmounted > 0 {
            anstream::println!("✓ Unmounted {} local NFS frontend(s)", summary.unmounted);
        }
        if summary.swept_orphans > 0 {
            anstream::println!(
                "✓ Swept {} orphaned NFS mount-state file(s)",
                summary.swept_orphans
            );
        }
        if !summary.failed.is_empty() {
            failures.push(format!(
                "{} NFS frontend(s) could not be safely unmounted",
                summary.failed.len()
            ));
        }
        if summary.skipped > 0 {
            failures.push(format!(
                "{} NFS mount-state file(s) could not be read",
                summary.skipped
            ));
        }
        summary.unmounted > 0 || summary.swept_orphans > 0
    };
    #[cfg(not(all(feature = "daemon", not(target_os = "linux"))))]
    let nfs_found = false;

    if !failures.is_empty() {
        anyhow::bail!("frontend teardown incomplete: {}", failures.join("; "));
    }

    Ok(krunkit_found || found || nfs_found)
}

async fn teardown_docker(paths: &WorkspaceLayout) -> anyhow::Result<bool> {
    let container_name = frontend_container_name(paths)?;

    // The image field is unused by removal; it only needs to be a valid
    // reference, so the dev placeholder is fine regardless of build channel.
    let target = DockerTarget::new(
        container_name.as_str().to_string(),
        FRONTEND_DEV_IMAGE.to_string(),
    )?;
    let runtime = match Runtime::connect_for(&target) {
        Ok(runtime) => runtime,
        Err(error) => {
            anstream::eprintln!(
                "⚠  Docker not reachable; could not check for frontend container `{container_name}`: {error}"
            );
            return Ok(false);
        },
    };
    let Some(discovered) = runtime
        .frontend_container_for_home(&paths.config_dir)
        .await?
    else {
        return Ok(false);
    };
    let discovered_target = DockerTarget::new(
        discovered.as_str().to_string(),
        FRONTEND_DEV_IMAGE.to_string(),
    )?;
    let backend = DockerBackend::new(Runtime::connect_for(&discovered_target)?);
    let running = backend.is_running().await?.is_some();
    if running {
        backend.tear_down().await?;
        anstream::println!("✓ Frontend container `{discovered}` removed");
    }
    Ok(running)
}
