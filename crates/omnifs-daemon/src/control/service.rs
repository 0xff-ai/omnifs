//! The gRPC control surface: one method per control-plane operation.

use super::mapping::{
    api_credential_status, api_provider_import_disposition, api_provider_metadata,
    api_provider_reference, credential_id,
};
use super::*;
use tokio_stream::StreamExt as _;

const PROVIDER_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);
pub(crate) struct GrpcControlService {
    control: Arc<ControlServer>,
}

impl GrpcControlService {
    pub(crate) fn new(control: Arc<ControlServer>) -> Self {
        Self { control }
    }

    fn phase(&self) -> ControlPhase {
        self.control.phase()
    }

    fn daemon(&self) -> Result<Arc<Daemon>, Status> {
        self.phase().ready().map_err(grpc_status)
    }

    async fn recovery(&self) -> Result<DaemonRecovery, Status> {
        match self.phase() {
            ControlPhase::Starting => Ok(DaemonRecovery {
                phase: DaemonPhase::Starting,
                durable_revision: None,
                serving_revision: None,
                store_health: HealthReport::new(HealthState::Degraded, "daemon is starting"),
                repair: None,
            }),
            ControlPhase::Recovery(recovery) => Ok(recovery),
            ControlPhase::Ready(daemon) => daemon.recovery().await.map_err(grpc_internal),
            ControlPhase::ShuttingDown => Err(grpc_status(ControlPhase::shutting_down())),
        }
    }

    async fn inventory(&self) -> Result<DaemonInventory, Status> {
        match self.phase() {
            ControlPhase::Starting => Ok(DaemonInventory {
                info: self.control.context.daemon_info(None, None),
                phase: DaemonPhase::Starting,
                durable_revision: None,
                serving_revision: None,
                health: DaemonHealth::new(
                    HealthReport::new(HealthState::Healthy, "control socket available"),
                    HealthReport::new(HealthState::Starting, "namespace endpoints unavailable"),
                    HealthReport::new(HealthState::Degraded, "daemon is starting"),
                ),
                mounts: Vec::new(),
                credentials: Vec::new(),
                filesystems: Vec::new(),
            }),
            ControlPhase::Recovery(recovery) => Ok(DaemonInventory {
                info: self.control.context.daemon_info(None, None),
                phase: recovery.phase,
                durable_revision: recovery.durable_revision,
                serving_revision: recovery.serving_revision,
                health: DaemonHealth::new(
                    HealthReport::new(HealthState::Healthy, "control socket available"),
                    HealthReport::new(HealthState::Unhealthy, "namespace endpoints unavailable"),
                    recovery.store_health,
                ),
                mounts: Vec::new(),
                credentials: Vec::new(),
                filesystems: Vec::new(),
            }),
            ControlPhase::Ready(daemon) => daemon.inventory().await.map_err(grpc_internal),
            ControlPhase::ShuttingDown => Err(grpc_status(ControlPhase::shutting_down())),
        }
    }

    fn info(&self) -> DaemonInfo {
        let (attach_unix, attach_tcp) = match self.phase() {
            ControlPhase::Ready(daemon) => {
                (Some(daemon.context.attach_socket()), daemon.attach_tcp())
            },
            ControlPhase::Starting | ControlPhase::Recovery(_) | ControlPhase::ShuttingDown => {
                (None, None)
            },
        };
        self.control.context.daemon_info(attach_unix, attach_tcp)
    }
}

