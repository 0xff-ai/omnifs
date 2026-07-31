//! CLI client for the daemon's fixed local control endpoint.

use anyhow::Context as _;
use bytes::Bytes;
use hyper_util::rt::TokioIo;
use omnifs_api::grpc::{self, wire};
use omnifs_api::{
    ActionReceipt, ApplyReceipt, ApplyResourcesRequest, AttachmentAccess, AttachmentStatus,
    CONTROL_LOG_TAIL_MAX_LINES, CONTROL_MUTATION_TIMEOUT_SECS, CONTROL_REQUEST_TIMEOUT_SECS,
    CONTROL_SHUTDOWN_TIMEOUT_SECS, CONTROL_STREAM_PAYLOAD_MAX_BYTES, CredentialKey,
    CredentialReceipt, CredentialStatus, DaemonInventory, GetAttachmentAccessRequest, MountRecord,
    MutationOp, MutationOpResult, ProgressEvent, ProgressTarget, ProviderImportReceipt,
    ProviderMetadata, ResourceDeclarations, ResourcePlan, ResourceSnapshot,
    RestartAttachmentRequest, RevokeCredentialRequest, ServingOutcome,
    SetCredentialMaterialRequest,
};
use omnifs_bootstrap::Profile;
use omnifs_core::{MountName, MutationId, ProviderId, ResourceName};
use prost::Message as _;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::OnceCell;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request};

use crate::error::{ExitCode, WithExitCode, WithHint};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS);
const MUTATION_TIMEOUT: Duration = Duration::from_secs(CONTROL_MUTATION_TIMEOUT_SECS);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(CONTROL_SHUTDOWN_TIMEOUT_SECS);
const PROVIDER_IMPORT_TIMEOUT: Duration = Duration::from_mins(3);

fn unary<T>(message: T, timeout: Duration) -> Request<T> {
    let mut request = Request::new(message);
    request.set_timeout(timeout);
    request
}

macro_rules! bounded_unary {
    ($client:expr, $method:ident, $request:expr) => {{ bounded_unary!($client, $method, $request, REQUEST_TIMEOUT) }};
    ($client:expr, $method:ident, $request:expr, $timeout:expr) => {{
        let timeout = $timeout;
        tokio::time::timeout(timeout, async {
            $client
                .client()
                .await?
                .$method(unary($request, timeout))
                .await
                .map_err(status_error)
        })
        .await
        .context("daemon control request timed out")
        .with_exit_code(ExitCode::DaemonUnavailable)??
    }};
}

#[derive(Debug, Clone)]
pub(crate) struct ShutdownResult {
    pub(crate) detached: usize,
    pub(crate) still_attached: Vec<String>,
}

pub(crate) struct ProgressWatch {
    first: Option<ProgressEvent>,
    stream: tonic::Streaming<wire::ProgressEvent>,
}

impl ProgressWatch {
    pub(crate) async fn next(&mut self) -> anyhow::Result<Option<ProgressEvent>> {
        if let Some(first) = self.first.take() {
            return Ok(Some(first));
        }
        self.stream
            .message()
            .await
            .map_err(status_error)?
            .as_ref()
            .map(grpc::progress_event)
            .transpose()
            .map_err(Into::into)
    }
}

pub(crate) struct RpcClient {
    endpoint: std::path::PathBuf,
    channel: OnceCell<Channel>,
    setup_timeout: Duration,
}

type ControlClient = wire::control_client::ControlClient<Channel>;

impl RpcClient {
    pub(crate) fn from_endpoint(endpoint: std::path::PathBuf) -> Self {
        Self::with_setup_timeout(endpoint, REQUEST_TIMEOUT)
    }

    fn with_setup_timeout(endpoint: std::path::PathBuf, setup_timeout: Duration) -> Self {
        Self {
            endpoint,
            channel: OnceCell::new(),
            setup_timeout,
        }
    }

    pub(crate) fn resolve() -> anyhow::Result<Self> {
        Ok(Self {
            endpoint: Profile::resolve()?.control_socket(),
            channel: OnceCell::new(),
            setup_timeout: REQUEST_TIMEOUT,
        })
    }

