//! Fast declarative resource validation, durable commit, and reconcile signals.

use crate::credential_document::prepare_credential_document;
use crate::progress::ProgressHub;
use omnifs_api::{
    ActionKind, ApplyReceipt, ApplyResourcesRequest, CredentialReceipt, CredentialStatusKind,
    NormalizedResourceSet, ProgressSnapshot, ProgressTarget, ResourceChangeAction,
    ResourceDeclarations, ResourceDefinition, ResourceDefinitionError, ResourcePhase, ResourcePlan,
    ResourceStatus, RevokeCredentialRequest, SetCredentialMaterialRequest, plan,
};
use omnifs_core::{
    ActionId, ProviderId, ResourceKey, ResourceKind, ResourceName, ResourceRevision,
};
use omnifs_state::{
    ActionWriteError, CredentialActionOperation, CredentialActionRequest, CredentialSecretSidecar,
    ResourceApplyError, ResourceApplyRequest as StateApplyRequest, StateStore,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

pub(crate) struct ResourceControl {
    state: Arc<StateStore>,
    revision_wakeup: watch::Sender<ResourceRevision>,
    action_wakeup: watch::Sender<Option<ActionId>>,
    progress: Arc<ProgressHub>,
    publication_fence: Arc<tokio::sync::Mutex<()>>,
    shutting_down: AtomicBool,
}

impl ResourceControl {
    #[cfg(test)]
    pub(crate) async fn new(
        state: Arc<StateStore>,
        daemon_instance_id: &str,
    ) -> anyhow::Result<Arc<Self>> {
        Self::new_with_progress(state, daemon_instance_id, None).await
    }

    pub(crate) async fn new_with_progress(
        state: Arc<StateStore>,
        daemon_instance_id: &str,
        existing_progress: Option<Arc<ProgressHub>>,
    ) -> anyhow::Result<Arc<Self>> {
        let current = state.resource_snapshot().await?;
        let actions = state.action_receipts().await?;
        let serving_revision = if existing_progress.is_some() {
            None
        } else {
            state
                .serving_state()
                .await
                .ok()
                .map(|serving| ResourceRevision::new(serving.revision.get()))
        };
        let statuses = current
            .resources
            .resources()
            .iter()
            .map(|resource| ResourceStatus {
                key: resource.key(),
                desired_revision: current.revision,
                observed_revision: serving_revision
                    .filter(|revision| *revision >= current.revision),
                phase: if serving_revision.is_some_and(|revision| revision >= current.revision) {
                    ResourcePhase::Ready
                } else {
                    ResourcePhase::Pending
                },
                error_code: None,
                detail: None,
            })
            .collect();
        let (providers, serving, credentials, attachments) = existing_progress
            .as_ref()
            .map(|progress| {
                let (_, snapshot) = progress.snapshot_for(ProgressTarget::Current);
                (
                    snapshot.providers,
                    snapshot.serving,
                    snapshot.credentials,
                    snapshot.attachments,
                )
            })
            .unwrap_or_default();
        let snapshot = ProgressSnapshot {
            desired_revision: current.revision,
            observed_revision: serving_revision,
            resources: statuses,
            actions,
            providers,
            serving,
            credentials,
            attachments,
        };
        let progress = existing_progress
            .unwrap_or_else(|| ProgressHub::new(daemon_instance_id, snapshot.clone()));
        progress.register_revision_providers(
            current.revision,
            revision_provider_membership(&current.resources),
        );
        if progress.snapshot_for(ProgressTarget::Current).1 != snapshot {
            progress.publish_snapshot(ProgressTarget::Current, snapshot);
        }
        let (revision_wakeup, _) = watch::channel(current.revision);
        let (action_wakeup, _) = watch::channel(None);
        Ok(Arc::new(Self {
            state,
            revision_wakeup,
            action_wakeup,
            progress,
            publication_fence: Arc::new(tokio::sync::Mutex::new(())),
            shutting_down: AtomicBool::new(false),
        }))
    }

    pub(crate) fn progress(&self) -> &Arc<ProgressHub> {
        &self.progress
    }

    pub(crate) fn publication_fence(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.publication_fence)
    }

    pub(crate) async fn snapshot(
        &self,
    ) -> Result<omnifs_api::ResourceSnapshot, ResourceControlError> {
        let _publication_fence = self.publication_fence.lock().await;
        let snapshot = self.state.resource_snapshot().await?;
        let (_, progress) = self.progress.snapshot_for(ProgressTarget::Current);
        Ok(omnifs_api::ResourceSnapshot {
            revision: snapshot.revision,
            desired_digest: snapshot.desired_digest,
            resources: snapshot.resources.resources().to_vec(),
            resource_statuses: progress.resources,
            serving_revision: progress.observed_revision,
            providers: progress.providers,
            serving: progress.serving,
        })
    }

    pub(crate) async fn plan(
        &self,
        declarations: ResourceDeclarations,
    ) -> Result<ResourcePlan, ResourceControlError> {
        self.ensure_admitted()?;
        let desired = self.normalize_and_validate(declarations).await?;
        let current = self.state.resource_snapshot().await?;
        let changes = plan(&current.resources, &desired);
        Ok(ResourcePlan {
            base_revision: current.revision,
            desired_digest: desired.digest(),
            normalized: desired.resources().to_vec(),
            changes,
        })
    }

    /// Validate, commit one `SQLite` transaction, issue non-blocking wakeups,
    /// and return. No runtime or reconciliation owner is reachable here.
    pub(crate) async fn apply(
        &self,
        request: ApplyResourcesRequest,
    ) -> Result<ApplyReceipt, ResourceControlError> {
        self.ensure_admitted()?;
        let ApplyResourcesRequest {
            mutation_id,
            base_revision,
            expected_desired_digest,
            declarations,
            credential_material,
        } = request;
        let desired = self.normalize_and_validate(declarations).await?;
        if desired.digest() != expected_desired_digest {
            return Err(ResourceControlError::DesiredDigestMismatch);
        }
        let current = self.state.resource_snapshot().await?;
        if current.desired_digest != desired.digest() && current.revision != base_revision {
            return Err(ResourceApplyError::StaleRevision {
                expected: base_revision,
                actual: current.revision,
            }
            .into());
        }
        let sidecars = self
            .prepare_apply_credentials(&desired, credential_material)
            .await?;
        // Serialize only the commit-to-publication decision. Reconciliation
        // never holds this fence during provider waits, generation builds, or
        // drains, so Apply cannot inherit runtime work.
        let _publication_fence = self.publication_fence.lock().await;
        let receipt = self
            .state
            .apply_resources(StateApplyRequest {
                mutation_id,
                base_revision,
                expected_desired_digest,
                desired: desired.clone(),
                credential_secrets: sidecars,
            })
            .await?;
        self.progress
            .register_revision_providers(receipt.revision, revision_provider_membership(&desired));
        self.revision_wakeup.send_replace(receipt.revision);
        if receipt.changed {
            let (_, previous) = self.progress.snapshot_for(ProgressTarget::Current);
            let statuses = next_resource_statuses(
                &current.resources,
                &desired,
                receipt.revision,
                &previous.resources,
            );
            self.progress.update_snapshot(
                ProgressTarget::DesiredRevision(receipt.revision),
                |snapshot| {
                    snapshot.desired_revision = receipt.revision;
                    snapshot.resources = statuses;
                },
            );
        }
        Ok(receipt)
    }

    pub(crate) async fn set_credential_material(
        &self,
        request: SetCredentialMaterialRequest,
    ) -> Result<CredentialReceipt, ResourceControlError> {
        self.ensure_admitted()?;
        if let Some(action) = self.state.action_receipt(request.action_id).await? {
            if action.kind != ActionKind::SetCredentialMaterial
                || action.target
                    != ResourceKey::new(ResourceKind::Credential, request.credential.clone())
            {
                return Err(ActionWriteError::IdReuse(request.action_id).into());
            }
            return Ok(CredentialReceipt {
                status: credential_action_status(action.kind, action.phase),
                action,
            });
        }
        let desired = self.state.resource_snapshot().await?;
        let (credential, provider) = credential_target(&desired.resources, &request.credential)?;
        let document = prepare_credential_document(
            &self.state,
            omnifs_api::CredentialSubmission {
                provider,
                scheme: credential.scheme.clone(),
                account_label: credential.account.clone(),
                material: request.material,
                overrides: request.overrides,
            },
        )
        .await
        .map_err(|error| ResourceControlError::InvalidCredentialMaterial(error.to_string()))?;
        let action = self
            .state
            .accept_credential_action(CredentialActionRequest {
                action_id: request.action_id,
                credential: request.credential,
                expected_generation: request.base_action_generation,
                operation: CredentialActionOperation::SetMaterial(document),
            })
            .await?;
        self.publish_action(&action);
        Ok(CredentialReceipt {
            status: credential_action_status(action.kind, action.phase),
            action,
        })
    }

    pub(crate) async fn revoke_credential(
        &self,
        request: RevokeCredentialRequest,
    ) -> Result<CredentialReceipt, ResourceControlError> {
        self.ensure_admitted()?;
        let action = self
            .state
            .accept_credential_action(CredentialActionRequest {
                action_id: request.action_id,
                credential: request.credential,
                expected_generation: request.base_action_generation,
                operation: CredentialActionOperation::Revoke,
            })
            .await?;
        self.publish_action(&action);
        Ok(CredentialReceipt {
            status: credential_action_status(action.kind, action.phase),
            action,
        })
    }

    pub(crate) fn publish_action(&self, action: &omnifs_api::ActionReceipt) {
        self.progress.record_action_receipt(action.clone());
        self.action_wakeup.send_replace(Some(action.action_id));
    }

    async fn normalize_and_validate(
        &self,
        declarations: ResourceDeclarations,
    ) -> Result<NormalizedResourceSet, ResourceControlError> {
        if declarations.resources.len() > omnifs_api::CONTROL_RESOURCE_MAX_COUNT {
            return Err(ResourceControlError::PlanTooLarge {
                count: declarations.resources.len(),
            });
        }
        let desired = declarations.normalize()?;
        let mut providers = BTreeMap::new();
        for resource in desired.resources() {
            let ResourceDefinition::Provider(provider) = resource else {
                continue;
            };
            let metadata = self
                .state
                .load_provider_metadata(provider.artifact)
                .await?
                .ok_or(ResourceControlError::MissingProviderArtifact(
                    provider.artifact,
                ))?;
            providers.insert(provider.name.clone(), metadata);
        }
        for resource in desired.resources() {
            let ResourceDefinition::Credential(credential) = resource else {
                continue;
            };
            let provider = providers
                .get(&credential.provider)
                .expect("normalized credential provider exists");
            if provider
                .manifest
                .auth
                .as_ref()
                .and_then(|auth| auth.scheme(&credential.scheme))
                .is_none()
            {
                return Err(ResourceControlError::InvalidCredentialScheme {
                    credential: credential.name.clone(),
                    scheme: credential.scheme.clone(),
                });
            }
        }
        Ok(desired)
    }

    async fn prepare_apply_credentials(
        &self,
        desired: &NormalizedResourceSet,
        sidecars: Vec<omnifs_api::CredentialMaterialSidecar>,
    ) -> Result<Vec<CredentialSecretSidecar>, ResourceControlError> {
        let mut prepared = Vec::with_capacity(sidecars.len());
        let mut seen = BTreeSet::new();
        for sidecar in sidecars {
            if !seen.insert(sidecar.credential.clone()) {
                return Err(ResourceControlError::DuplicateCredentialMaterial(
                    sidecar.credential,
                ));
            }
            let (credential, provider) = credential_target(desired, &sidecar.credential)?;
            let document = prepare_credential_document(
                &self.state,
                omnifs_api::CredentialSubmission {
                    provider,
                    scheme: credential.scheme.clone(),
                    account_label: credential.account.clone(),
                    material: sidecar.material,
                    overrides: sidecar.overrides,
                },
            )
            .await
            .map_err(|error| ResourceControlError::InvalidCredentialMaterial(error.to_string()))?;
            prepared.push(CredentialSecretSidecar {
                credential: sidecar.credential,
                document,
            });
        }
        Ok(prepared)
    }

    #[allow(dead_code, reason = "Plan 003 reconciliation consumes this wakeup")]
    pub(crate) fn subscribe_revisions(&self) -> watch::Receiver<ResourceRevision> {
        self.revision_wakeup.subscribe()
    }

    #[allow(
        dead_code,
        reason = "Plan 003 action reconciliation consumes this wakeup"
    )]
    pub(crate) fn subscribe_actions(&self) -> watch::Receiver<Option<ActionId>> {
        self.action_wakeup.subscribe()
    }

    pub(crate) fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    fn ensure_admitted(&self) -> Result<(), ResourceControlError> {
        if self.shutting_down.load(Ordering::Acquire) {
            Err(ResourceControlError::ShuttingDown)
        } else {
            Ok(())
        }
    }
}

