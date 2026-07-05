//! `omnifs status` verb handler.

use crate::cli::OutputFormat;
use crate::error::ExitCode;
use crate::status::StatusReport;
use crate::workspace::Workspace;
use anyhow::Context as _;
use clap::Args;

#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {
    /// Reveal configured provider detail.
    #[arg(long = "detail")]
    pub detail: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

impl StatusArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let mounts = workspace.mounts()?;
        let runtime = workspace.daemon().compatible_status_optional().await?;
        let report = StatusReport::collect(
            workspace.catalog(),
            workspace.layout().clone(),
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
        Ok(report.exit_code())
    }
}
