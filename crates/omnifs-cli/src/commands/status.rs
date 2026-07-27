//! `omnifs status` verb handler.

use crate::error::ExitCode;
use crate::status::InventoryReport;
use crate::ui::output::Output;
use omnifs_workspace::Workspace;

pub async fn run(output: Output) -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let report = InventoryReport::collect(&workspace).await?;
    let exit_code = report.exit_code();
    if output.is_structured() {
        output.emit_result(report.inventory.verdict(), report.inventory)?;
    } else {
        crate::ui::print_raw(&format!("{}\n", report.render().render()));
        if let Some(action) = report.closing_action() {
            output.narrate("");
            output.narrate(crate::ui::access::action_line(&action).render());
        }
    }
    Ok(exit_code)
}
