//! Shared resource-plan, apply, and progress-watch flow.
//!
//! The daemon owns validation, durable planning, apply, and reconciliation.
//! This module only composes the typed client calls and keeps the receipt
//! separate from later readiness observed through `WatchProgress`.

use crate::{
    error::{ErrorVerdict, ExitCode, WithExitCode as _, WithHint as _},
    rpc::RpcClient,
    ui::{
        consent::{Decision, Plan, Row},
        output::Output,
    },
};
use anyhow::Context as _;
use getrandom::fill;
use omnifs_api::{
    ActionPhase, ActionReceipt, ApplyReceipt, ApplyResourcesRequest, CredentialMaterialSidecar,
    ProgressEventKind, ProgressSnapshot, ProgressTarget, ResourceChangeAction,
    ResourceDeclarations, ResourceDefinition, ResourcePhase, ResourcePlan,
};
use omnifs_core::{ActionId, MutationId, ResourceKey, ResourceRevision};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// The one automation route for mutations that would otherwise prompt.
pub(crate) const AUTOMATION_HINT: &str =
    "Use `omnifs plan <file>` and `omnifs apply <file> --yes` for automation.";

/// Durable desired-state receipt plus the terminal revision snapshot.
#[derive(Debug)]
pub(crate) struct AppliedResources {
    pub(crate) receipt: ApplyReceipt,
    pub(crate) snapshot: ProgressSnapshot,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "desired revision {} remains applied and daemon work continues: {source}",
    receipt.revision
)]
struct CommittedResourceWatchError {
    receipt: ApplyReceipt,
    #[source]
    source: anyhow::Error,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommittedResourceResult {
    receipt: ApplyReceipt,
    follow: String,
}

/// Terminal state returned by a target-scoped follow. Current follows have no
/// terminal state because they intentionally run until the client detaches.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub(crate) enum FollowedProgress {
    Revision(ProgressSnapshot),
    Action(ActionReceipt),
}

/// Read, edit, plan, consent, apply, and follow one interactive resource edit.
///
/// `edit` changes only the caller's in-memory complete desired set. The daemon
/// remains the sole owner of normalization and cross-resource validation.
pub(crate) async fn edit_resources_and_wait(
    rpc: &RpcClient,
    output: &Output,
    title: &str,
    edit: impl FnOnce(&mut Vec<omnifs_api::ResourceDefinition>) -> anyhow::Result<()>,
    credential_material: Vec<CredentialMaterialSidecar>,
) -> anyhow::Result<AppliedResources> {
    ensure_interactive_mutation(output)?;
    let current = rpc.resources().await?;
    let mut declarations = ResourceDeclarations {
        api_version: omnifs_api::API_VERSION.to_owned(),
        resources: current.resources,
    };
    let before = declarations.resources.clone();
    edit(&mut declarations.resources)?;
    let scoped = changed_keys(&before, &declarations.resources);
    let plan = rpc.plan_resources(&declarations).await?;
    output.plan(&plan_preview_scoped(title, &plan, Some(&scoped)));
    match Decision::resolve(output.prompt_mode(), false, "Apply?", "--yes", output)? {
        Decision::Apply => {},
        Decision::DryRun => unreachable!("interactive resource edits cannot be dry runs"),
    }
    apply_plan_and_wait(rpc, output, plan, declarations, credential_material).await
}

/// Apply the exact daemon plan and observe its target revision to a terminal
/// state. This never moves reconciliation work into the unary apply RPC.
pub(crate) async fn apply_plan_and_wait(
    rpc: &RpcClient,
    output: &Output,
    plan: ResourcePlan,
    declarations: ResourceDeclarations,
    credential_material: Vec<CredentialMaterialSidecar>,
) -> anyhow::Result<AppliedResources> {
    let receipt = apply_plan(rpc, plan, declarations, credential_material).await?;
    if output.mode() == crate::ui::output::OutputMode::Human && !output.quiet() {
        output.report(format!("desired revision {} committed\n", receipt.revision));
    }
    let snapshot = match wait_for_revision(rpc, receipt.revision, output).await {
        Ok(snapshot) => snapshot,
        Err(source) => {
            let follow = revision_follow_hint(receipt.revision);
            let code = crate::error::exit_code(&source);
            return Err(anyhow::Error::new(CommittedResourceWatchError {
                receipt,
                source,
            }))
            .with_hint(follow)
            .with_exit_code(code);
        },
    };
    if output.quiet() && !output.is_structured() {
        output.report(format!("revision {} ready\n", receipt.revision));
    }
    Ok(AppliedResources { receipt, snapshot })
}