fn credential_action_status(
    kind: ActionKind,
    phase: omnifs_api::ActionPhase,
) -> CredentialStatusKind {
    use omnifs_api::ActionPhase;
    match (kind, phase) {
        (ActionKind::SetCredentialMaterial, ActionPhase::Failed)
        | (ActionKind::RestartAttachment, _) => CredentialStatusKind::Blocked,
        (ActionKind::SetCredentialMaterial, _) => CredentialStatusKind::Active,
        (ActionKind::RevokeCredential, ActionPhase::Ready) => CredentialStatusKind::Deleted,
        (ActionKind::RevokeCredential, ActionPhase::Failed) => {
            CredentialStatusKind::RevocationUnknown
        },
        (ActionKind::RevokeCredential, _) => CredentialStatusKind::RevocationPending,
    }
}

fn credential_target<'a>(
    desired: &'a NormalizedResourceSet,
    name: &ResourceName,
) -> Result<(&'a omnifs_api::CredentialDefinition, ProviderId), ResourceControlError> {
    let credential = desired
        .resources()
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Credential(credential) if &credential.name == name => {
                Some(credential)
            },
            _ => None,
        })
        .ok_or_else(|| ResourceControlError::CredentialNotFound(name.clone()))?;
    let provider = desired
        .resources()
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Provider(provider) if provider.name == credential.provider => {
                Some(provider.artifact)
            },
            _ => None,
        })
        .expect("normalized credential provider exists");
    Ok((credential, provider))
}