    pub(crate) async fn get_mount(&self, name: MountName) -> anyhow::Result<Option<MountRecord>> {
        let response = bounded_unary!(
            self,
            get_mount,
            wire::GetMountRequest {
                name: name.as_str().to_owned(),
            }
        )
        .into_inner();
        response
            .mount
            .as_ref()
            .map(grpc::mount_record)
            .transpose()
            .map_err(Into::into)
    }

    pub(crate) async fn inventory(&self) -> anyhow::Result<DaemonInventory> {
        let response = bounded_unary!(self, get_inventory, wire::Empty {}).into_inner();
        grpc::daemon_inventory(response.inventory.as_ref().context("missing inventory")?)
            .map_err(Into::into)
    }

    pub(crate) async fn ready(&self) -> anyhow::Result<()> {
        bounded_unary!(self, ready, wire::Empty {});
        Ok(())
    }

    pub(crate) async fn status_optional(&self) -> anyhow::Result<Option<omnifs_api::DaemonStatus>> {
        if !self.endpoint.exists() {
            return Ok(None);
        }
        let response = bounded_unary!(self, get_status, wire::Empty {}).into_inner();
        grpc::daemon_status(response.status.as_ref().context("missing daemon status")?)
            .map(Some)
            .map_err(Into::into)
    }

    pub(crate) async fn shutdown(
        &self,
        stop_filesystems: bool,
    ) -> anyhow::Result<Option<ShutdownResult>> {
        if !self.endpoint.exists() {
            return Ok(None);
        }
        let response = bounded_unary!(
            self,
            shutdown,
            wire::ShutdownRequest { stop_filesystems },
            SHUTDOWN_TIMEOUT
        )
        .into_inner();
        Ok(Some(ShutdownResult {
            detached: response.detached as usize,
            still_attached: response.still_attached,
        }))
    }

    /// Open the daemon-owned log stream.
    pub(crate) async fn stream_logs(
        &self,
        follow: bool,
        tail_lines: u32,
    ) -> anyhow::Result<tonic::Streaming<wire::LogStreamItem>> {
        anyhow::ensure!(
            (1..=CONTROL_LOG_TAIL_MAX_LINES).contains(&tail_lines),
            "tail_lines must be between 1 and {CONTROL_LOG_TAIL_MAX_LINES}"
        );
        tokio::time::timeout(self.setup_timeout, async {
            let response = self
                .client()
                .await?
                .stream_logs(Request::new(wire::StreamLogsRequest { follow, tail_lines }))
                .await
                .map_err(status_error)?;
            let mut stream = response.into_inner();
            let first = stream
                .message()
                .await?
                .context("daemon closed log stream before ready")?;
            anyhow::ensure!(
                matches!(first.value, Some(wire::log_stream_item::Value::Ready(_))),
                "daemon returned an invalid log stream prelude"
            );
            Ok(stream)
        })
        .await
        .context("timed out starting daemon log stream")
        .with_exit_code(ExitCode::DaemonUnavailable)?
    }

