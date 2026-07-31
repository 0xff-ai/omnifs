//! Declarative `omnifs plan` handler.

use crate::{
    commands::daemon_start,
    error::ExitCode,
    provider_resolver::resolve_kcl_sources,
    rpc::RpcClient,
    ui::output::{Output, ResultVerdict},
};
use omnifs_api::{ResourceChange, ResourceChangeAction, ResourcePlan};
use omnifs_kcl::{EvaluateOptions, evaluate};
use serde::Serialize;
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanResult {
    base_revision: omnifs_core::ResourceRevision,
    desired_digest: omnifs_core::ResourceDigest,
    changes: Vec<ResourceChange>,
    warnings: Vec<String>,
    counts: PlanCounts,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanCounts {
    creates: usize,
    updates: usize,
    deletes: usize,
    unchanged: usize,
}

pub async fn run(path: Option<PathBuf>, output: Output) -> anyhow::Result<ExitCode> {
    let path = super::config::default_path(path)?;
    daemon_start::start(&output).await?;
    let evaluated = evaluate(path, EvaluateOptions::default()).await?;
    let rpc = RpcClient::resolve()?;
    let resolved = resolve_kcl_sources(&evaluated, &rpc).await?;
    let declarations = evaluated.config.into_declarations(&resolved)?;
    let plan = rpc.plan_resources(&declarations).await?;
    let result = plan_result(&plan);
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        output.report(render_plan(&plan));
    }
    Ok(ExitCode::Success)
}

fn render_plan(plan: &ResourcePlan) -> String {
    let mut output = String::new();
    let result = plan_result(plan);
    let changed = result.counts.creates + result.counts.updates + result.counts.deletes;
    writeln!(
        output,
        "Plan (base revision {}, desired digest {})",
        result.base_revision, result.desired_digest
    )
    .expect("writing to a String cannot fail");
    if changed == 0 {
        output.push_str("No changes.\n");
        return output;
    }
    writeln!(output, "{changed} change(s):").expect("writing to a String cannot fail");
    for change in &result.changes {
        let marker = match change.action {
            ResourceChangeAction::Create => '+',
            ResourceChangeAction::Update => '~',
            ResourceChangeAction::Delete => '-',
            ResourceChangeAction::Unchanged => ' ',
        };
        let warning = if change.destructive {
            " (destructive)"
        } else {
            ""
        };
        writeln!(output, "  {marker} {}{warning}", change.key)
            .expect("writing to a String cannot fail");
    }
    for warning in &result.warnings {
        writeln!(output, "Warning: {warning}").expect("writing to a String cannot fail");
    }
    output
}

fn plan_result(plan: &ResourcePlan) -> PlanResult {
    let mut counts = PlanCounts::default();
    for change in &plan.changes {
        match change.action {
            ResourceChangeAction::Create => counts.creates += 1,
            ResourceChangeAction::Update => counts.updates += 1,
            ResourceChangeAction::Delete => counts.deletes += 1,
            ResourceChangeAction::Unchanged => counts.unchanged += 1,
        }
    }
    let warnings = plan
        .changes
        .iter()
        .filter(|change| change.destructive)
        .map(|change| format!("deleting {} is destructive", change.key))
        .collect();
    PlanResult {
        base_revision: plan.base_revision,
        desired_digest: plan.desired_digest,
        changes: plan.changes.clone(),
        warnings,
        counts,
    }
}

#[cfg(test)]
mod tests {
    use super::{plan_result, render_plan};
    use omnifs_api::{ResourceChange, ResourceChangeAction, ResourcePlan};
    use omnifs_core::{ResourceDigest, ResourceKey, ResourceKind, ResourceName, ResourceRevision};

    #[test]
    fn empty_plan_is_explicitly_no_changes() {
        let plan = ResourcePlan {
            base_revision: ResourceRevision::new(7),
            desired_digest: ResourceDigest::from_bytes([0; 32]),
            normalized: Vec::new(),
            changes: vec![ResourceChange {
                key: ResourceKey::new(ResourceKind::Mount, ResourceName::new("demo").unwrap()),
                action: ResourceChangeAction::Unchanged,
                destructive: false,
                secret_impact: false,
            }],
        };
        assert!(render_plan(&plan).contains("No changes."));
    }

    #[test]
    fn plan_counts_every_change_class_and_warns_on_destructive_rows() {
        let key = |kind, name| ResourceKey::new(kind, ResourceName::new(name).unwrap());
        let plan = ResourcePlan {
            base_revision: ResourceRevision::new(1),
            desired_digest: ResourceDigest::from_bytes([1; 32]),
            normalized: Vec::new(),
            changes: vec![
                ResourceChange {
                    key: key(ResourceKind::Provider, "create"),
                    action: ResourceChangeAction::Create,
                    destructive: false,
                    secret_impact: false,
                },
                ResourceChange {
                    key: key(ResourceKind::Mount, "update"),
                    action: ResourceChangeAction::Update,
                    destructive: false,
                    secret_impact: false,
                },
                ResourceChange {
                    key: key(ResourceKind::Credential, "delete"),
                    action: ResourceChangeAction::Delete,
                    destructive: true,
                    secret_impact: true,
                },
                ResourceChange {
                    key: key(ResourceKind::Attachment, "same"),
                    action: ResourceChangeAction::Unchanged,
                    destructive: false,
                    secret_impact: false,
                },
            ],
        };
        let result = plan_result(&plan);
        assert_eq!(result.counts.creates, 1);
        assert_eq!(result.counts.updates, 1);
        assert_eq!(result.counts.deletes, 1);
        assert_eq!(result.counts.unchanged, 1);
        assert_eq!(
            result.warnings,
            vec!["deleting Credential/delete is destructive"]
        );
        assert!(render_plan(&plan).contains("Warning: deleting Credential/delete is destructive"));
    }
}