/// Read one provider upload stream to completion and hand back the staged,
/// validated artifact. No mutation identity is involved: content-digest
/// dedup inside the durable write is the only idempotency layer.
async fn import_provider_inner(
    daemon: &Daemon,
    request: Request<tonic::Streaming<wire::ImportProviderRequest>>,
) -> Result<omnifs_state::ValidatedProviderUpload, Status> {
    let mut stream = request.into_inner();
    let first = stream
        .message()
        .await
        .map_err(grpc_invalid)?
        .ok_or_else(|| grpc_invalid("provider upload is missing start"))?;
    let start = match first.value {
        Some(wire::import_provider_request::Value::Start(start)) => start,
        Some(wire::import_provider_request::Value::Chunk(_)) => {
            return Err(grpc_invalid("provider upload chunk arrived before start"));
        },
        None => return Err(grpc_invalid("provider upload item is empty")),
    };
    let digest =
        omnifs_core::ProviderId::from_digest(exact_bytes(&start.digest, "provider digest")?);
    let mut upload = daemon
        .state
        .begin_provider_upload(start.file_name.clone(), digest, start.total_length)
        .await
        .map_err(grpc_invalid)?;
    let mut received = 0_u64;
    while let Some(item) = stream.message().await.map_err(grpc_invalid)? {
        let chunk = match item.value {
            Some(wire::import_provider_request::Value::Chunk(chunk)) => chunk,
            Some(wire::import_provider_request::Value::Start(_)) => {
                return Err(grpc_invalid("provider upload contains duplicate start"));
            },
            None => return Err(grpc_invalid("provider upload item is empty")),
        };
        if chunk.is_empty() {
            return Err(grpc_invalid("provider upload chunk is empty"));
        }
        received = received
            .checked_add(u64::try_from(chunk.len()).map_err(grpc_invalid)?)
            .ok_or_else(|| grpc_invalid("provider upload length overflow"))?;
        if received > start.total_length {
            return Err(grpc_invalid("provider upload exceeds declared length"));
        }
        upload.write_chunk(&chunk).await.map_err(grpc_invalid)?;
    }
    if received != start.total_length {
        return Err(grpc_invalid(
            "provider upload is shorter than declared length",
        ));
    }
    upload.finish().await.map_err(grpc_invalid)
}

/// A repaired artifact may be pinned by a mount that has been degraded
/// (missing/corrupt blob) since it started serving; drive one rebuild
/// through the manager's single-writer path so that mount recovers
/// immediately instead of waiting for an unrelated mutation or a daemon
/// restart. An `Inserted` artifact cannot yet be pinned by an existing
/// mount, and an `Unchanged` re-import repaired nothing, so only
/// `Repaired` warrants this. Best-effort: the import itself already
/// committed, so a rebuild failure is logged rather than failing the RPC.
fn provider_import_response(
    outcome: omnifs_state::ProviderImportOutcome,
) -> wire::ImportProviderResponse {
    let receipt = ProviderImportReceipt {
        provider: api_provider_reference(outcome.reference),
        disposition: api_provider_import_disposition(outcome.disposition),
    };
    wire::ImportProviderResponse {
        receipt: Some(grpc::to_provider_import_receipt(&receipt)),
    }
}

#[allow(clippy::too_many_lines)]
#[tonic::async_trait]
impl wire::control_server::Control for GrpcControlService {
    async fn ready(&self, _request: Request<wire::Empty>) -> Result<Response<wire::Empty>, Status> {
        let daemon = self.daemon()?;
        if !daemon.vfs.ready() {
            return Err(grpc_status(ControlError::new(
                ControlErrorCode::NotReady,
                "namespace listeners are not serving yet",
            )));
        }
        Ok(Response::new(wire::Empty {}))
    }