/// Settle an error returned after a durable resource commit. Structured
/// modes emit the one terminal envelope here so the caller does not lose the
/// receipt. Human mode keeps the ordinary error path and exact follow hint.
pub(crate) fn finish_resource_error(
    output: &Output,
    error: anyhow::Error,
) -> anyhow::Result<ExitCode> {
    let committed = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<CommittedResourceWatchError>()
            .map(|value| value.receipt.clone())
    });
    let Some(receipt) = committed else {
        return Err(error);
    };
    let code = crate::error::exit_code(&error);
    let follow = revision_follow_hint(receipt.revision);
    if !output.is_structured() {
        if code == ExitCode::Canceled {
            output.outro(format!(
                "Canceled. Desired revision {} remains applied. Follow with {follow}.",
                receipt.revision
            ));
        }
        return Err(error);
    }
    output.emit_detailed_error(
        if code == ExitCode::Canceled {
            ErrorVerdict::Canceled
        } else {
            ErrorVerdict::Failed
        },
        if code == ExitCode::Canceled {
            "canceled"
        } else {
            "reconcile-failed"
        },
        code.code(),
        error.to_string(),
        follow.clone(),
        CommittedResourceResult { receipt, follow },
    )?;
    Ok(code)
}

/// Commit the exact typed plan and return as soon as the daemon durably
/// acknowledges it. Callers must separately follow the returned revision.
pub(crate) async fn apply_plan(
    rpc: &RpcClient,
    plan: ResourcePlan,
    declarations: ResourceDeclarations,
    credential_material: Vec<CredentialMaterialSidecar>,
) -> anyhow::Result<ApplyReceipt> {
    rpc.apply_resources(&ApplyResourcesRequest {
        mutation_id: random_mutation_id()?,
        base_revision: plan.base_revision,
        expected_desired_digest: plan.desired_digest,
        declarations,
        credential_material,
    })
    .await
}

/// Follow a status target. Current watches deliberately run until the caller
/// cancels them; durable revision and action targets end at their own terminal
/// event or terminal first snapshot.
pub(crate) async fn follow_progress(
    rpc: &RpcClient,
    target: ProgressTarget,
    output: &Output,
) -> anyhow::Result<Option<FollowedProgress>> {
    match target {
        ProgressTarget::DesiredRevision(revision) => wait_for_revision(rpc, revision, output)
            .await
            .map(FollowedProgress::Revision)
            .map(Some),
        ProgressTarget::Action(action_id) => wait_for_action(rpc, action_id, output)
            .await
            .map(FollowedProgress::Action)
            .map(Some),
        ProgressTarget::Current => follow_current(rpc, output).await,
    }
}

/// Render daemon-provided changes in the common consent layout.
pub(crate) fn plan_preview(title: &str, plan: &ResourcePlan) -> Plan {
    plan_preview_scoped(title, plan, None)
}

fn plan_preview_scoped(
    title: &str,
    plan: &ResourcePlan,
    scoped: Option<&BTreeSet<ResourceKey>>,
) -> Plan {
    let mut preview = Plan::new(title);
    for change in plan
        .changes
        .iter()
        .filter(|change| change.action != ResourceChangeAction::Unchanged)
    {
        let id = change.key.to_string();
        let key = if scoped.is_some_and(|keys| keys.contains(&change.key)) {
            format!("{} (selected)", change.key)
        } else {
            change.key.to_string()
        };
        let mut value = match change.action {
            ResourceChangeAction::Create => "create",
            ResourceChangeAction::Update => "update",
            ResourceChangeAction::Delete => "delete",
            ResourceChangeAction::Unchanged => unreachable!("unchanged changes were filtered"),
        }
        .to_owned();
        if change.secret_impact {
            value.push_str(", credential material affected");
        }
        if change.destructive {
            value.push_str(", destructive");
        }
        preview.push(if change.action == ResourceChangeAction::Delete {
            Row::remove(id, key, value)
        } else {
            Row::keep(id, key, value)
        });
    }
    if preview.rows.is_empty() {
        preview.push(Row::keep("desired-state", "Desired state", "no changes"));
    }
    preview
}

