//! `omnifs down` — container lifecycle: stop.

use clap::Args;

use crate::runtime::Runtime;
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
    /// Keep the per-session tmpfs directory after teardown (debugging).
    #[arg(long)]
    pub keep_session: bool,
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::PathOverrides;

        let DownArgs {
            container_name,
            keep_session,
        } = self;
        let (paths, config) = crate::paths::resolve_with_config(PathOverrides::default())?;
        let container_name = RuntimeTarget::resolve_container_name(container_name, &config)?;
        let remove_result = match Runtime::connect_docker() {
            Ok(runtime) => runtime.remove_existing(&container_name).await,
            Err(error) => Err(error),
        };

        if !keep_session {
            let synced =
                sync_session_credentials_to_host(&container_name, &paths.credentials_file)?;
            if synced > 0 {
                anstream::println!("✓ Synced {synced} OAuth credential(s) refreshed by the daemon");
            }
            clean_session_dir(&container_name)?;
        }
        remove_result?;
        anstream::println!("✓ Container `{container_name}` removed");
        Ok(())
    }
}
