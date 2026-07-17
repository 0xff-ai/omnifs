//! `omnifs status` verb handler.

use crate::error::ExitCode;
use crate::status::InventoryReport;
use crate::ui::output::Output;
use clap::Args;
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {}

impl StatusArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let report = InventoryReport::collect(&workspace).await?;
        let exit_code = report.exit_code();
        if output.is_structured() {
            output.emit_result(report.inventory.verdict(), report.inventory)?;
        } else {
            crate::ui::print_raw(&format!("{}\n", report.render().render()));
        }
        Ok(exit_code)
    }
}
