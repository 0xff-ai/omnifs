//! `omnifs frontend down`: tear down the optional virtualized FUSE frontend,
//! whichever backend it was launched with (Docker container or krunkit
//! microVM).
//!
//! Neither backend's attach listener has a close route on the daemon side
//! (`POST /v1/attach-listeners`/`/v1/attach-listeners/vsock` only ever bind,
//! idempotently): the listener stays bound until the daemon itself restarts.
//! This command says so rather than implying it closed something it did not.
//!
//! [`teardown`] is shared with `omnifs down`, which tears down a running
//! frontend before stopping the daemon.

use clap::Args;
use omnifs_workspace::layout::{OMNIFS_HOME_ENV, WorkspaceLayout};
use omnifs_workspace::runtime_record::{RuntimeRecord, Via};

use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
use crate::krunkit_backend::{KrunkitBackend, UNUSED_GUEST_IMAGE_PLACEHOLDER};
use crate::launch_backend::DockerTarget;
use crate::runtime::Runtime;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendDownArgs {}

impl FrontendDownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let found = teardown(workspace.layout()).await?;
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

/// Remove the virtualized frontend for `paths`'s workspace, if one exists,
/// dispatching to whichever backend the runtime record names, and clear its
/// record entry. Returns whether a frontend was found. An unreachable backend
/// (Docker daemon down, or a leftover pidfile) is a warning, not an error:
/// this workspace may simply have no frontend attached.
pub(crate) async fn teardown(paths: &WorkspaceLayout) -> anyhow::Result<bool> {
    let recorded_via = RuntimeRecord::read(&paths.runtime_record_file())
        .ok()
        .flatten()
        .and_then(|record| record.frontends.iter().find_map(|frontend| frontend.via));

    if recorded_via == Some(Via::Krunkit) {
        let backend = KrunkitBackend::new(
            paths.config_dir.clone(),
            UNUSED_GUEST_IMAGE_PLACEHOLDER.into(),
        );
        let running = backend.is_running().await?;
        if running.is_some() {
            backend.tear_down().await?;
            anstream::println!("✓ krunkit frontend removed");
        }
        clear_frontend_record(&paths.runtime_record_file());
        return Ok(running.is_some());
    }

    let is_default_home = std::env::var_os(OMNIFS_HOME_ENV).is_none();
    let container_name = frontend_container_name(&paths.config_dir, is_default_home)?;

    // The image field is unused by removal; it only needs to be a valid
    // reference, so the dev placeholder is fine regardless of build channel.
    let target = DockerTarget::new(
        container_name.as_str().to_string(),
        FRONTEND_DEV_IMAGE.to_string(),
    )?;
    let found = match Runtime::connect_for(&target) {
        Ok(runtime) => {
            let backend = DockerBackend::new(runtime);
            let running = backend.is_running().await?;
            if running.is_some() {
                backend.tear_down().await?;
                anstream::println!("✓ Frontend container `{container_name}` removed");
            }
            running.is_some()
        },
        Err(error) => {
            anstream::eprintln!(
                "⚠  Docker not reachable; could not check for frontend container `{container_name}`: {error}"
            );
            false
        },
    };

    clear_frontend_record(&paths.runtime_record_file());
    Ok(found)
}

/// Drop the virtualized frontend's entry from the on-disk runtime record (a
/// read-modify-write, mirroring `frontend up`'s append). Best-effort: a
/// missing or unreadable record is not an error, since the daemon may already
/// be down. Drops any virtualized entry regardless of which backend
/// delivered it, mirroring `frontend up`'s "at most one" invariant.
fn clear_frontend_record(record_path: &std::path::Path) {
    let Ok(Some(mut record)) = RuntimeRecord::read(record_path) else {
        return;
    };
    let before = record.frontends.len();
    record.frontends.retain(|frontend| frontend.via.is_none());
    if record.frontends.len() != before
        && let Err(error) = record.write(record_path)
    {
        anstream::eprintln!("warning: could not update the runtime record: {error:#}");
    }
}
