//! `omnifs status` read and progress-follow handlers.

use crate::status::InventoryReport;
use crate::ui::access::ActionLine;
use crate::ui::output::{Output, ResultVerdict};
use crate::{
    commands::resource_flow,
    error::{ErrorVerdict, ExitCode},
    rpc::RpcClient,
};
use omnifs_api::{
    ActionPhase, ProgressTarget, ResourceDefinition, ResourcePhase, ResourceSnapshot,
};
use omnifs_core::{ActionId, ResourceKind, ResourceName, ResourceRevision};
use serde::Serialize;

/// A typed `status --follow` target. CLI grammar converts its mutually
/// exclusive flags into this closed value before calling [`follow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FollowTarget {
    Current,
    Revision(ResourceRevision),
    Action(ActionId),
}

impl From<FollowTarget> for ProgressTarget {
    fn from(target: FollowTarget) -> Self {
        match target {
            FollowTarget::Current => Self::Current,
            FollowTarget::Revision(revision) => Self::DesiredRevision(revision),
            FollowTarget::Action(action_id) => Self::Action(action_id),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FollowResult {
    target: FollowTargetResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<resource_flow::FollowedProgress>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
enum FollowTargetResult {
    Current,
    Revision(ResourceRevision),
    Action(ActionId),
}

impl From<FollowTarget> for FollowTargetResult {
    fn from(target: FollowTarget) -> Self {
        match target {
            FollowTarget::Current => Self::Current,
            FollowTarget::Revision(revision) => Self::Revision(revision),
            FollowTarget::Action(action_id) => Self::Action(action_id),
        }
    }
}

pub async fn run(output: Output) -> anyhow::Result<ExitCode> {
    let report = InventoryReport::collect().await?;
    let resources = if report.inventory.daemon.status.is_some() {
        Some(RpcClient::resolve()?.resources().await?)
    } else {
        None
    };
    let exit_code = if resources.as_ref().is_some_and(|snapshot| {
        snapshot
            .resource_statuses
            .iter()
            .any(|status| matches!(status.phase, ResourcePhase::Failed | ResourcePhase::Blocked))
    }) {
        ExitCode::Degraded
    } else {
        report.exit_code()
    };
    if output.is_structured() {
        let verdict = if exit_code == ExitCode::Success {
            ResultVerdict::Ok
        } else {
            ResultVerdict::Degraded
        };
        output.emit_result(
            verdict,
            StatusResult::new(report.inventory, resources.as_ref()),
        )?;
    } else {
        output.report(format!("{}\n", report.render().render()));
        if let Some(snapshot) = &resources {
            output.report(render_resources(snapshot));
        }
        if let Some(action) = report.closing_action() {
            output.narrate("");
            output.narrate(ActionLine::from(&action).render());
        }
    }
    Ok(exit_code)
}

/// Follow current, revision, or durable action progress through the typed
/// daemon stream. Current watches run until Ctrl-C; revision and action
/// watches return only after their target reaches a terminal outcome.
pub(crate) async fn follow(target: FollowTarget, output: Output) -> anyhow::Result<ExitCode> {
    let rpc = RpcClient::resolve()?;
    let outcome = match resource_flow::follow_progress(&rpc, target.into(), &output).await {
        Ok(outcome) => outcome,
        Err(error) if crate::error::exit_code(&error) == ExitCode::Canceled => {
            let follow = follow_command(target);
            if output.is_structured() {
                output.emit_detailed_error(
                    ErrorVerdict::Canceled,
                    "canceled",
                    ExitCode::Canceled.code(),
                    error.to_string(),
                    follow,
                    FollowResult {
                        target: target.into(),
                        outcome: None,
                    },
                )?;
                return Ok(ExitCode::Canceled);
            }
            output.outro(format!(
                "Canceled. Daemon work continues. Follow with {follow}."
            ));
            return Err(error);
        },
        Err(error) => return Err(error),
    };
    let failed_action = match &outcome {
        Some(resource_flow::FollowedProgress::Action(receipt))
            if receipt.phase == ActionPhase::Failed =>
        {
            Some((
                receipt.action_id,
                receipt.error_code.clone(),
                receipt.detail.clone(),
            ))
        },
        _ => None,
    };
    if let Some((action_id, error_code, detail)) = failed_action {
        let follow = format!("omnifs status --follow --action {action_id}");
        let message = format!(
            "action {action_id} failed{}{}",
            error_code
                .as_deref()
                .map(|code| format!(" ({code})"))
                .unwrap_or_default(),
            detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        );
        if output.is_structured() {
            output.emit_detailed_error(
                ErrorVerdict::Failed,
                "action-failed",
                ExitCode::GenericFailure.code(),
                message,
                follow,
                FollowResult {
                    target: target.into(),
                    outcome,
                },
            )?;
        } else {
            output.report(format!("{message}\n"));
        }
        return Ok(ExitCode::GenericFailure);
    }
    if output.is_structured() {
        output.emit_result(
            ResultVerdict::Ok,
            FollowResult {
                target: target.into(),
                outcome,
            },
        )?;
    } else {
        match target {
            FollowTarget::Revision(revision) => {
                output.report(format!("revision {revision} ready\n"));
            },
            FollowTarget::Action(action) => output.report(format!("action {action} complete\n")),
            FollowTarget::Current => {},
        }
    }
    Ok(ExitCode::Success)
}

fn follow_command(target: FollowTarget) -> String {
    match target {
        FollowTarget::Current => "omnifs status --follow".to_owned(),
        FollowTarget::Revision(revision) => {
            format!("omnifs status --follow --revision {revision}")
        },
        FollowTarget::Action(action) => format!("omnifs status --follow --action {action}"),
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResult {
    inventory: crate::inventory::Inventory,
    providers: Vec<ResourceRow>,
    credentials: Vec<ResourceRow>,
    mounts: Vec<ResourceRow>,
    attachments: Vec<ResourceRow>,
}

impl StatusResult {
    fn new(inventory: crate::inventory::Inventory, snapshot: Option<&ResourceSnapshot>) -> Self {
        let mut result = Self {
            inventory,
            providers: Vec::new(),
            credentials: Vec::new(),
            mounts: Vec::new(),
            attachments: Vec::new(),
        };
        let Some(snapshot) = snapshot else {
            return result;
        };
        for resource in &snapshot.resources {
            let key = resource.key();
            let status = snapshot
                .resource_statuses
                .iter()
                .find(|status| status.key == key);
            let row = ResourceRow {
                name: resource.name().clone(),
                phase: status.map_or(ResourcePhase::Pending, |status| status.phase),
                desired_revision: status
                    .map_or(snapshot.revision, |status| status.desired_revision),
                observed_revision: status.and_then(|status| status.observed_revision),
                detail: status.and_then(|status| status.detail.clone()),
            };
            match resource {
                ResourceDefinition::Provider(_) => result.providers.push(row),
                ResourceDefinition::Credential(_) => result.credentials.push(row),
                ResourceDefinition::Mount(_) => result.mounts.push(row),
                ResourceDefinition::Attachment(_) => result.attachments.push(row),
            }
        }
        result
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceRow {
    name: ResourceName,
    phase: ResourcePhase,
    desired_revision: ResourceRevision,
    observed_revision: Option<ResourceRevision>,
    detail: Option<String>,
}

fn render_resources(snapshot: &ResourceSnapshot) -> String {
    use crate::ui::table::{
        Block, Cell, Column, Priority, Report, ResourceRow as TableRow, ResourceTable, StateToken,
        WidthPolicy,
    };

    let mut table = ResourceTable::new(
        "Resources",
        format!(
            "desired {}, serving {}",
            snapshot.revision,
            snapshot
                .serving_revision
                .map_or_else(|| "none".to_owned(), |revision| revision.to_string())
        ),
        vec![
            Column::new("Kind", Priority::Identity, WidthPolicy::Auto),
            Column::new("Name", Priority::Identity, WidthPolicy::Auto),
            Column::new("Phase", Priority::Essential, WidthPolicy::Auto),
            Column::new("Desired", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Observed", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Detail", Priority::Detail, WidthPolicy::Auto),
        ],
    );
    let mut resources = snapshot.resources.iter().collect::<Vec<_>>();
    resources.sort_by_key(|resource| resource.key());
    for resource in resources {
        let key = resource.key();
        let status = snapshot
            .resource_statuses
            .iter()
            .find(|status| status.key == key);
        let phase = status.map_or(ResourcePhase::Pending, |status| status.phase);
        let desired = status.map_or(snapshot.revision, |status| status.desired_revision);
        let observed = status
            .and_then(|status| status.observed_revision)
            .map_or_else(|| "-".to_owned(), |revision| revision.to_string());
        let detail = status
            .and_then(|status| status.detail.as_deref())
            .unwrap_or("-");
        let state = match phase {
            ResourcePhase::Ready => StateToken::positive(phase_label(phase)),
            ResourcePhase::Failed | ResourcePhase::Blocked => {
                StateToken::failure(phase_label(phase))
            },
            ResourcePhase::Preparing | ResourcePhase::Retrying => {
                StateToken::attention(phase_label(phase))
            },
            ResourcePhase::Pending | ResourcePhase::Deleting => {
                StateToken::neutral(phase_label(phase))
            },
        };
        table.push(TableRow::new(
            [
                Cell::new(kind_label(resource.kind())),
                Cell::new(resource.name().to_string()),
                Cell::state(state.clone()),
                Cell::new(desired.to_string()),
                Cell::new(observed),
                Cell::new(detail),
            ],
            state,
        ));
    }
    let mut report = Report::new();
    report.push(Block::Resources(table));
    format!("\n{}", report.render())
}

const fn kind_label(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Provider => "Provider",
        ResourceKind::Credential => "Credential",
        ResourceKind::Mount => "Mount",
        ResourceKind::Attachment => "Attachment",
    }
}

const fn phase_label(phase: ResourcePhase) -> &'static str {
    match phase {
        ResourcePhase::Pending => "pending",
        ResourcePhase::Preparing => "preparing",
        ResourcePhase::Ready => "ready",
        ResourcePhase::Retrying => "retrying",
        ResourcePhase::Failed => "failed",
        ResourcePhase::Blocked => "blocked",
        ResourcePhase::Deleting => "deleting",
    }
}

#[cfg(test)]
mod tests {
    use super::{FollowTarget, follow_command};
    use omnifs_api::ProgressTarget;
    use omnifs_core::{ActionId, ResourceRevision};

    #[test]
    fn follow_targets_keep_revision_and_action_scopes_distinct() {
        let revision = ResourceRevision::new(7);
        assert_eq!(
            ProgressTarget::from(FollowTarget::Revision(revision)),
            ProgressTarget::DesiredRevision(revision)
        );
        let action = ActionId::from_bytes([3; 16]);
        assert_eq!(
            ProgressTarget::from(FollowTarget::Action(action)),
            ProgressTarget::Action(action)
        );
        assert_eq!(
            follow_command(FollowTarget::Revision(revision)),
            "omnifs status --follow --revision 7"
        );
        assert_eq!(
            follow_command(FollowTarget::Action(action)),
            format!("omnifs status --follow --action {action}")
        );
    }
}