fn changed_keys(
    before: &[ResourceDefinition],
    after: &[ResourceDefinition],
) -> BTreeSet<ResourceKey> {
    let before = before
        .iter()
        .map(|resource| (resource.key(), resource))
        .collect::<BTreeMap<_, _>>();
    let after = after
        .iter()
        .map(|resource| (resource.key(), resource))
        .collect::<BTreeMap<_, _>>();
    before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key) != after.get(*key))
        .cloned()
        .collect()
}

/// Reject prompt-driven porcelain before it can read or alter daemon state.
pub(crate) fn ensure_interactive_mutation(output: &Output) -> anyhow::Result<()> {
    if output.interactive() {
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "interactive resource mutations require a terminal. {AUTOMATION_HINT}"
    ))
    .with_exit_code(ExitCode::AuthRequired)
}

pub(crate) async fn wait_for_revision(
    rpc: &RpcClient,
    revision: ResourceRevision,
    output: &Output,
) -> anyhow::Result<ProgressSnapshot> {
    let mut watch = rpc
        .watch_progress(ProgressTarget::DesiredRevision(revision))
        .await
        .with_context(|| format!("watch desired revision {revision}"))?;
    let mut latest_snapshot = None;
    let mut renderer = ProgressRenderer::default();
    loop {
        let event = next_event(&mut watch, revision_follow_hint(revision)).await?;
        let Some(event) = event else {
            return Err(anyhow::anyhow!(
                "progress stream closed before revision {revision} reached a terminal state; daemon work continues"
            ))
            .with_hint(revision_follow_hint(revision));
        };
        output.emit_jsonl_event(&event)?;
        match event.event {
            ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot) => {
                latest_snapshot = Some(snapshot.clone());
                if snapshot_outcome(&snapshot, revision)? {
                    return Ok(snapshot);
                }
                render_progress_snapshot(output, &snapshot, &mut renderer);
            },
            progress @ (ProgressEventKind::ProviderPreparation(_)
            | ProgressEventKind::ServingProgress(_)
            | ProgressEventKind::CredentialProgress(_)
            | ProgressEventKind::FilesystemProgress(_)) => {
                render_active_progress(output, &progress, &mut renderer);
            },
            ProgressEventKind::RevisionReady(ready) if ready == revision => {
                return latest_snapshot.ok_or_else(|| {
                    anyhow::anyhow!("revision {revision} became ready without a progress snapshot")
                });
            },
            ProgressEventKind::RevisionFailed {
                revision: failed,
                error_code,
                detail,
            } if failed == revision => {
                anyhow::bail!("revision {failed} failed ({error_code}): {detail}");
            },
            ProgressEventKind::RevisionSuperseded {
                revision: replaced,
                replaced_by,
            } if replaced == revision => {
                return Err(anyhow::anyhow!(
                    "revision {replaced} was superseded by revision {replaced_by}; daemon work continues"
                ))
                .with_hint(revision_follow_hint(replaced_by));
            },
            _ => {},
        }
    }
}

async fn wait_for_action(
    rpc: &RpcClient,
    action_id: ActionId,
    output: &Output,
) -> anyhow::Result<ActionReceipt> {
    let mut watch = rpc
        .watch_progress(ProgressTarget::Action(action_id))
        .await
        .with_context(|| format!("watch action {action_id}"))?;
    let mut renderer = ProgressRenderer::default();
    loop {
        let event = next_event(&mut watch, action_follow_hint(action_id)).await?;
        let Some(event) = event else {
            return Err(anyhow::anyhow!(
                "progress stream closed before action {action_id} reached a terminal state; daemon work continues"
            ))
            .with_hint(action_follow_hint(action_id));
        };
        output.emit_jsonl_event(&event)?;
        match event.event {
            ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot) => {
                render_progress_snapshot(output, &snapshot, &mut renderer);
                if let Some(receipt) = snapshot.actions.into_iter().find(|receipt| {
                    receipt.action_id == action_id
                        && matches!(receipt.phase, ActionPhase::Ready | ActionPhase::Failed)
                }) {
                    return Ok(receipt);
                }
            },
            progress @ (ProgressEventKind::ProviderPreparation(_)
            | ProgressEventKind::ServingProgress(_)
            | ProgressEventKind::CredentialProgress(_)
            | ProgressEventKind::FilesystemProgress(_)) => {
                render_active_progress(output, &progress, &mut renderer);
            },
            ProgressEventKind::ActionCompleted(receipt)
            | ProgressEventKind::ActionFailed {
                receipt,
                error_code: _,
                detail: _,
            } if receipt.action_id == action_id => {
                return Ok(receipt);
            },
            _ => {},
        }
    }
}

