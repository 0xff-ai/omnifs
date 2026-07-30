//! `omnifs status` verb handler.

use crate::error::ExitCode;
use crate::status::InventoryReport;
use crate::ui::access::ActionLine;
use crate::ui::output::Output;

pub async fn run(output: Output) -> anyhow::Result<ExitCode> {
    let report = InventoryReport::collect().await?;
    let exit_code = report.exit_code();
    if output.is_structured() {
        output.emit_result(report.inventory.verdict(), report.inventory)?;
    } else {
        output.report(format!("{}\n", report.render().render()));
        if let Some(action) = report.closing_action() {
            output.narrate("");
            output.narrate(ActionLine::from(&action).render());
        }
    }
    Ok(exit_code)
}
