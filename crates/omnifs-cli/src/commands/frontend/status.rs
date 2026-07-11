//! `omnifs frontend status`: the virtualized FUSE frontend's state and attach
//! health, whichever backend it was launched with.

use clap::Args;
#[cfg(feature = "daemon")]
use omnifs_mtab::MountState;
use omnifs_workspace::runtime_record::{AttachRecord, RuntimeRecord};

use crate::error::ExitCode;
use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
use crate::krunkit_backend::KrunkitBackend;
use crate::launch_backend::DockerTarget;
use crate::runtime::Runtime;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendStatusArgs {}

impl FrontendStatusArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let paths = workspace.layout().clone();

        let record = RuntimeRecord::read(&paths.runtime_record_file())
            .ok()
            .flatten();
        let mut degraded = false;
        #[cfg(feature = "daemon")]
        {
            let local_states = match MountState::read_all(&paths.nfs_state_dir()) {
                Ok(states) => states,
                Err(error) => {
                    anstream::eprintln!("⚠  Could not read local frontend records: {error}");
                    degraded = true;
                    Vec::new()
                },
            };
            for state in local_states {
                let kind = match &state.kind {
                    omnifs_mtab::MountKind::Fuse => "FUSE",
                    omnifs_mtab::MountKind::Nfs { .. } => "NFS",
                };
                if crate::host_teardown::local_mount_is_owned(&state) {
                    anstream::println!(
                        "local {kind} frontend at {}: running",
                        state.mount_point.display()
                    );
                } else {
                    anstream::println!(
                        "local {kind} frontend at {}: stopped",
                        state.mount_point.display()
                    );
                    degraded = true;
                }
            }
        }

        match KrunkitBackend::new(paths.config_dir.clone())
            .is_running()
            .await
        {
            Ok(Some(true)) => anstream::println!("krunkit frontend: running"),
            Ok(Some(false)) => {
                anstream::println!("krunkit frontend: stopped");
                degraded = true;
            },
            Ok(None) => anstream::println!("krunkit frontend: not found"),
            Err(error) => {
                anstream::eprintln!("⚠  Could not inspect the krunkit frontend: {error:#}");
                degraded = true;
            },
        }

        let container_name = frontend_container_name(&paths)?;

        // The image field is unused by inspection; a valid placeholder is fine.
        let target = DockerTarget::new(
            container_name.as_str().to_string(),
            FRONTEND_DEV_IMAGE.to_string(),
        )?;
        let running = match Runtime::connect_for(&target) {
            Ok(runtime) => DockerBackend::new(runtime).is_running().await?,
            Err(_) => None,
        };

        let attach_addr = record
            .into_iter()
            .flat_map(|record| record.attach)
            .find_map(|attach| match attach {
                AttachRecord::Tcp { addr, .. } => Some(addr),
                AttachRecord::Vsock { .. } => None,
            });

        let exit_code = match running {
            Some(true) => {
                anstream::println!("frontend container `{container_name}`: running");
                ExitCode::Success
            },
            Some(false) => {
                anstream::println!("frontend container `{container_name}`: stopped");
                degraded = true;
                ExitCode::Success
            },
            None => {
                anstream::println!("frontend container: not found");
                ExitCode::Success
            },
        };

        match attach_addr {
            Some(addr) => anstream::println!("attach listener: {addr}"),
            None => anstream::println!("attach listener: not bound"),
        }

        Ok(if degraded {
            ExitCode::Degraded
        } else {
            exit_code
        })
    }
}
