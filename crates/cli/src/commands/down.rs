//! `omnifs down` — container lifecycle: stop.

use clap::Args;

use crate::container_name::ContainerName;
use crate::runtime::Runtime;
use crate::session::{
    self, ENV_CONTAINER_NAME, clean_session_dir, sync_session_credentials_to_host,
};

#[derive(Args, Debug, Clone, Default)]
pub struct DownArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    /// Keep the per-session tmpfs directory after teardown (debugging).
    #[arg(long)]
    pub keep_session: bool,
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::{PathOverrides, Paths};

        let (paths, config) = Paths::resolve_with_config(PathOverrides::default())?;
        let container_name = self
            .container_name
            .or_else(|| session::env_string(ENV_CONTAINER_NAME))
            .or(config.container_name)
            .unwrap_or_else(|| session::CONTAINER_NAME.to_string());
        let container_name = ContainerName::new(container_name)?;
        let remove_result = match Runtime::connect() {
            Ok(runtime) => runtime.remove_existing(&container_name).await,
            Err(error) => Err(error),
        };

        if !self.keep_session {
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
