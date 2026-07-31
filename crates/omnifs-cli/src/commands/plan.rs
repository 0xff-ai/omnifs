//! Declarative `omnifs plan` handler.

use crate::{
    commands::daemon_start,
    error::ExitCode,
    provider_resolver::resolve_kcl_sources,
    rpc::RpcClient,
    ui::output::{Output, ResultVerdict},
};
use omnifs_api::{ResourceChangeAction, ResourcePlan};
use omnifs_kcl::evaluate;
use std::fmt::Write as _;
use std::path::PathBuf;

pub async fn run(path: Option<PathBuf>, output: Output) -> anyhow::Result<ExitCode> {
    let path = default_path(path)?;
    daemon_start::start(&output).await?;
    let evaluated = evaluate(path).await?;
    let rpc = RpcClient::resolve()?;
    let resolved = resolve_kcl_sources(&evaluated, &rpc).await?;
    let declarations = evaluated.config.into_declarations(&resolved)?;
    let plan = rpc.plan_resources(&declarations).await?;
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, &plan)?;
    } else {
        output.report(render_plan(&plan));
    }
    Ok(ExitCode::Success)
}

pub(crate) fn default_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let path = PathBuf::from("omnifs.k");
    anyhow::ensure!(
        path.is_file(),
        "no omnifs.k in the current directory; pass a path"
    );
    Ok(path)
}

fn render_plan(plan: &ResourcePlan) -> String {
    let mut output = String::new();
    let changed = plan
        .changes
        .iter()
        .filter(|change| change.action != ResourceChangeAction::Unchanged)
        .count();
    writeln!(
        output,
        "Plan (base revision {}, desired digest {})",
        plan.base_revision, plan.desired_digest
    )
    .expect("writing to a String cannot fail");
    if changed == 0 {
        output.push_str("No changes.\n");
        return output;
    }
    writeln!(output, "{changed} change(s):").expect("writing to a String cannot fail");
    for change in &plan.changes {
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
    for change in plan.changes.iter().filter(|change| change.destructive) {
        writeln!(output, "Warning: deleting {} is destructive", change.key)
            .expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{default_path, render_plan};
    use omnifs_api::{ResourceChange, ResourceChangeAction, ResourcePlan};
    use omnifs_core::{ResourceDigest, ResourceKey, ResourceKind, ResourceName, ResourceRevision};
    use std::path::PathBuf;

    #[test]
    fn explicit_path_is_preserved() {
        let path = PathBuf::from("some/omnifs.k");
        assert_eq!(default_path(Some(path.clone())).unwrap(), path);
    }

    #[test]
    fn empty_plan_is_explicitly_no_changes() {
        let plan = ResourcePlan {
            base_revision: ResourceRevision::new(7),
            desired_digest: ResourceDigest::from_bytes([0; 32]),
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
    fn plan_renders_every_change_class_and_warns_on_destructive_rows() {
        let key = |kind, name| ResourceKey::new(kind, ResourceName::new(name).unwrap());
        let plan = ResourcePlan {
            base_revision: ResourceRevision::new(1),
            desired_digest: ResourceDigest::from_bytes([1; 32]),
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
                    key: key(ResourceKind::Filesystem, "same"),
                    action: ResourceChangeAction::Unchanged,
                    destructive: false,
                    secret_impact: false,
                },
            ],
        };
        assert!(render_plan(&plan).contains("Warning: deleting Credential/delete is destructive"));
    }
}
