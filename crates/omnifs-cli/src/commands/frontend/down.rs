//! `omnifs frontend down`: tear down the Docker-hosted FUSE frontend
//! container.
//!
//! The daemon's TCP attach listener has no close route (`POST
//! /v1/attach-listeners` only ever binds, idempotently): the listener stays
//! bound until the daemon itself restarts. This command says so rather than
//! implying it closed something it did not.
//!
//! [`teardown`] is shared with `omnifs down`, which tears down a running
//! frontend container before stopping the daemon.

use clap::Args;
use omnifs_workspace::layout::{OMNIFS_HOME_ENV, WorkspaceLayout};
use omnifs_workspace::runtime_record::{RuntimeRecord, Via};

use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
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
                "note: the daemon's TCP namespace attach listener is not closed by this command; \
                 it stays bound until the daemon restarts"
            );
        } else {
            anstream::println!("No frontend container found.");
        }
        Ok(())
    }
}

/// Remove the Docker-hosted frontend container for `paths`'s workspace, if
/// one exists, and clear its record entry. Returns whether a container was
/// found. Docker being unreachable is a warning, not an error: the container
/// (and this workspace) may simply have no Docker frontend attached.
pub(crate) async fn teardown(paths: &WorkspaceLayout) -> anyhow::Result<bool> {
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
            let running = runtime.container_running(&container_name).await?;
            if running.is_some() {
                runtime.remove_existing(&container_name).await?;
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

/// Drop the Docker-hosted frontend's entry from the on-disk runtime record
/// (a read-modify-write, mirroring `frontend up`'s append). Best-effort: a
/// missing or unreadable record is not an error, since the daemon may already
/// be down.
fn clear_frontend_record(record_path: &std::path::Path) {
    let Ok(Some(mut record)) = RuntimeRecord::read(record_path) else {
        return;
    };
    let before = record.frontends.len();
    record
        .frontends
        .retain(|frontend| frontend.via != Some(Via::Docker));
    if record.frontends.len() != before
        && let Err(error) = record.write(record_path)
    {
        anstream::eprintln!("warning: could not update the runtime record: {error:#}");
    }
}