async fn follow_current(
    rpc: &RpcClient,
    output: &Output,
) -> anyhow::Result<Option<FollowedProgress>> {
    let mut watch = rpc
        .watch_progress(ProgressTarget::Current)
        .await
        .context("watch current daemon progress")?;
    let mut renderer = ProgressRenderer::default();
    loop {
        let event = tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for Ctrl-C while following current daemon work")?;
            return Err(anyhow::Error::new(crate::ui::prompt::Canceled)
                .context("stopped following current daemon work"))
                .with_exit_code(ExitCode::Canceled);
        }
            event = watch.next() => event?,
        };
        let Some(event) = event else {
            anyhow::bail!("current progress stream closed");
        };
        output.emit_jsonl_event(&event)?;
        match event.event {
            ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot) => {
                render_progress_snapshot(output, &snapshot, &mut renderer);
            },
            progress @ (ProgressEventKind::ProviderPreparation(_)
            | ProgressEventKind::ServingProgress(_)
            | ProgressEventKind::CredentialProgress(_)
            | ProgressEventKind::FilesystemProgress(_)) => {
                render_active_progress(output, &progress, &mut renderer);
            },
            _ => {},
        }
    }
}

async fn next_event(
    watch: &mut crate::rpc::ProgressWatch,
    follow_hint: String,
) -> anyhow::Result<Option<omnifs_api::ProgressEvent>> {
    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for Ctrl-C while following daemon work")?;
            Err(anyhow::Error::new(crate::ui::prompt::Canceled)
                .context(format!("daemon work continues; follow it with {follow_hint}")))
            .with_exit_code(ExitCode::Canceled)
        }
        event = watch.next() => event,
    }
}

fn revision_follow_hint(revision: ResourceRevision) -> String {
    format!("omnifs status --follow --revision {revision}")
}

fn action_follow_hint(action_id: ActionId) -> String {
    format!("omnifs status --follow --action {action_id}")
}

fn random_mutation_id() -> anyhow::Result<MutationId> {
    Ok(MutationId::from_bytes(random_id_bytes()?))
}

pub(crate) fn random_action_id() -> anyhow::Result<ActionId> {
    Ok(ActionId::from_bytes(random_id_bytes()?))
}

fn random_id_bytes() -> anyhow::Result<[u8; 16]> {
    let mut bytes = [0_u8; 16];
    fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!(error))
        .context("generate operation id")?;
    Ok(bytes)
}

#[derive(Default)]
struct ProgressRenderer {
    last_stage: BTreeMap<String, &'static str>,
}

impl ProgressRenderer {
    fn should_render(&mut self, key: String, stage: &'static str) -> bool {
        self.last_stage.insert(key, stage) != Some(stage)
    }
}

