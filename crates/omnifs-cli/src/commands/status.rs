//! `omnifs status` verb handler.

use clap::Args;

use crate::error::ExitCode;
use crate::rpc::RpcClient;
use crate::status::InventoryReport;
use crate::ui::access::ActionLine;
use crate::ui::output::Output;

#[derive(Args, Debug, Clone, Default)]
#[command(after_help = "Examples:\n  omnifs status\n  omnifs status --ready")]
pub struct StatusArgs {
    /// Give the status of a starting daemon once it is ready or requires
    /// recovery; without a daemon, give the status at once.
    #[arg(long)]
    pub ready: bool,
}

pub async fn run(args: StatusArgs, output: Output) -> anyhow::Result<ExitCode> {
    if args.ready {
        RpcClient::resolve()?.settled_phase().await?;
    }
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
