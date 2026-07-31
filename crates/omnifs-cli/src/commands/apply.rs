//! Declarative apply handler.

use crate::{
    commands::{daemon_start, plan, resource_flow},
    error::{ErrorVerdict, ExitCode, WithHint as _},
    provider_resolver::resolve_kcl_sources,
    rpc::RpcClient,
    ui::{
        consent::Decision,
        output::{Output, ResultVerdict},
    },
};
use omnifs_api::{ApplyReceipt, ProgressSnapshot};
use omnifs_kcl::evaluate;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplyResult {
    receipt: ApplyReceipt,
    follow: String,
    snapshot: Option<ProgressSnapshot>,
}

pub async fn run(path: Option<PathBuf>, output: Output) -> anyhow::Result<ExitCode> {
    let path = plan::default_path(path)?;
    daemon_start::start(&output).await?;
    // The source is evaluated once. Provider imports are content-addressed and
    // inert, so they do not change desired state before this pure plan call.
    let evaluated = evaluate(path).await?;
    let rpc = RpcClient::resolve()?;
    let resolved = resolve_kcl_sources(&evaluated, &rpc).await?;
    let declarations = evaluated.config.into_declarations(&resolved)?;
    let plan = rpc.plan_resources(&declarations).await?;
    if !output.quiet() {
        output.plan(&resource_flow::plan_preview(
            "Apply declarative resources",
            &plan,
        ));
    }
    match Decision::resolve(output.prompt_mode(), false, "Apply?", "--yes", &output)? {
        Decision::Apply => {},
        Decision::DryRun => unreachable!("apply has no dry-run mode"),
    }

    // This unary call ends at the daemon's durable SQLite commit. The watch
    // below, not the apply RPC, owns the potentially long reconciliation wait.
    let receipt = resource_flow::apply_plan(&rpc, plan, declarations, Vec::new()).await?;
    let follow = format!(
        "omnifs status --follow --revision {}",
        receipt.revision.get()
    );
    if output.mode() == crate::ui::output::OutputMode::Human && !output.quiet() {
        output.report(format!("desired revision {} committed\n", receipt.revision));
    }
    let snapshot = match resource_flow::wait_for_revision(&rpc, receipt.revision, &output).await {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            let code = crate::error::exit_code(&error);
            let message = format!(
                "desired revision {} remains applied and daemon work continues: {error}",
                receipt.revision
            );
            let result = ApplyResult {
                receipt,
                follow: follow.clone(),
                snapshot: None,
            };
            if output.is_structured() {
                let verdict = if code == ExitCode::Canceled {
                    ErrorVerdict::Canceled
                } else {
                    ErrorVerdict::Failed
                };
                let id = if code == ExitCode::Canceled {
                    "canceled"
                } else {
                    "reconcile-failed"
                };
                output.emit_detailed_error(
                    verdict,
                    id,
                    code.code(),
                    message,
                    result.follow.clone(),
                    result,
                )?;
                return Ok(code);
            }
            if code == ExitCode::Canceled {
                output.outro(format!(
                    "Canceled. Desired revision {} remains applied. Follow with {}.",
                    result.receipt.revision, result.follow
                ));
            }
            return Err(error.context(message)).with_hint(follow);
        },
    };
    let result = ApplyResult {
        receipt,
        follow,
        snapshot,
    };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        output.report(format!("revision {} ready\n", result.receipt.revision));
    }
    Ok(ExitCode::Success)
}
