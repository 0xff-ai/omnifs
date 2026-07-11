//! `omnifs status` verb handler.

use crate::cli::OutputFormat;
use crate::error::ExitCode;
use crate::status::StatusReport;
use crate::workspace::Workspace;
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
            &mounts,
        );
        let exit_code = report.exit_code();
        match OutputFormat::from(self.json) {
            OutputFormat::Json => crate::ui::print_json(&report.into_json())?,
            OutputFormat::Text => report.build_report(self.detail).print(),
        }
        Ok(exit_code)
    }
}