fn render_active_progress(
    output: &Output,
    event: &ProgressEventKind,
    renderer: &mut ProgressRenderer,
) {
    if !output.show_progress() {
        return;
    }
    let (key, stage, line) = match event {
        ProgressEventKind::ProviderPreparation(progress) => (
            format!("provider:{}", progress.digest),
            stage_name(progress.stage),
            format!(
                "provider {} [{}] {} ({}, queued {}, active {}, retry {})",
                progress.catalog_name,
                digest_prefix(progress.digest),
                stage_name(progress.stage),
                byte_progress(progress.completed_bytes, progress.total_bytes),
                progress.queued_digests,
                progress.active_digests,
                progress.retry_count,
            ),
        ),
        ProgressEventKind::ServingProgress(progress) => (
            format!("serving:{}", progress.revision),
            stage_name(progress.stage),
            format!(
                "serving {} ({}/{}, queued {}, retry {})",
                stage_name(progress.stage),
                progress.completed,
                progress.total,
                progress.queued_generations,
                progress.retry_count,
            ),
        ),
        ProgressEventKind::CredentialProgress(progress) => (
            format!("credential:{}", progress.key),
            stage_name(progress.stage),
            format!("credential {} {}", progress.key, stage_name(progress.stage)),
        ),
        ProgressEventKind::FilesystemProgress(progress) => (
            format!("filesystem:{}", progress.key),
            stage_name(progress.stage),
            format!(
                "filesystem {} {} ({}, queued {}, active {}, retry {})",
                progress.key,
                stage_name(progress.stage),
                byte_progress(progress.completed_bytes, progress.total_bytes),
                progress.queued_filesystems,
                progress.active_filesystems,
                progress.retry_count,
            ),
        ),
        _ => return,
    };
    if renderer.should_render(key, stage) {
        output.narrate(line);
    }
}

fn snapshot_outcome(
    snapshot: &ProgressSnapshot,
    revision: ResourceRevision,
) -> anyhow::Result<bool> {
    if snapshot.desired_revision > revision {
        return Err(anyhow::anyhow!(
            "revision {revision} was superseded by revision {}",
            snapshot.desired_revision
        ))
        .with_hint(revision_follow_hint(snapshot.desired_revision));
    }
    if let Some(status) = snapshot.resources.iter().find(|status| {
        status.desired_revision == revision
            && matches!(status.phase, ResourcePhase::Failed | ResourcePhase::Blocked)
    }) {
        anyhow::bail!(
            "{} failed{}{}",
            status.key,
            status
                .error_code
                .as_deref()
                .map(|code| format!(" ({code})"))
                .unwrap_or_default(),
            status
                .detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        );
    }
    Ok(snapshot
        .observed_revision
        .is_some_and(|observed| observed >= revision)
        && snapshot
            .resources
            .iter()
            .filter(|status| status.desired_revision == revision)
            .all(|status| status.phase == ResourcePhase::Ready))
}

fn render_progress_snapshot(
    output: &Output,
    snapshot: &ProgressSnapshot,
    renderer: &mut ProgressRenderer,
) {
    if !output.show_progress() {
        return;
    }
    if let Some(serving) = &snapshot.serving {
        render_active_progress(
            output,
            &ProgressEventKind::ServingProgress(serving.clone()),
            renderer,
        );
    }
    for provider in &snapshot.providers {
        render_active_progress(
            output,
            &ProgressEventKind::ProviderPreparation(provider.clone()),
            renderer,
        );
    }
    for credential in &snapshot.credentials {
        render_active_progress(
            output,
            &ProgressEventKind::CredentialProgress(credential.clone()),
            renderer,
        );
    }
    for filesystem in &snapshot.filesystems {
        render_active_progress(
            output,
            &ProgressEventKind::FilesystemProgress(filesystem.clone()),
            renderer,
        );
    }
}

trait StageName {
    fn stage_name(self) -> &'static str;
}