    pub(crate) async fn list_mounts(&self) -> anyhow::Result<Vec<MountRecord>> {
        let response = bounded_unary!(self, list_mounts, wire::Empty {}).into_inner();
        response
            .mounts
            .iter()
            .map(grpc::mount_record)
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    pub(crate) async fn provider_metadata(
        &self,
        id: ProviderId,
    ) -> anyhow::Result<Option<ProviderMetadata>> {
        let response = bounded_unary!(
            self,
            get_provider_metadata,
            wire::GetProviderMetadataRequest {
                provider_id: id.as_bytes().to_vec().into(),
            }
        )
        .into_inner();
        response
            .metadata
            .as_ref()
            .map(grpc::provider_metadata)
            .transpose()
            .map_err(Into::into)
    }

    pub(crate) async fn list_providers(&self) -> anyhow::Result<Vec<ProviderMetadata>> {
        let response = bounded_unary!(self, list_providers, wire::Empty {}).into_inner();
        retained_provider_metadata(&response.providers)
    }

    pub(crate) async fn list_embedded_providers(&self) -> anyhow::Result<Vec<ProviderMetadata>> {
        let response = bounded_unary!(self, list_providers, wire::Empty {}).into_inner();
        response
            .providers
            .iter()
            .map(grpc::provider_entry)
            .collect::<Result<Vec<_>, _>>()
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|(metadata, embedded, _)| embedded.then_some(metadata))
                    .collect()
            })
            .map_err(Into::into)
    }

    /// Upload one exact provider artifact as a gRPC client stream. The stream
    /// carries bounded chunks, then one terminal reply. Import carries no
    /// mutation identity: the daemon dedupes by content digest, so a dropped
    /// stream can simply be retried.
    pub(crate) async fn import_provider(
        &self,
        file_name: String,
        bytes: &[u8],
    ) -> anyhow::Result<ProviderImportReceipt> {
        let total_length = u64::try_from(bytes.len()).context("provider artifact is too large")?;
        anyhow::ensure!(
            !bytes.is_empty(),
            "provider artifact must contain at least one byte"
        );
        let digest = ProviderId::from_wasm_bytes(bytes);
        tokio::time::timeout(PROVIDER_IMPORT_TIMEOUT, async {
            let owned = Bytes::copy_from_slice(bytes);
            let start = grpc::to_provider_upload_start(&file_name, total_length, &digest);
            let mut requests = Vec::new();
            requests.push(wire::ImportProviderRequest {
                value: Some(wire::import_provider_request::Value::Start(start)),
            });
            for offset in (0..owned.len()).step_by(CONTROL_STREAM_PAYLOAD_MAX_BYTES) {
                let end = (offset + CONTROL_STREAM_PAYLOAD_MAX_BYTES).min(owned.len());
                requests.push(wire::ImportProviderRequest {
                    value: Some(wire::import_provider_request::Value::Chunk(
                        owned.slice(offset..end),
                    )),
                });
            }
            let response = self
                .client()
                .await?
                .import_provider(Request::new(tokio_stream::iter(requests)))
                .await
                .map_err(status_error)?
                .into_inner();
            grpc::provider_import_receipt(
                response
                    .receipt
                    .as_ref()
                    .context("missing provider receipt")?,
            )
            .map_err(anyhow::Error::from)
        })
        .await
        .with_context(|| {
            format!(
                "provider import to {} timed out after {} seconds",
                self.endpoint.display(),
                PROVIDER_IMPORT_TIMEOUT.as_secs()
            )
        })?
    }

    pub(crate) async fn import_embedded_provider(
        &self,
        name: String,
    ) -> anyhow::Result<ProviderImportReceipt> {
        let response = bounded_unary!(
            self,
            import_embedded_provider,
            wire::ImportEmbeddedProviderRequest { name },
            MUTATION_TIMEOUT
        )
        .into_inner();
        grpc::provider_import_receipt(
            response
                .receipt
                .as_ref()
                .context("missing provider receipt")?,
        )
        .map_err(anyhow::Error::from)
    }

    pub(crate) async fn credential_status(
        &self,
        key: CredentialKey,
    ) -> anyhow::Result<Option<CredentialStatus>> {
        let response = bounded_unary!(
            self,
            get_credential_status,
            wire::GetCredentialStatusRequest {
                key: Some(grpc::to_credential_key(&key)),
            }
        )
        .into_inner();
        response
            .status
            .as_ref()
            .map(grpc::credential_status)
            .transpose()
            .map_err(Into::into)
    }

    pub(crate) async fn list_credentials(&self) -> anyhow::Result<Vec<CredentialStatus>> {
        let response = bounded_unary!(self, list_credentials, wire::Empty {}).into_inner();
        response
            .credentials
            .iter()
            .map(grpc::credential_status)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    #[allow(dead_code, reason = "Plan 007 declarative commands consume this RPC")]
    pub(crate) async fn resources(&self) -> anyhow::Result<ResourceSnapshot> {
        let response = bounded_unary!(self, get_resources, wire::Empty {}).into_inner();
        grpc::get_resources_response(&response).map_err(Into::into)
    }

    pub(crate) async fn plan_resources(
        &self,
        declarations: &ResourceDeclarations,
    ) -> anyhow::Result<ResourcePlan> {
        let response = bounded_unary!(
            self,
            plan_resources,
            grpc::to_plan_resources_request(declarations)
        )
        .into_inner();
        grpc::plan_resources_response(&response).map_err(Into::into)
    }

    pub(crate) async fn apply_resources(
        &self,
        request: &ApplyResourcesRequest,
    ) -> anyhow::Result<ApplyReceipt> {
        let response = bounded_unary!(
            self,
            apply_resources,
            grpc::to_apply_resources_request(request)
        )
        .into_inner();
        grpc::apply_resources_response(&response).map_err(Into::into)
    }

    #[allow(dead_code, reason = "Plan 008 attachment commands consume this RPC")]
    pub(crate) async fn attachment_status(
        &self,
        name: ResourceName,
    ) -> anyhow::Result<Option<AttachmentStatus>> {
        let response = bounded_unary!(
            self,
            get_attachment_status,
            wire::GetAttachmentStatusRequest {
                attachment_name: name.to_string(),
            }
        )
        .into_inner();
        grpc::get_attachment_status_response(&response).map_err(Into::into)
    }

    #[allow(dead_code, reason = "Plan 008 attachment commands consume this RPC")]
    pub(crate) async fn restart_attachment(
        &self,
        request: &RestartAttachmentRequest,
    ) -> anyhow::Result<ActionReceipt> {
        let response = bounded_unary!(
            self,
            restart_attachment,
            grpc::to_restart_attachment_request(request)
        )
        .into_inner();
        grpc::restart_attachment_response(&response).map_err(Into::into)
    }

    #[allow(dead_code, reason = "Plan 008 attachment commands consume this RPC")]
    pub(crate) async fn attachment_access(
        &self,
        request: &GetAttachmentAccessRequest,
    ) -> anyhow::Result<AttachmentAccess> {
        let response = bounded_unary!(
            self,
            get_attachment_access,
            grpc::to_get_attachment_access_request(request)
        )
        .into_inner();
        grpc::get_attachment_access_response(&response).map_err(Into::into)
    }

    #[allow(dead_code, reason = "Plan 008 credential commands consume this RPC")]
    pub(crate) async fn set_credential_material(
        &self,
        request: &SetCredentialMaterialRequest,
    ) -> anyhow::Result<CredentialReceipt> {
        let response = bounded_unary!(
            self,
            set_credential_material,
            grpc::to_set_credential_material_request(request)
        )
        .into_inner();
        grpc::set_credential_material_response(&response).map_err(Into::into)
    }

    #[allow(dead_code, reason = "Plan 008 credential commands consume this RPC")]
    pub(crate) async fn revoke_credential(
        &self,
        request: &RevokeCredentialRequest,
    ) -> anyhow::Result<CredentialReceipt> {
        let response = bounded_unary!(
            self,
            revoke_credential,
            grpc::to_revoke_credential_request(request)
        )
        .into_inner();
        grpc::revoke_credential_response(&response).map_err(Into::into)
    }

    /// Open one target-scoped progress stream. Only stream setup and the
    /// required first snapshot use the request deadline. Once subscribed,
    /// reconciliation can run for as long as it needs.
    pub(crate) async fn watch_progress(
        &self,
        target: ProgressTarget,
    ) -> anyhow::Result<ProgressWatch> {
        tokio::time::timeout(self.setup_timeout, async {
            let response = self
                .client()
                .await?
                .watch_progress(Request::new(grpc::to_progress_target(target)))
                .await
                .map_err(status_error)?;
            let mut stream = response.into_inner();
            let first_wire = stream
                .message()
                .await
                .map_err(status_error)?
                .context("daemon closed progress stream before snapshot")?;
            let first = grpc::progress_event(&first_wire)?;
            anyhow::ensure!(
                matches!(first.event, omnifs_api::ProgressEventKind::Snapshot(_)),
                "daemon returned an invalid progress stream prelude"
            );
            Ok(ProgressWatch {
                first: Some(first),
                stream,
            })
        })
        .await
        .context("timed out starting daemon progress stream")
        .with_exit_code(ExitCode::DaemonUnavailable)?
    }

    /// Acquire the daemon's single mutation lease for `id`. Rejects with a
    /// `MutationInProgress`/`LeaseExpired`/`LeaseNotHeld`-coded error (message
    /// already names the holder and deadline where relevant) if another
    /// batch holds it.
    pub(crate) async fn begin_mutation(&self, id: MutationId) -> anyhow::Result<u64> {
        let response = bounded_unary!(
            self,
            begin_mutation,
            wire::BeginMutationRequest {
                mutation_id: id.as_bytes().to_vec().into(),
            },
            MUTATION_TIMEOUT
        )
        .into_inner();
        Ok(response.lease_deadline_unix_ms)
    }

    /// Apply a batch of ops under the lease `id` holds. Returns only after the
    /// daemon commits the batch and reports whether the serving generation
    /// reflects it.
    pub(crate) async fn apply_mutation(
        &self,
        id: MutationId,
        ops: Vec<MutationOp>,
    ) -> anyhow::Result<(Vec<MutationOpResult>, ServingOutcome)> {
        let wire_ops = ops.iter().map(grpc::to_mutation_op).collect();
        let response = bounded_unary!(
            self,
            apply_mutation,
            wire::ApplyMutationRequest {
                mutation_id: id.as_bytes().to_vec().into(),
                ops: wire_ops,
            },
            MUTATION_TIMEOUT
        )
        .into_inner();
        let results = response
            .results
            .iter()
            .map(grpc::mutation_op_result)
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::from)?;
        let serving = response
            .serving
            .as_ref()
            .map(grpc::serving_outcome)
            .context("missing serving outcome")?;
        Ok((results, serving))
    }

    /// Release the lease if `id` holds it. Idempotent: an id that does not
    /// hold the lease (already applied, or never begun) is a no-op.
    pub(crate) async fn drop_mutation(&self, id: MutationId) -> anyhow::Result<()> {
        bounded_unary!(
            self,
            drop_mutation,
            wire::DropMutationRequest {
                mutation_id: id.as_bytes().to_vec().into(),
            },
            MUTATION_TIMEOUT
        );
        Ok(())
    }

    async fn client(&self) -> anyhow::Result<ControlClient> {
        let socket = self.endpoint.clone();
        let socket_display = socket.display().to_string();
        let setup_timeout = self.setup_timeout;
        let channel = self
            .channel
            .get_or_try_init(|| async move {
                let endpoint =
                    Endpoint::from_static("http://[::]:50051").connect_timeout(setup_timeout);
                let connected = tokio::time::timeout(
                    setup_timeout,
                    endpoint.connect_with_connector(tower::service_fn(move |_| {
                        let socket = socket.clone();
                        async move { UnixStream::connect(socket).await.map(TokioIo::new) }
                    })),
                )
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "connect to daemon at {socket_display} timed out after {setup_timeout:?}"
                    )
                })?
                .map_err(|error| connect_error(&error))
                .with_context(|| format!("connect to daemon at {socket_display}"))?;
                Ok::<_, anyhow::Error>(connected)
            })
            .await
            .context("daemon not running")
            .with_exit_code(ExitCode::DaemonUnavailable)?;
        Ok(ControlClient::new(channel.clone()))
    }
}

