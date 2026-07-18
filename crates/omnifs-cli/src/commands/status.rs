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
            // The context strip already names the next step (`fix:  omnifs
            // up`/`fix:  omnifs logs`) whenever the daemon is not running;
            // repeating a `Browse:` line derived from a daemon that cannot
            // currently serve anything would state two competing "what to
            // do next" facts (spec 3.1).
            if crate::ui::access::show_browse_line(report.inventory.daemon_state()) {
                output.narrate("");
                output.narrate(format!(
                    "Browse:  `{}`",
                    crate::ui::access::browse_command(&report.inventory)
                ));
            }
        }
        Ok(exit_code)
    }
}