impl StageName for omnifs_api::ProviderPreparationStage {
    fn stage_name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Compiling => "compiling",
            Self::Retrying => "retrying",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl StageName for omnifs_api::ServingProgressStage {
    fn stage_name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::WaitingProviders => "waiting-providers",
            Self::ProvidersReady => "providers-ready",
            Self::Building => "building",
            Self::Built => "built",
            Self::Publishing => "publishing",
            Self::Draining => "draining",
            Self::Degraded => "degraded",
            Self::Retrying => "retrying",
            Self::Superseded => "superseded",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl StageName for omnifs_api::CredentialProgressStage {
    fn stage_name(self) -> &'static str {
        match self {
            Self::Refreshing => "refreshing",
            Self::Revoking => "revoking",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl StageName for omnifs_api::FilesystemProgressStage {
    fn stage_name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::WaitingForNamespace => "waiting-for-namespace",
            Self::PullingImage => "pulling-image",
            Self::Materializing => "materializing",
            Self::Starting => "starting",
            Self::Mounting => "mounting",
            Self::Stopping => "stopping",
            Self::Retrying => "retrying",
            Self::Deleting => "deleting",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl StageName for ResourcePhase {
    fn stage_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Preparing => "preparing",
            Self::Ready => "ready",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Deleting => "deleting",
        }
    }
}

fn stage_name<T: StageName>(stage: T) -> &'static str {
    stage.stage_name()
}

fn digest_prefix(digest: omnifs_core::ProviderId) -> String {
    digest.to_string().chars().take(12).collect()
}

fn byte_progress(completed: u64, total: Option<u64>) -> String {
    total.map_or_else(
        || format!("{completed} bytes"),
        |total| format!("{completed}/{total} bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        AUTOMATION_HINT, byte_progress, changed_keys, ensure_interactive_mutation,
        plan_preview_scoped, snapshot_outcome,
    };
    use crate::error::{ExitCode, exit_code};
    use crate::ui::output::{Output, OutputMode};
    use omnifs_api::{
        ProgressSnapshot, ProviderDefinition, ResourceChange, ResourceChangeAction,
        ResourceDefinition, ResourcePhase, ResourcePlan, ResourceStatus,
    };
    use omnifs_core::{
        ProviderId, ResourceDigest, ResourceKey, ResourceKind, ResourceName, ResourceRevision,
    };

    #[test]
    fn interactive_mutations_refuse_machine_output_with_the_kcl_route() {
        let error = ensure_interactive_mutation(&Output::new(OutputMode::Json, false)).unwrap_err();
        assert!(error.to_string().contains(AUTOMATION_HINT));
        assert_eq!(exit_code(&error), ExitCode::AuthRequired);
    }

    #[test]
    fn snapshot_waits_until_every_resource_is_ready() {
        let revision = ResourceRevision::new(4);
        let key = ResourceKey::new(ResourceKind::Mount, ResourceName::new("demo").unwrap());
        let mut snapshot = ProgressSnapshot {
            desired_revision: revision,
            observed_revision: Some(revision),
            resources: vec![ResourceStatus {
                key,
                desired_revision: revision,
                observed_revision: Some(revision),
                phase: ResourcePhase::Pending,
                error_code: None,
                detail: None,
            }],
            actions: Vec::new(),
            providers: Vec::new(),
            serving: None,
            credentials: Vec::new(),
            filesystems: Vec::new(),
        };
        assert!(!snapshot_outcome(&snapshot, revision).unwrap());
        snapshot.resources[0].phase = ResourcePhase::Ready;
        assert!(snapshot_outcome(&snapshot, revision).unwrap());
    }

    #[test]
    fn byte_progress_never_invents_an_unknown_total() {
        assert_eq!(byte_progress(7, None), "7 bytes");
        assert_eq!(byte_progress(7, Some(10)), "7/10 bytes");
    }

    #[test]
    fn porcelain_plan_marks_the_selected_edit_and_keeps_dependent_changes() {
        let selected = ResourceKey::new(
            ResourceKind::Provider,
            ResourceName::new("selected").unwrap(),
        );
        let dependent =
            ResourceKey::new(ResourceKind::Mount, ResourceName::new("dependent").unwrap());
        let definition = ResourceDefinition::Provider(ProviderDefinition {
            name: selected.name.clone(),
            artifact: ProviderId::from_digest([1; 32]),
        });
        let scoped = changed_keys(&[], std::slice::from_ref(&definition));
        let preview = plan_preview_scoped(
            "Add provider",
            &ResourcePlan {
                base_revision: ResourceRevision::new(1),
                desired_digest: ResourceDigest::from_bytes([2; 32]),
                changes: vec![
                    ResourceChange {
                        key: selected,
                        action: ResourceChangeAction::Create,
                        destructive: false,
                        secret_impact: false,
                    },
                    ResourceChange {
                        key: dependent,
                        action: ResourceChangeAction::Update,
                        destructive: false,
                        secret_impact: false,
                    },
                ],
            },
            Some(&scoped),
        );
        assert_eq!(preview.rows.len(), 2);
        assert!(preview.rows[0].key.contains("(selected)"));
        assert!(!preview.rows[1].key.contains("(selected)"));
    }
}