fn connect_error(error: &tonic::transport::Error) -> anyhow::Error {
    let mut cause: &(dyn std::error::Error + 'static) = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    anyhow::anyhow!("{cause}")
}

fn retained_provider_metadata(
    entries: &[wire::ProviderEntry],
) -> anyhow::Result<Vec<ProviderMetadata>> {
    entries
        .iter()
        .map(grpc::provider_entry)
        .collect::<Result<Vec<_>, _>>()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|(metadata, _, retained)| retained.then_some(metadata))
                .collect()
        })
        .map_err(Into::into)
}

/// The human top-level sentence for a lease-conflict error code, or `None`
/// for every other code. The daemon's own message (holder id, raw deadline)
/// is never parsed or thrown away; it stays one step down the chain via
/// `.context`, and `omnifs status` is the command that actually renders it
/// nicely (holder id, seconds remaining), which is why the caller attaches
/// that as the `Try:` hint rather than repeating the detail here.
fn lease_conflict_sentence(code: omnifs_api::ControlErrorCode) -> Option<&'static str> {
    match code {
        omnifs_api::ControlErrorCode::MutationInProgress => {
            Some("another omnifs command is applying changes right now")
        },
        omnifs_api::ControlErrorCode::LeaseExpired => {
            Some("this command's change timed out before it could finish")
        },
        omnifs_api::ControlErrorCode::LeaseNotHeld => {
            Some("this command no longer holds the slot needed to apply changes")
        },
        _ => None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn status_error(status: tonic::Status) -> anyhow::Error {
    if let Ok(detail) = wire::ErrorDetail::decode(status.details())
        && let Ok(error) = grpc::error_detail(&detail)
    {
        let code = match error.code {
            omnifs_api::ControlErrorCode::Busy
            | omnifs_api::ControlErrorCode::NotReady
            | omnifs_api::ControlErrorCode::RecoveryRequired
            | omnifs_api::ControlErrorCode::MutationInProgress
            | omnifs_api::ControlErrorCode::LeaseExpired
            | omnifs_api::ControlErrorCode::LeaseNotHeld => ExitCode::DaemonUnavailable,
            _ => ExitCode::GenericFailure,
        };
        let result: anyhow::Result<()> = match lease_conflict_sentence(error.code) {
            Some(sentence) => {
                Err(anyhow::anyhow!(error.message).context(sentence)).with_hint("omnifs status")
            },
            None => Err(anyhow::anyhow!(error.message)),
        };
        return WithExitCode::<()>::with_exit_code(result, code)
            .expect_err("status error starts from Err");
    }
    let error = anyhow::anyhow!("daemon control request failed: {}", status.message());
    match status.code() {
        Code::ResourceExhausted | Code::Unavailable | Code::DeadlineExceeded => {
            let result: anyhow::Result<()> = Err(error).with_hint("omnifs logs");
            WithExitCode::<()>::with_exit_code(result, ExitCode::DaemonUnavailable)
                .expect_err("status error starts from Err")
        },
        _ => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::exit_code;

    #[test]
    fn mutation_in_progress_humanizes_the_top_level_message_and_keeps_the_daemon_detail() {
        let daemon_message = "mutation deadbeef is already in progress; its lease expires at 123";
        let detail = wire::ErrorDetail {
            code: wire::ErrorCode::MutationInProgress as i32,
            message: daemon_message.into(),
        };
        let status = tonic::Status::with_details(
            Code::Internal,
            "fallback message",
            detail.encode_to_vec().into(),
        );
        let error = status_error(status);
        // The top-level message is a human sentence, not the daemon's raw
        // hex-id-and-unix-ms wire message.
        assert_eq!(
            error.to_string(),
            "another omnifs command is applying changes right now"
        );
        // Nothing is lost: the daemon's own message (holder id, deadline)
        // still rides in the chain for anyone who reads the full transcript.
        assert!(
            crate::error::message_chain(&error).contains(&daemon_message.to_owned()),
            "{:?}",
            crate::error::message_chain(&error)
        );
        assert_eq!(
            crate::error::hints(&error),
            vec!["omnifs status".to_owned()]
        );
        assert_eq!(exit_code(&error), ExitCode::DaemonUnavailable);
    }

    #[test]
    fn lease_expired_and_lease_not_held_also_get_a_human_sentence_and_the_status_hint() {
        for (code, expected) in [
            (
                wire::ErrorCode::LeaseExpired,
                "this command's change timed out before it could finish",
            ),
            (
                wire::ErrorCode::LeaseNotHeld,
                "this command no longer holds the slot needed to apply changes",
            ),
        ] {
            let detail = wire::ErrorDetail {
                code: code as i32,
                message: "daemon detail".into(),
            };
            let status = tonic::Status::with_details(
                Code::Internal,
                "fallback",
                detail.encode_to_vec().into(),
            );
            let error = status_error(status);
            assert_eq!(error.to_string(), expected);
            assert_eq!(
                crate::error::hints(&error),
                vec!["omnifs status".to_owned()]
            );
            assert_eq!(exit_code(&error), ExitCode::DaemonUnavailable);
        }
    }

    #[test]
    fn structured_error_detail_wins_over_tonic_code_fallback() {
        let detail = wire::ErrorDetail {
            code: wire::ErrorCode::Busy as i32,
            message: "busy detail".into(),
        };
        let status = tonic::Status::with_details(
            Code::Internal,
            "fallback message",
            detail.encode_to_vec().into(),
        );
        let error = status_error(status);

        assert_eq!(error.to_string(), "busy detail");
        assert_eq!(exit_code(&error), ExitCode::DaemonUnavailable);
    }

    #[test]
    fn transient_tonic_codes_map_to_daemon_unavailable_with_a_logs_hint() {
        for code in [
            Code::ResourceExhausted,
            Code::Unavailable,
            Code::DeadlineExceeded,
        ] {
            let error = status_error(tonic::Status::new(code, "transient"));
            assert_eq!(exit_code(&error), ExitCode::DaemonUnavailable);
            assert_eq!(crate::error::hints(&error), vec!["omnifs logs".to_owned()]);
        }
    }

    #[test]
    fn list_providers_keeps_retained_rows_and_excludes_embedded_only_rows() {
        let metadata = |byte| wire::ProviderMetadata {
            reference: Some(wire::ProviderReference {
                id: vec![byte; 32].into(),
                name: format!("provider-{byte}"),
                version: None,
            }),
            manifest: br"{}".to_vec().into(),
        };
        let entries = vec![
            wire::ProviderEntry {
                metadata: Some(metadata(1)),
                embedded: false,
                retained: true,
            },
            wire::ProviderEntry {
                metadata: Some(metadata(2)),
                embedded: true,
                retained: false,
            },
            wire::ProviderEntry {
                metadata: Some(metadata(3)),
                embedded: true,
                retained: true,
            },
        ];

        let providers = retained_provider_metadata(&entries).unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].reference.id.as_bytes(), &[1; 32]);
        assert_eq!(providers[1].reference.id.as_bytes(), &[3; 32]);
    }

    #[tokio::test]
    async fn stalled_rpc_setup_respects_setup_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = Profile::under_root(dir.path()).control_socket();
        let listener = tokio::net::UnixListener::bind(&endpoint).expect("bind control socket");
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept client");
            std::future::pending::<()>().await;
        });

        let rpc = RpcClient::with_setup_timeout(endpoint, Duration::from_millis(20));
        let result = tokio::time::timeout(Duration::from_secs(1), rpc.stream_logs(false, 1)).await;
        let error = match result {
            Ok(Err(error)) => error,
            Ok(Ok(_)) => panic!("RPC setup unexpectedly succeeded"),
            Err(elapsed) => panic!("outer test timeout elapsed: {elapsed}"),
        };
        assert!(
            error
                .to_string()
                .contains("timed out starting daemon log stream")
        );
    }

    #[tokio::test]
    async fn stalled_progress_setup_respects_setup_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = Profile::under_root(dir.path()).control_socket();
        let listener = tokio::net::UnixListener::bind(&endpoint).expect("bind control socket");
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept client");
            std::future::pending::<()>().await;
        });

        let rpc = RpcClient::with_setup_timeout(endpoint, Duration::from_millis(20));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            rpc.watch_progress(ProgressTarget::Current),
        )
        .await;
        let error = match result {
            Ok(Err(error)) => error,
            Ok(Ok(_)) => panic!("progress RPC setup unexpectedly succeeded"),
            Err(elapsed) => panic!("outer test timeout elapsed: {elapsed}"),
        };
        assert!(
            error
                .to_string()
                .contains("timed out starting daemon progress stream")
        );
    }
}
