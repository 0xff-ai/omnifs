//! `omnifs down`: daemon lifecycle stop.
//!
//! Resolution order:
//!   1. Probe the control port: if a live daemon answers, trust
//!      `DaemonStatus.backend` to identify the backend.
//!   2. Fall back to the launch record: if the daemon is dead, the record
//!      says what was started.
//!   3. If neither applies, nothing is running.
//!
//! The backend is never inferred from `[system].runtime`. `down` is
//! backend-transparent: it dispatches through `LaunchBackend::reclaim` without
//! naming Docker or native.

use clap::Args;

use crate::daemon_teardown::DaemonTeardown;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct DownArgs {
    /// Force the host-native unmount if a clean shutdown leaves the mount busy.
    #[arg(long)]
    pub force: bool,
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let DownArgs { force } = self;
        let workspace = Workspace::resolve()?;

        // A contributor dev session (started by `omnifs dev`) records itself
        // under the dev home; its runtime and db/k8s fixtures run in
        // containers. Sweep the whole session and skip the normal daemon
        // teardown. A no-op outside a dev session, so production `down` never
        // touches it.
        let dev_home = workspace.layout().config_dir.clone();
        if omnifs_fixture::DevSessionRecord::read(&dev_home)?.is_some() {
            match omnifs_fixture::DevSessionRecord::teardown_all(&dev_home).await {
                Ok(()) => anstream::println!("✓ Dev session torn down"),
                Err(error) => anstream::eprintln!("note: dev session teardown: {error:#}"),
            }
            return Ok(());
        }

        DaemonTeardown::new(&workspace).down(force).await?;
        Ok(())
    }
}
