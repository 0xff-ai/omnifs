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

        DaemonTeardown::new(&workspace).down(force).await?;
        crate::telemetry::maybe_print_health_nudge(&workspace).await;
        Ok(())
    }
}
