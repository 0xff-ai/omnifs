//! `omnifs down` — runtime lifecycle: stop.

use clap::Args;
use std::path::PathBuf;

use crate::app_context::AppContext;
use crate::native_runtime;
use crate::paths::PathOverrides;
use crate::runtime::Runtime;
use crate::runtime_mode::RuntimeMode;
use crate::runtime_target::RuntimeTarget;
use crate::session::{clean_session_dir, sync_session_credentials_to_host};

#[derive(Args, Debug, Clone, Default)]
pub struct DownArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    /// Runtime launch mode.
    #[arg(long, value_enum)]
    pub mode: Option<RuntimeMode>,
    /// Host mount point for native mode.
    #[arg(long)]
    pub mount_point: Option<PathBuf>,
    /// Keep the per-session tmpfs directory after teardown (debugging).
    #[arg(long)]
    pub keep_session: bool,
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let DownArgs {
            container_name,
            mode,
            mount_point,
            keep_session,
        } = self;
        let ctx = AppContext::resolve_with_runtime(
            PathOverrides::default(),
            container_name,
            None,
            mode,
            mount_point,
        )?;
        let paths = ctx.paths();
        let target = ctx.runtime();
        let teardown_result = match target {
            RuntimeTarget::Docker(docker_target) => match Runtime::connect_docker() {
                Ok(runtime) => {
                    runtime
                        .remove_existing(docker_target.container_name())
                        .await
                },
                Err(error) => Err(error),
            }
            .map(|()| Some("container")),
            RuntimeTarget::Native(native_target) => native_runtime::down(paths, native_target)
                .map(|removed| if removed { Some("native mount") } else { None }),
        };

        if !keep_session {
            let synced =
                sync_session_credentials_to_host(target.session_name(), &paths.credentials_file)?;
            if synced > 0 {
                anstream::println!("✓ Synced {synced} OAuth credential(s) refreshed by the daemon");
            }
            clean_session_dir(target.session_name())?;
        }
        match teardown_result? {
            Some(removed) => anstream::println!("✓ {removed} removed"),
            None => anstream::println!("✓ native mount was not running"),
        }
        Ok(())
    }
}
