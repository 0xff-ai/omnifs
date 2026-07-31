//! Declarative apply handler.

use crate::{
    commands::{config, daemon_start},
    error::{ErrorVerdict, ExitCode, WithExitCode as _, WithHint as _},
    provider_resolver::resolve_kcl_sources,
    rpc::RpcClient,
    ui::{
        consent::{Decision, Plan, Row},
        output::{Output, ResultVerdict},
    },
};
use anyhow::Context as _;
use getrandom::fill;
use omnifs_api::{
    ApplyReceipt, ApplyResourcesRequest, ProgressEventKind, ProgressSnapshot, ProgressTarget,
    ResourcePhase, ResourcePlan,
};
use omnifs_core::{MutationId, ResourceRevision};
use omnifs_kcl::{EvaluateOptions, evaluate};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplyResult {
    receipt: ApplyReceipt,
    committed_revision: ResourceRevision,
    follow: String,
    snapshot: Option<ProgressSnapshot>,
    outcome: &'static str,
}

pub async fn run(path: Option<PathBuf>, output: Output) -> anyhow::Result<ExitCode> {
    let path = config::default_path(path)?;
    daemon_start::start(&output).await?;
    // The source is evaluated once. Provider imports are content-addressed and
    // inert, so they do not change desired state before this pure plan call.
    let evaluated = evaluate(path, EvaluateOptions::default()).await?;
    let rpc = RpcClient::resolve()?;
    let resolved = resolve_kcl_sources(&evaluated, &rpc).await?;
    let declarations = evaluated.config.into_declarations(&resolved)?;
    let plan = rpc.plan_resources(&declarations).await?;
    if !output.quiet() {
        output.plan(&plan_preview(&plan));
    }
    match Decision::resolve(output.prompt_mode(), false, "Apply?", "--yes", &output)? {
        Decision::Apply => {},
        Decision::DryRun => unreachable!("apply has no dry-run mode"),
    }

    let receipt = rpc
        .apply_resources(&ApplyResourcesRequest {
            mutation_id: random_mutation_id()?,
            base_revision: plan.base_revision,
            expected_desired_digest: plan.desired_digest,
            declarations,
            credential_material: Vec::new(),
        })
        .await?;
    let follow = format!(
        "omnifs status --follow --revision {}",
        receipt.revision.get()
    );
    if output.mode() == crate::ui::output::OutputMode::Human && !output.quiet() {
        output.report(format!("desired revision {} committed\n", receipt.revision));
    }
    let snapshot = match wait_for_revision(&rpc, receipt.revision, &output).await {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            let code = crate::error::exit_code(&error);
            let message = format!(
                "desired revision {} remains applied and daemon work continues: {error}",
                receipt.revision
            );
            let result = ApplyResult {
                committed_revision: receipt.revision,
                receipt,
                follow: follow.clone(),
                snapshot: None,
                outcome: if code == ExitCode::Canceled {
                    "canceled"
                } else {
                    "watch_failed"
                },
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
            return Err(error.context(message)).with_hint(follow);
        },
    };
    let result = ApplyResult {
        committed_revision: receipt.revision,
        receipt,
        follow,
        snapshot,
        outcome: "ready",
    };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        output.report(format!("revision {} ready\n", result.committed_revision));
    }
    Ok(ExitCode::Success)
}

fn random_mutation_id() -> anyhow::Result<MutationId> {
    let mut bytes = [0_u8; 16];
    fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!(error))
        .context("generate resource mutation id")?;
    Ok(MutationId::from_bytes(bytes))
}

fn plan_preview(plan: &ResourcePlan) -> Plan {
    let mut preview = Plan::new("Apply declarative resources");
    for change in &plan.changes {
        let value = format!("{:?}", change.action);
        preview.push(if change.destructive {
            Row::remove(change.key.to_string(), change.key.to_string(), value)
        } else {
            Row::keep(change.key.to_string(), change.key.to_string(), value)
        });
    }
    preview
}

