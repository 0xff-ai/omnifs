//! `omnifs frontend status`: the Docker-hosted FUSE frontend's container
//! state and attach health.

use clap::Args;
use omnifs_workspace::layout::OMNIFS_HOME_ENV;
use omnifs_workspace::runtime_record::RuntimeRecord;

use crate::error::ExitCode;
use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
use crate::launch_backend::DockerTarget;
use crate::runtime::Runtime;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendStatusArgs {}

impl FrontendStatusArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let paths = workspace.layout().clone();

        let is_default_home = std::env::var_os(OMNIFS_HOME_ENV).is_none();
        let container_name = frontend_container_name(&paths.config_dir, is_default_home)?;

        // The image field is unused by inspection; a valid placeholder is fine.
        let target = DockerTarget::new(
            container_name.as_str().to_string(),
            FRONTEND_DEV_IMAGE.to_string(),
        )?;
        let running = match Runtime::connect_for(&target) {
            Ok(runtime) => DockerBackend::new(runtime).is_running().await?,
            Err(_) => None,
        };

        let attach_addr = RuntimeRecord::read(&paths.runtime_record_file())
            .ok()
            .flatten()
            .and_then(|record| record.attach)
            .map(|attach| attach.addr);

        let exit_code = match running {
            Some(true) => {
                anstream::println!("frontend container `{container_name}`: running");
                ExitCode::Success
            },
            Some(false) => {
                anstream::println!("frontend container `{container_name}`: stopped");
                ExitCode::Degraded
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

        Ok(exit_code)
    }
}
