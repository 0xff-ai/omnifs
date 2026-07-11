//! `omnifs frontend status`: local, Docker, and krunkit frontend state and
//! attach health, whichever drivers this workspace has running.

use clap::Args;
#[cfg(feature = "daemon")]
use omnifs_api::{FrontendDelivery, FrontendInfo, FsType};
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

#[cfg(feature = "daemon")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachmentStatus {
    Connected,
    Disconnected,
    Unknown,
}

#[cfg(feature = "daemon")]
impl AttachmentStatus {
    fn for_state(state: &MountState, live: Option<&[FrontendInfo]>) -> Self {
        let fs_type = match &state.kind {
            omnifs_mtab::MountKind::Fuse => FsType::Fuse,
            omnifs_mtab::MountKind::Nfs { .. } => FsType::Nfs,
        };
        live.map_or(Self::Unknown, |frontends| {
            if frontends.iter().any(|frontend| {
                frontend.delivery == FrontendDelivery::Local
                    && frontend.fs_type == fs_type
                    && frontend.mount_point == state.mount_point
            }) {
                Self::Connected
            } else {
                Self::Disconnected
            }
        })
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Unknown => "unknown",
        }
    }
}

impl FrontendStatusArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let paths = workspace.layout().clone();

        let record = RuntimeRecord::read(&paths.runtime_record_file())
            .ok()
            .flatten();
        let mut degraded = false;
        #[cfg(feature = "daemon")]
        if self.report_local(&workspace).await {
            degraded = true;
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

    #[cfg(feature = "daemon")]
    async fn report_local(&self, workspace: &Workspace) -> bool {
        let mut degraded = false;
        let live_frontends = match workspace.daemon().compatible_status_optional().await {
            Ok(Some(status)) => Some(status.frontends),
            Ok(None) => Some(Vec::new()),
            Err(error) => {
                anstream::eprintln!("⚠  Could not inspect local frontend attachments: {error}");
                degraded = true;
                None
            },
        };
        let state_files = match MountState::files_under(&workspace.layout().frontend_state_root()) {
            Ok(paths) => paths,
            Err(error) => {
                anstream::eprintln!("⚠  Could not discover local frontend records: {error}");
                return true;
            },
        };
        for path in state_files {
            let state = match MountState::read_file(&path) {
                Ok(state) => state,
                Err(error) => {
                    anstream::eprintln!(
                        "⚠  Could not read local frontend record {}: {error}",
                        path.display()
                    );
                    degraded = true;
                    continue;
                },
            };
            let kind = match &state.kind {
                omnifs_mtab::MountKind::Fuse => "FUSE",
                omnifs_mtab::MountKind::Nfs { .. } => "NFS",
            };
            let owned = crate::host_teardown::local_mount_is_owned(&state);
            let attachment = AttachmentStatus::for_state(&state, live_frontends.as_deref());
            anstream::println!(
                "local {kind} frontend at {}: mount {}, attachment {}",
                state.mount_point.display(),
                if owned { "owned" } else { "not mounted" },
                attachment.label()
            );
            if !owned || attachment != AttachmentStatus::Connected {
                degraded = true;
            }
        }
        degraded
    }
}

#[cfg(all(test, feature = "daemon"))]
mod tests {
    use super::*;
    use omnifs_mtab::MountKind;
    use std::path::PathBuf;

    #[test]
    fn local_attachment_requires_matching_live_daemon_entry() {
        let state = MountState {
            version: MountState::VERSION,
            mount_point: PathBuf::from("/mnt/omnifs"),
            pid: 42,
            kind: MountKind::Nfs {
                addr: "127.0.0.1:2049".parse().unwrap(),
            },
        };
        let frontend = FrontendInfo {
            source: "local".to_string(),
            fs_type: FsType::Nfs,
            mount_point: state.mount_point.clone(),
            delivery: FrontendDelivery::Local,
        };

        assert_eq!(
            AttachmentStatus::for_state(&state, Some(std::slice::from_ref(&frontend))),
            AttachmentStatus::Connected
        );
        assert_eq!(
            AttachmentStatus::for_state(&state, Some(&[])),
            AttachmentStatus::Disconnected
        );
        assert_eq!(
            AttachmentStatus::for_state(&state, None),
            AttachmentStatus::Unknown
        );
    }
}
