//! `omnifs status` verb handler.

use crate::cli::OutputFormat;
use crate::client::DaemonProbe;
use crate::paths::PathOverrides;
use crate::status::collect_status;
use crate::workspace::Workspace;
use anyhow::Context as _;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
    /// Reveal provider runtime detail.
    #[arg(long = "detail")]
    pub detail: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

impl StatusArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let overrides = PathOverrides {
            config_dir: self.config_dir.clone(),
            ..Default::default()
        };
        let workspace = Workspace::resolve(overrides)?;
        let mounts = workspace.mounts()?;
        let runtime = match workspace.daemon().probe().await? {
            DaemonProbe::Unreachable => None,
            DaemonProbe::Compatible(status) => Some(*status),
        };
        let report = collect_status(
            workspace.catalog(),
            workspace.paths().clone(),
            runtime,
            mounts,
        );
        match OutputFormat::from(self.json) {
            OutputFormat::Json => {
                let payload = report.to_json();
                let serialized =
                    serde_json::to_string(&payload).context("serialize status JSON")?;
                anstream::println!("{serialized}");
            },
            OutputFormat::Text => {
                anstream::print!("{}", report.render(self.detail));
            },
        }
        Ok(())
    }
}