    async fn get_status(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::StatusResponse>, Status> {
        let daemon = self.daemon()?;
        Ok(Response::new(wire::StatusResponse {
            status: Some(grpc::to_daemon_status(&daemon.control_status())),
        }))
    }

    async fn get_inventory(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::InventoryResponse>, Status> {
        Ok(Response::new(wire::InventoryResponse {
            inventory: Some(grpc::to_daemon_inventory(&self.inventory().await?)),
        }))
    }

    async fn get_daemon_info(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::DaemonInfoResponse>, Status> {
        Ok(Response::new(wire::DaemonInfoResponse {
            info: Some(grpc::to_daemon_info(&self.info())),
        }))
    }

    async fn get_recovery_state(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::RecoveryResponse>, Status> {
        Ok(Response::new(wire::RecoveryResponse {
            recovery: Some(grpc::to_daemon_recovery(&self.recovery().await?)),
        }))
    }

    async fn run_doctor(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::RunDoctorResponse>, Status> {
        let daemon = self.daemon()?;
        let report = daemon.run_doctor().await.map_err(grpc_internal)?;
        Ok(Response::new(grpc::to_run_doctor_response(&report)))
    }

    async fn apply_doctor_repairs(
        &self,
        request: Request<wire::ApplyDoctorRepairsRequest>,
    ) -> Result<Response<wire::ApplyDoctorRepairsResponse>, Status> {
        let daemon = self.daemon()?;
        let ids =
            grpc::apply_doctor_repairs_request(&request.into_inner()).map_err(grpc_invalid)?;
        let outcomes = daemon
            .apply_doctor_repairs(ids)
            .await
            .map_err(grpc_internal)?;
        Ok(Response::new(grpc::to_apply_doctor_repairs_response(
            &outcomes,
        )))
    }

    async fn repair_state(
        &self,
        request: Request<wire::RepairStateRequest>,
    ) -> Result<Response<wire::RepairStateResponse>, Status> {
        let request = request.into_inner();
        let action = match wire::RepairAction::try_from(request.action)
            .map_err(|_| grpc_invalid("invalid repair action"))?
        {
            wire::RepairAction::RepairRecreateControlStore => RepairAction::RecreateControlStore,
            wire::RepairAction::Unspecified => {
                return Err(grpc_invalid("repair action unspecified"));
            },
        };
        let recovery_id = RecoveryId::from_bytes(exact_bytes(&request.recovery_id, "recovery id")?);
        let recovery = {
            let mut phase = self
                .control
                .phase
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ControlPhase::Recovery(recovery) = &*phase else {
                return Err(grpc_status(ControlError::new(
                    ControlErrorCode::Conflict,
                    "the daemon is not offering state repair",
                )));
            };
            let valid = request.instance_id == self.control.context.instance_id()
                && recovery.repair.as_ref().is_some_and(|offer| {
                    offer.id == recovery_id && offer.actions.contains(&action)
                });
            if !valid {
                return Err(grpc_status(ControlError::new(
                    ControlErrorCode::Conflict,
                    "the recovery offer is stale or does not match this daemon",
                )));
            }
            let recovery = recovery.clone();
            *phase = ControlPhase::Starting;
            recovery
        };
        let (reply, receive) = tokio::sync::oneshot::channel();
        if self
            .control
            .repair_tx
            .send(RepairCommand {
                recovery_id,
                action,
                reply,
            })
            .await
            .is_err()
        {
            self.control.set_recovery(recovery);
            return Err(grpc_internal("daemon repair worker stopped"));
        }
        let receipt = match receive.await {
            Ok(Ok(receipt)) => receipt,
            Ok(Err(error)) => return Err(grpc_status(error)),
            Err(_) => return Err(grpc_internal("daemon repair worker dropped its reply")),
        };
        Ok(Response::new(wire::RepairStateResponse {
            receipt: Some(grpc::to_repair_receipt(&receipt)),
        }))
    }

    async fn shutdown(
        &self,
        request: Request<wire::ShutdownRequest>,
    ) -> Result<Response<wire::ShutdownResponse>, Status> {
        let stop_filesystems = request.into_inner().stop_filesystems;
        let ControlPhase::Ready(daemon) = self.phase() else {
            let _ = self.control.shutdown_tx.send(true);
            return Ok(Response::new(wire::ShutdownResponse {
                stopped: 0,
                still_running: Vec::new(),
            }));
        };
        daemon.resources.shutdown();
        let supervisor = Arc::clone(daemon.filesystem_supervisor().map_err(grpc_internal)?);
        let vfs = Arc::clone(&daemon.vfs);
        let shutdown_tx = daemon.shutdown_tx.clone();
        let result = tokio::spawn(async move {
            let (stopped, still_running) = if stop_filesystems {
                let before = vfs.sessions().len();
                let still = supervisor.stop_all().await?;
                (
                    before.saturating_sub(still.len()),
                    still.into_iter().map(|name| name.to_string()).collect(),
                )
            } else {
                (0, Vec::new())
            };
            let _ = shutdown_tx.send(true);
            anyhow::Ok((stopped, still_running))
        })
        .await
        .map_err(|error| grpc_internal(error.to_string()))?
        .map_err(grpc_internal)?;
        let (stopped, still_running) = result;
        Ok(Response::new(wire::ShutdownResponse {
            stopped: u32::try_from(stopped).map_err(grpc_internal)?,
            still_running,
        }))
    }

    async fn list_providers(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::ListProvidersResponse>, Status> {
        let daemon = self.daemon()?;
        let retained = daemon.state.list_providers().await.map_err(grpc_internal)?;
        let mut rows = std::collections::BTreeMap::new();
        for provider in retained {
            let metadata = api_provider_metadata(provider);
            rows.insert(*metadata.reference.id.as_bytes(), (metadata, false, true));
        }
        for provider in self.control.embedded.metadata() {
            let key = *provider.reference.id.as_bytes();
            rows.entry(key)
                .and_modify(|row| row.1 = true)
                .or_insert((provider, true, false));
        }
        Ok(Response::new(wire::ListProvidersResponse {
            providers: rows
                .into_values()
                .map(|(metadata, embedded, retained)| wire::ProviderEntry {
                    metadata: Some(grpc::to_provider_metadata(&metadata)),
                    embedded,
                    retained,
                })
                .collect(),
        }))
    }

    async fn get_provider_metadata(
        &self,
        request: Request<wire::GetProviderMetadataRequest>,
    ) -> Result<Response<wire::GetProviderMetadataResponse>, Status> {
        let daemon = self.daemon()?;
        let request = request.into_inner();
        let id =
            omnifs_core::ProviderId::from_digest(exact_bytes(&request.provider_id, "provider id")?);
        let metadata = daemon
            .state
            .load_provider_metadata(id)
            .await
            .map_err(grpc_internal)?
            .map(api_provider_metadata)
            .map(|metadata| grpc::to_provider_metadata(&metadata));
        Ok(Response::new(wire::GetProviderMetadataResponse {
            metadata,
        }))
    }

    async fn import_provider(
        &self,
        request: Request<tonic::Streaming<wire::ImportProviderRequest>>,
    ) -> Result<Response<wire::ImportProviderResponse>, Status> {
        let daemon = self.daemon()?;
        let upload = tokio::time::timeout(
            PROVIDER_UPLOAD_TIMEOUT,
            import_provider_inner(&daemon, request),
        )
        .await
        .map_err(|_| grpc_invalid("provider upload timed out"))??;
        let outcome = daemon
            .state
            .import_provider(upload)
            .await
            .map_err(grpc_internal)?;
        daemon.provider_imported(&outcome);
        Ok(Response::new(provider_import_response(outcome)))
    }

    async fn import_embedded_provider(
        &self,
        request: Request<wire::ImportEmbeddedProviderRequest>,
    ) -> Result<Response<wire::ImportProviderResponse>, Status> {
        let daemon = self.daemon()?;
        let request = request.into_inner();
        let provider = daemon.embedded.by_name(&request.name).ok_or_else(|| {
            grpc_status(ControlError::new(
                ControlErrorCode::NotFound,
                "embedded provider not found",
            ))
        })?;
        let upload = daemon
            .state
            .stage_provider_bytes(
                provider.artifact().file().to_owned(),
                provider.artifact().id(),
                provider.artifact().bytes(),
            )
            .await
            .map_err(grpc_invalid)?;
        let outcome = daemon
            .state
            .import_provider(upload)
            .await
            .map_err(grpc_internal)?;
        daemon.provider_imported(&outcome);
        Ok(Response::new(provider_import_response(outcome)))
    }

    async fn list_credentials(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::ListCredentialsResponse>, Status> {
        let daemon = self.daemon()?;
        let summaries = daemon
            .state
            .list_credentials()
            .await
            .map_err(grpc_internal)?;
        let mut credentials = Vec::with_capacity(summaries.len());
        for summary in summaries {
            credentials.push(
                api_credential_status(&daemon.state, summary)
                    .await
                    .map_err(grpc_internal)?,
            );
        }
        Ok(Response::new(wire::ListCredentialsResponse {
            credentials: credentials.iter().map(grpc::to_credential_status).collect(),
        }))
    }

    async fn get_credential_status(
        &self,
        request: Request<wire::GetCredentialStatusRequest>,
    ) -> Result<Response<wire::GetCredentialStatusResponse>, Status> {
        let daemon = self.daemon()?;
        let key = grpc::credential_key(&required(request.into_inner().key, "credential key")?);
        let id = credential_id(key).map_err(grpc_invalid)?;
        let summary = daemon
            .state
            .list_credentials()
            .await
            .map_err(grpc_internal)?
            .into_iter()
            .find(|summary| summary.id == id);
        let status = match summary {
            Some(summary) => Some(
                api_credential_status(&daemon.state, summary)
                    .await
                    .map_err(grpc_internal)?,
            ),
            None => None,
        };
        Ok(Response::new(wire::GetCredentialStatusResponse {
            status: status.as_ref().map(grpc::to_credential_status),
        }))
    }

    async fn get_resources(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::GetResourcesResponse>, Status> {
        let daemon = self.daemon()?;
        let snapshot = daemon
            .resources
            .snapshot()
            .await
            .map_err(|error| resource_control_status(&error))?;
        Ok(Response::new(wire::GetResourcesResponse {
            snapshot: Some(grpc::to_resource_snapshot(&snapshot)),
        }))
    }

    async fn plan_resources(
        &self,
        request: Request<wire::PlanResourcesRequest>,
    ) -> Result<Response<wire::PlanResourcesResponse>, Status> {
        let daemon = self.daemon()?;
        let declarations = grpc::resource_declarations(&required(
            request.into_inner().declarations,
            "resource declarations",
        )?)
        .map_err(|error| resource_grpc_error(&error))?;
        let plan = daemon
            .resources
            .plan(declarations)
            .await
            .map_err(|error| resource_control_status(&error))?;
        Ok(Response::new(wire::PlanResourcesResponse {
            plan: Some(grpc::to_resource_plan(&plan)),
        }))
    }

    async fn apply_resources(
        &self,
        request: Request<wire::ApplyResourcesRequest>,
    ) -> Result<Response<wire::ApplyResourcesResponse>, Status> {
        let daemon = self.daemon()?;
        let request = grpc::apply_resources_request(&request.into_inner())
            .map_err(|error| resource_grpc_error(&error))?;
        let receipt = daemon
            .resources
            .apply(request)
            .await
            .map_err(|error| resource_control_status(&error))?;
        Ok(Response::new(wire::ApplyResourcesResponse {
            receipt: Some(grpc::to_apply_receipt(&receipt)),
        }))
    }

    async fn set_credential_material(
        &self,
        request: Request<wire::SetCredentialMaterialRequest>,
    ) -> Result<Response<wire::SetCredentialMaterialResponse>, Status> {
        let daemon = self.daemon()?;
        let request =
            grpc::set_credential_material_request(&request.into_inner()).map_err(grpc_invalid)?;
        let receipt = daemon
            .resources
            .set_credential_material(request)
            .await
            .map_err(|error| resource_control_status(&error))?;
        Ok(Response::new(wire::SetCredentialMaterialResponse {
            receipt: Some(grpc::to_credential_receipt(&receipt)),
        }))
    }

    async fn revoke_credential(
        &self,
        request: Request<wire::RevokeCredentialRequest>,
    ) -> Result<Response<wire::RevokeCredentialResponse>, Status> {
        let daemon = self.daemon()?;
        let request =
            grpc::revoke_credential_request(&request.into_inner()).map_err(grpc_invalid)?;
        let receipt = daemon
            .resources
            .revoke_credential(request)
            .await
            .map_err(|error| resource_control_status(&error))?;
        Ok(Response::new(wire::RevokeCredentialResponse {
            receipt: Some(grpc::to_credential_receipt(&receipt)),
        }))
    }

    async fn get_filesystem_status(
        &self,
        request: Request<wire::GetFilesystemStatusRequest>,
    ) -> Result<Response<wire::GetFilesystemStatusResponse>, Status> {
        let daemon = self.daemon()?;
        let name = omnifs_core::ResourceName::new(request.into_inner().filesystem_name)
            .map_err(grpc_invalid)?;
        let status = daemon
            .filesystem_status(&name)
            .await
            .map_err(grpc_internal)?;
        Ok(Response::new(grpc::to_get_filesystem_status_response(
            status.as_ref(),
        )))
    }

    async fn restart_filesystem(
        &self,
        request: Request<wire::RestartFilesystemRequest>,
    ) -> Result<Response<wire::RestartFilesystemResponse>, Status> {
        let daemon = self.daemon()?;
        let request =
            grpc::restart_filesystem_request(&request.into_inner()).map_err(grpc_invalid)?;
        let receipt = daemon
            .resources
            .restart_filesystem(request)
            .await
            .map_err(|error| resource_control_status(&error))?;
        Ok(Response::new(grpc::to_restart_filesystem_response(
            &receipt,
        )))
    }

    async fn get_filesystem_access(
        &self,
        request: Request<wire::GetFilesystemAccessRequest>,
    ) -> Result<Response<wire::GetFilesystemAccessResponse>, Status> {
        let daemon = self.daemon()?;
        let request =
            grpc::get_filesystem_access_request(&request.into_inner()).map_err(grpc_invalid)?;
        let access = daemon
            .filesystem_access(request)
            .await
            .map_err(grpc_status)?;
        Ok(Response::new(grpc::to_get_filesystem_access_response(
            &access,
        )))
    }

    type WatchProgressStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<wire::ProgressEvent, Status>> + Send>>;

    async fn watch_progress(
        &self,
        request: Request<wire::WatchProgressRequest>,
    ) -> Result<Response<Self::WatchProgressStream>, Status> {
        let daemon = self.daemon()?;
        let target = grpc::progress_target(&request.into_inner()).map_err(grpc_invalid)?;
        if daemon.resources.progress().target_state(target)
            == crate::progress::ProgressTargetState::Unavailable
        {
            let code = if matches!(target, omnifs_api::ProgressTarget::Action(_)) {
                ControlErrorCode::ActionUnavailable
            } else {
                ControlErrorCode::NotFound
            };
            return Err(grpc_status(ControlError::new(
                code,
                "progress target is unknown or no longer retained",
            )));
        }
        let permit = Arc::clone(&self.control.stream_permits)
            .try_acquire_owned()
            .map_err(|_| {
                grpc_status(ControlError::new(
                    ControlErrorCode::Busy,
                    "stream capacity exhausted",
                ))
            })?;
        let receive = daemon.resources.progress().subscribe(target);
        let stream = ReceiverStream::new(receive).map(move |event| {
            let _permit = &permit;
            let event = grpc::to_progress_event(&event);
            if event.encoded_len() > omnifs_api::CONTROL_STREAM_ITEM_MAX_BYTES {
                return Err(grpc_status(ControlError::new(
                    ControlErrorCode::PlanTooLarge,
                    "progress snapshot exceeds the control stream item limit",
                )));
            }
            Ok(event)
        });
        Ok(Response::new(Box::pin(stream)))
    }

    type SubscribeInspectorStream = ReceiverStream<Result<wire::InspectorStreamItem, Status>>;

    async fn subscribe_inspector(
        &self,
        _request: Request<wire::InspectorRequest>,
    ) -> Result<Response<Self::SubscribeInspectorStream>, Status> {
        let daemon = self.daemon()?;
        let inspector = daemon.inspector.clone().ok_or_else(|| {
            grpc_status(ControlError::new(
                ControlErrorCode::Internal,
                "inspector stream disabled",
            ))
        })?;
        let permit = Arc::clone(&self.control.stream_permits)
            .try_acquire_owned()
            .map_err(|_| {
                grpc_status(ControlError::new(
                    ControlErrorCode::Busy,
                    "stream capacity exhausted",
                ))
            })?;
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        sender
            .send(Ok(wire::InspectorStreamItem {
                value: Some(wire::inspector_stream_item::Value::Ready(
                    wire::InspectorReady {
                        instance_id: daemon.context.instance_id().to_owned(),
                    },
                )),
            }))
            .await
            .map_err(|_| grpc_internal("inspector client disconnected"))?;
        let subscription = inspector.subscribe();
        let mut live = subscription.live;
        let mut shutdown = self.control.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let _permit = permit;
            for record in subscription.history {
                let Ok(json_line) = serde_json::to_vec(&*record) else {
                    break;
                };
                if sender
                    .send(Ok(wire::InspectorStreamItem {
                        value: Some(wire::inspector_stream_item::Value::JsonLine(
                            json_line.into(),
                        )),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            loop {
                tokio::select! {
                    () = sender.closed() => return,
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { return; }
                    }
                    event = live.recv() => match event {
                        Ok(record) => {
                            let Ok(json_line) = serde_json::to_vec(&*record) else { return };
                            if sender.send(Ok(wire::InspectorStreamItem { value: Some(wire::inspector_stream_item::Value::JsonLine(json_line.into())) })).await.is_err() { return; }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            if sender.send(Ok(wire::InspectorStreamItem { value: Some(wire::inspector_stream_item::Value::Dropped(count)) })).await.is_err() { return; }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    type StreamLogsStream = ReceiverStream<Result<wire::LogStreamItem, Status>>;

    async fn stream_logs(
        &self,
        request: Request<wire::StreamLogsRequest>,
    ) -> Result<Response<Self::StreamLogsStream>, Status> {
        let daemon = self.daemon()?;
        let request = request.into_inner();
        if request.tail_lines == 0 || request.tail_lines > omnifs_api::CONTROL_LOG_TAIL_MAX_LINES {
            return Err(grpc_invalid(format!(
                "tail_lines must be between 1 and {}",
                omnifs_api::CONTROL_LOG_TAIL_MAX_LINES
            )));
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        let permit = Arc::clone(&self.control.stream_permits)
            .try_acquire_owned()
            .map_err(|_| {
                grpc_status(ControlError::new(
                    ControlErrorCode::Busy,
                    "stream capacity exhausted",
                ))
            })?;
        sender
            .send(Ok(wire::LogStreamItem {
                value: Some(wire::log_stream_item::Value::Ready(wire::LogsReady {
                    instance_id: daemon.context.instance_id().to_owned(),
                })),
            }))
            .await
            .map_err(|_| grpc_internal("log client disconnected"))?;
        let shutdown = self.control.shutdown_tx.subscribe();
        let path = daemon.state.daemon_log_path();
        let tail_lines = request.tail_lines as usize;
        let follow = request.follow;
        tokio::spawn(async move {
            let _permit = permit;
            log_stream::stream(path, tail_lines, follow, sender, shutdown).await;
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}
