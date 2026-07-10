//! `omnifs frontend status`: the virtualized FUSE frontend's state and attach
//! health, whichever backend it was launched with.

use clap::Args;
use omnifs_workspace::layout::OMNIFS_HOME_ENV;
use omnifs_workspace::runtime_record::{RuntimeRecord, Via};

use crate::error::ExitCode;
use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
use crate::krunkit_backend::{KrunkitBackend, UNUSED_GUEST_IMAGE_PLACEHOLDER};
use crate::launch_backend::DockerTarget;
use crate::runtime::Runtime;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendStatusArgs {}

impl FrontendStatusArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let paths = workspace.layout().clone();

        let recorded_via = RuntimeRecord::read(&paths.runtime_record_file())
            .ok()
            .flatten()
            .and_then(|record| record.frontends.iter().find_map(|frontend| frontend.via));
        if recorded_via == Some(Via::Krunkit) {
            return Self::krunkit_status(&paths).await;
        }

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

    /// The krunkit guest's pidfile-based liveness. The daemon's vsock-attach
    /// listener binding is never persisted into the runtime record (unlike
    /// the TCP listener's `attach` field: see
    /// `crates/omnifs-daemon/src/server.rs::ensure_attach_uds`), so there is
    /// no attach-address line to report here.
    async fn krunkit_status(
        paths: &omnifs_workspace::layout::WorkspaceLayout,
    ) -> anyhow::Result<ExitCode> {
        let backend = KrunkitBackend::new(
            paths.config_dir.clone(),
            UNUSED_GUEST_IMAGE_PLACEHOLDER.into(),
        );
        let running = backend.is_running().await?;
        let exit_code = match running {
            Some(true) => {
                anstream::println!("krunkit frontend: running");
                ExitCode::Success
            },
            Some(false) => {
                anstream::println!("krunkit frontend: stopped");
                ExitCode::Degraded
            },
            None => {
                anstream::println!("krunkit frontend: not found");
                ExitCode::Success
            },
        };
        Ok(exit_code)
    }
}