async fn wait_for_revision(
    rpc: &RpcClient,
    revision: ResourceRevision,
    output: &Output,
) -> anyhow::Result<ProgressSnapshot> {
    let mut watch = rpc
        .watch_progress(ProgressTarget::DesiredRevision(revision))
        .await
        .with_context(|| format!("watch desired revision {revision}"))?;
    let mut latest_snapshot = None;
    loop {
        let event = tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C while following resource revision")?;
                return Err(anyhow::anyhow!(
                    "revision {revision} is committed and daemon work continues; follow it with omnifs status --follow --revision {revision}"
                )).with_exit_code(ExitCode::Canceled);
            }
            event = watch.next() => event?,
        };
        let Some(event) = event else {
            return Err(anyhow::anyhow!(
                "progress stream closed before revision {revision} reached a terminal state; daemon work continues"
            ))
            .with_hint(format!("omnifs status --follow --revision {revision}"));
        };
        output.emit_jsonl_event(&event)?;
        match event.event {
            ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot) => {
                latest_snapshot = Some(snapshot.clone());
                if snapshot_outcome(&snapshot, revision)? {
                    return Ok(snapshot);
                }
                render_progress_snapshot(output, &snapshot);
            },
            progress @ (ProgressEventKind::ProviderPreparation(_)
            | ProgressEventKind::ServingProgress(_)
            | ProgressEventKind::CredentialProgress(_)
            | ProgressEventKind::AttachmentProgress(_)) => {
                render_active_progress(output, &progress);
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
                .with_hint(format!("omnifs status --follow --revision {replaced_by}"));
            },
            _ => {},
        }
    }
}

fn render_active_progress(output: &Output, event: &ProgressEventKind) {
    if !output.show_progress() {
        return;
    }
    let line = match event {
        ProgressEventKind::ProviderPreparation(progress) => format!(
            "provider {} [{}] {} ({}, queued {}, active {}, retry {})",
            progress.catalog_name,
            digest_prefix(progress.digest),
            stage_name(progress.stage),
            byte_progress(progress.completed_bytes, progress.total_bytes),
            progress.queued_digests,
            progress.active_digests,
            progress.retry_count,
        ),
        ProgressEventKind::ServingProgress(progress) => format!(
            "serving {} ({}/{}, queued {}, retry {})",
            stage_name(progress.stage),
            progress.completed,
            progress.total,
            progress.queued_generations,
            progress.retry_count,
        ),
        ProgressEventKind::CredentialProgress(progress) => format!(
            "credential {} {} (retry {})",
            progress.key,
            stage_name(progress.stage),
            progress.retry_count,
        ),
        ProgressEventKind::AttachmentProgress(progress) => format!(
            "attachment {} {} ({}, queued {}, active {}, retry {})",
            progress.key,
            stage_name(progress.stage),
            byte_progress(progress.completed_bytes, progress.total_bytes),
            progress.queued_attachments,
            progress.active_attachments,
            progress.retry_count,
        ),
        _ => return,
    };
    output.narrate(line);
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
        .with_hint(format!(
            "omnifs status --follow --revision {}",
            snapshot.desired_revision
        ));
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

fn render_progress_snapshot(output: &Output, snapshot: &ProgressSnapshot) {
    if !output.show_progress() {
        return;
    }
    if let Some(serving) = &snapshot.serving {
        output.narrate(format!(
            "serving {} ({}/{}, queued {}, retry {})",
            stage_name(serving.stage),
            serving.completed,
            serving.total,
            serving.queued_generations,
            serving.retry_count,
        ));
    }
    for provider in &snapshot.providers {
        output.narrate(format!(
            "provider {} [{}] {} ({}, queued {}, active {}, retry {})",
            provider.catalog_name,
            digest_prefix(provider.digest),
            stage_name(provider.stage),
            byte_progress(provider.completed_bytes, provider.total_bytes),
            provider.queued_digests,
            provider.active_digests,
            provider.retry_count,
        ));
    }
    for credential in &snapshot.credentials {
        output.narrate(format!(
            "credential {} {} (retry {})",
            credential.key,
            stage_name(credential.stage),
            credential.retry_count,
        ));
    }
    for attachment in &snapshot.attachments {
        output.narrate(format!(
            "attachment {} {} ({}, queued {}, active {}, retry {})",
            attachment.key,
            stage_name(attachment.stage),
            byte_progress(attachment.completed_bytes, attachment.total_bytes),
            attachment.queued_attachments,
            attachment.active_attachments,
            attachment.retry_count,
        ));
    }
}

fn stage_name<T: std::fmt::Debug>(stage: T) -> String {
    let value = format!("{stage:?}").to_ascii_lowercase();
    value.replace("waitingproviders", "waiting-providers")
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
    use super::{byte_progress, snapshot_outcome};
    use omnifs_api::{ProgressSnapshot, ResourcePhase, ResourceStatus};
    use omnifs_core::{ResourceKey, ResourceKind, ResourceName, ResourceRevision};

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
            attachments: Vec::new(),
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
}