fn next_resource_statuses(
    current: &NormalizedResourceSet,
    desired: &NormalizedResourceSet,
    revision: ResourceRevision,
    previous: &[ResourceStatus],
) -> Vec<ResourceStatus> {
    let changes = plan(current, desired);
    let previous: BTreeMap<_, _> = previous
        .iter()
        .map(|status| (status.key.clone(), status))
        .collect();
    let mut statuses = Vec::with_capacity(changes.len());
    for change in changes {
        let old = previous.get(&change.key).copied();
        let (phase, observed_revision) = match change.action {
            ResourceChangeAction::Unchanged => old
                .map_or((ResourcePhase::Pending, None), |status| {
                    (status.phase, status.observed_revision)
                }),
            ResourceChangeAction::Create | ResourceChangeAction::Update => {
                (ResourcePhase::Pending, None)
            },
            ResourceChangeAction::Delete => (
                ResourcePhase::Deleting,
                old.and_then(|status| status.observed_revision),
            ),
        };
        statuses.push(ResourceStatus {
            key: change.key,
            desired_revision: revision,
            observed_revision,
            phase,
            error_code: None,
            detail: None,
        });
    }
    statuses
}

fn revision_provider_membership(
    desired: &NormalizedResourceSet,
) -> HashMap<ProviderId, Vec<ResourceName>> {
    let providers: BTreeMap<_, _> = desired
        .resources()
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Provider(provider) => {
                Some((provider.name.clone(), provider.artifact))
            },
            _ => None,
        })
        .collect();
    let mut membership = HashMap::<ProviderId, Vec<ResourceName>>::new();
    for resource in desired.resources() {
        let ResourceDefinition::Mount(mount) = resource else {
            continue;
        };
        let Some(provider_id) = providers.get(&mount.provider) else {
            continue;
        };
        membership
            .entry(*provider_id)
            .or_default()
            .push(mount.provider.clone());
    }
    for names in membership.values_mut() {
        names.sort();
        names.dedup();
    }
    membership
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResourceControlError {
    #[error("daemon is shutting down")]
    ShuttingDown,
    #[error(
        "resource plan contains {count} entries; the limit is {}",
        omnifs_api::CONTROL_RESOURCE_MAX_COUNT
    )]
    PlanTooLarge { count: usize },
    #[error("provider artifact {0} is not retained")]
    MissingProviderArtifact(ProviderId),
    #[error("credential `{credential}` uses undeclared auth scheme `{scheme}`")]
    InvalidCredentialScheme {
        credential: ResourceName,
        scheme: String,
    },
    #[error("credential resource `{0}` was not found")]
    CredentialNotFound(ResourceName),
    #[error("credential material target `{0}` appears more than once")]
    DuplicateCredentialMaterial(ResourceName),
    #[error("credential material is invalid: {0}")]
    InvalidCredentialMaterial(String),
    #[error("desired digest does not match the normalized declarations")]
    DesiredDigestMismatch,
    #[error(transparent)]
    Definition(#[from] ResourceDefinitionError),
    #[error(transparent)]
    Apply(#[from] ResourceApplyError),
    #[error(transparent)]
    Action(#[from] ActionWriteError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{API_VERSION, ActionPhase, ProviderDefinition};
    use omnifs_core::MutationId;

    fn provider_wasm() -> Vec<u8> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "description": "A test provider",
            "provider": "demo.wasm",
            "defaultMount": "demo",
            "refreshIntervalSecs": 0,
            "capabilities": [{
                "kind": "domain",
                "value": "api.demo.test",
                "why": "Test credential injection."
            }],
            "auth": {
                "default": "pat",
                "schemes": [{
                    "staticToken": {
                        "key": "pat",
                        "valuePrefix": "Bearer ",
                        "description": "Demo token",
                        "injectDomains": ["api.demo.test"]
                    }
                }]
            }
        }))
        .unwrap();
        let name = omnifs_provider::PROVIDER_METADATA_SECTION_NAME.as_bytes();
        let mut payload = Vec::new();
        append_uleb(&mut payload, name.len());
        payload.extend_from_slice(name);
        payload.extend_from_slice(&metadata);
        let mut wasm = b"\0asm\x01\0\0\0".to_vec();
        wasm.push(0);
        append_uleb(&mut wasm, payload.len());
        wasm.extend_from_slice(&payload);
        wasm
    }

    fn append_uleb(output: &mut Vec<u8>, mut value: usize) {
        loop {
            let byte = u8::try_from(value & 0x7f).unwrap();
            value >>= 7;
            if value == 0 {
                output.push(byte);
                return;
            }
            output.push(byte | 0x80);
        }
    }

    async fn control_fixture() -> (
        tempfile::TempDir,
        Arc<StateStore>,
        Arc<ResourceControl>,
        ProviderId,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let endpoint =
            omnifs_bootstrap::Bootstrap::<omnifs_bootstrap::Daemon>::under_root(temp.path());
        let state = Arc::new(
            StateStore::open(&endpoint, omnifs_state::StateStoreOptions::default())
                .await
                .unwrap(),
        );
        let bytes = provider_wasm();
        let digest = ProviderId::from_wasm_bytes(&bytes);
        let upload = state
            .stage_provider_bytes("demo.wasm", digest, &bytes)
            .await
            .unwrap();
        state.import_provider(upload).await.unwrap();
        let control = ResourceControl::new(Arc::clone(&state), "test-daemon")
            .await
            .unwrap();
        (temp, state, control, digest)
    }

    fn declarations(digest: ProviderId) -> ResourceDeclarations {
        ResourceDeclarations {
            api_version: API_VERSION.to_owned(),
            resources: vec![ResourceDefinition::Provider(ProviderDefinition {
                name: ResourceName::new("demo").unwrap(),
                artifact: digest,
            })],
        }
    }

    #[test]
    fn resource_status_diff_keeps_deletion_tombstones() {
        let provider = ProviderId::from_digest([3; 32]);
        let current = NormalizedResourceSet::new(vec![ResourceDefinition::Provider(
            omnifs_api::ProviderDefinition {
                name: ResourceName::new("demo").unwrap(),
                artifact: provider,
            },
        )])
        .unwrap();
        let desired = NormalizedResourceSet::empty();
        let statuses = next_resource_statuses(
            &current,
            &desired,
            ResourceRevision::new(2),
            &[ResourceStatus {
                key: ResourceKey::new(
                    omnifs_core::ResourceKind::Provider,
                    ResourceName::new("demo").unwrap(),
                ),
                desired_revision: ResourceRevision::new(1),
                observed_revision: Some(ResourceRevision::new(1)),
                phase: ResourcePhase::Ready,
                error_code: None,
                detail: None,
            }],
        );
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].phase, ResourcePhase::Deleting);
        assert_eq!(
            statuses[0].observed_revision,
            Some(ResourceRevision::new(1))
        );
    }

    #[test]
    fn credential_action_replay_status_matches_the_durable_terminal_receipt() {
        assert_eq!(
            credential_action_status(ActionKind::SetCredentialMaterial, ActionPhase::Ready),
            CredentialStatusKind::Active
        );
        assert_eq!(
            credential_action_status(ActionKind::SetCredentialMaterial, ActionPhase::Failed),
            CredentialStatusKind::Blocked
        );
        assert_eq!(
            credential_action_status(ActionKind::RevokeCredential, ActionPhase::Running),
            CredentialStatusKind::RevocationPending
        );
        assert_eq!(
            credential_action_status(ActionKind::RevokeCredential, ActionPhase::Ready),
            CredentialStatusKind::Deleted
        );
        assert_eq!(
            credential_action_status(ActionKind::RevokeCredential, ActionPhase::Failed),
            CredentialStatusKind::RevocationUnknown
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resource_control_applies_without_a_reconciler_and_recovers_retries() {
        let (_temp, state, control, digest) = control_fixture().await;
        let desired = declarations(digest);
        let plan = control.plan(desired.clone()).await.unwrap();
        assert_eq!(plan.base_revision, ResourceRevision::new(1));
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].action, ResourceChangeAction::Create);

        // This receiver never consumes a wakeup. Apply must still commit and
        // reply because the revision watch is a coalescing notification.
        let _stalled_reconciler = control.subscribe_revisions();
        let next_revision = plan.base_revision.next().unwrap();
        let mut progress = control
            .progress()
            .subscribe(ProgressTarget::DesiredRevision(next_revision));
        let first = progress.recv().await.unwrap();
        assert!(matches!(
            first.event,
            omnifs_api::ProgressEventKind::Snapshot(_)
        ));

        let mutation_id = MutationId::from_bytes([0x31; 16]);
        let request = || ApplyResourcesRequest {
            mutation_id,
            base_revision: plan.base_revision,
            expected_desired_digest: plan.desired_digest,
            declarations: desired.clone(),
            credential_material: Vec::new(),
        };
        let receipt = control.apply(request()).await.unwrap();
        assert!(receipt.changed);
        assert_eq!(receipt.revision, next_revision);
        assert_eq!(control.snapshot().await.unwrap().revision, receipt.revision);
        assert_eq!(control.apply(request()).await.unwrap(), receipt);

        let committed = progress.recv().await.unwrap();
        assert_eq!(
            committed.target,
            ProgressTarget::DesiredRevision(receipt.revision)
        );
        drop(progress);
        assert_eq!(control.snapshot().await.unwrap().revision, receipt.revision);

        let unchanged_plan = control.plan(desired.clone()).await.unwrap();
        let unchanged = control
            .apply(ApplyResourcesRequest {
                mutation_id: MutationId::from_bytes([0x32; 16]),
                base_revision: unchanged_plan.base_revision,
                expected_desired_digest: unchanged_plan.desired_digest,
                declarations: desired,
                credential_material: Vec::new(),
            })
            .await
            .unwrap();
        assert!(!unchanged.changed);
        assert_eq!(unchanged.revision, receipt.revision);

        let empty = ResourceDeclarations {
            api_version: API_VERSION.to_owned(),
            resources: Vec::new(),
        };
        let empty_digest = empty.clone().normalize().unwrap().digest();
        let stale_error = control
            .apply(ApplyResourcesRequest {
                mutation_id: MutationId::from_bytes([0x33; 16]),
                base_revision: ResourceRevision::new(0),
                expected_desired_digest: empty_digest,
                declarations: empty,
                credential_material: Vec::new(),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            stale_error,
            ResourceControlError::Apply(ResourceApplyError::StaleRevision { .. })
        ));

        control.shutdown();
        assert!(matches!(
            control.plan(declarations(digest)).await.unwrap_err(),
            ResourceControlError::ShuttingDown
        ));
        state.shutdown().await.unwrap();
    }
}
