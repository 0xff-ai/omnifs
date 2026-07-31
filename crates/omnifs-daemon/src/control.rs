//! Typed local control protocol: transport, phase, and error vocabulary.
//!
//! [`ControlServer`] owns the control socket, the uid check, and the
//! connection budget. The RPC surface lives in [`service`]; the durable
//! state to API projection lives in [`mapping`].

pub(crate) mod mapping;
mod service;

use super::context::DaemonContext;
use super::provider_bundle::EmbeddedProviders;
use crate::generation_builder::{ResolvedMount, credential_scopes};
use crate::log_stream;
use anyhow::Context as _;
use bytes::Bytes;
use omnifs_api::grpc::{self, wire};
use omnifs_api::{
    CONTROL_MESSAGE_MAX_BYTES, ControlError, ControlErrorCode, CredentialHealth, CredentialKey,
    CredentialKind, CredentialStatus, CredentialStatusKind, DaemonHealth, DaemonInfo,
    DaemonInventory, DaemonPhase, DaemonRecovery, HealthReport, HealthState,
    MountDefinition as ApiMountDefinition, MountHealth, MountLimits as ApiMountLimits, MountRecord,
    ProviderImportDisposition, ProviderImportReceipt, ProviderMetadata, ProviderReference,
    RecoveryId, RepairAction, RepairReceipt,
};
use omnifs_state::StateStore;
use prost::Message as _;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tower::limit::ConcurrencyLimitLayer;
use tracing::{info, warn};

use crate::daemon::Daemon;
use mapping::resource_control_error;
use service::GrpcControlService;

const CONTROL_CONNECTION_LIMIT: usize = 64;
const CONTROL_PREFACE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const CONTROL_HTTP2_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
#[derive(Clone)]
enum ControlPhase {
    Starting,
    Ready(Arc<Daemon>),
    Recovery(DaemonRecovery),
    ShuttingDown,
}

impl ControlPhase {
    /// The serving runtime, or the error a request gets in this phase.
    fn ready(self) -> Result<Arc<Daemon>, ControlError> {
        match self {
            Self::Ready(daemon) => Ok(daemon),
            Self::Starting => Err(ControlError::new(
                ControlErrorCode::NotReady,
                "daemon is starting",
            )),
            Self::Recovery(_) => Err(ControlError::new(
                ControlErrorCode::RecoveryRequired,
                "daemon recovery is required",
            )),
            Self::ShuttingDown => Err(Self::shutting_down()),
        }
    }

    /// Shared by the accessors that still answer while starting or in
    /// recovery but have nothing to report once teardown has begun.
    fn shutting_down() -> ControlError {
        ControlError::new(ControlErrorCode::NotReady, "daemon is shutting down")
    }
}

pub(crate) struct RepairCommand {
    pub(crate) recovery_id: RecoveryId,
    pub(crate) action: RepairAction,
    pub(crate) reply: tokio::sync::oneshot::Sender<Result<RepairReceipt, ControlError>>,
}

pub(crate) struct ControlServer {
    context: Arc<DaemonContext>,
    embedded: Arc<EmbeddedProviders>,
    phase: RwLock<ControlPhase>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    repair_tx: tokio::sync::mpsc::Sender<RepairCommand>,
    connection_permits: Arc<tokio::sync::Semaphore>,
    stream_permits: Arc<tokio::sync::Semaphore>,
    preface_timeout: std::time::Duration,
}

impl ControlServer {
    pub(crate) fn new(
        context: Arc<DaemonContext>,
        embedded: Arc<EmbeddedProviders>,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
    ) -> (Arc<Self>, tokio::sync::mpsc::Receiver<RepairCommand>) {
        Self::new_with_limits(
            context,
            embedded,
            shutdown_tx,
            CONTROL_CONNECTION_LIMIT,
            CONTROL_PREFACE_TIMEOUT,
        )
    }

    fn new_with_limits(
        context: Arc<DaemonContext>,
        embedded: Arc<EmbeddedProviders>,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
        connection_limit: usize,
        preface_timeout: std::time::Duration,
    ) -> (Arc<Self>, tokio::sync::mpsc::Receiver<RepairCommand>) {
        let (repair_tx, repair_rx) = tokio::sync::mpsc::channel(1);
        (
            Arc::new(Self {
                context,
                embedded,
                phase: RwLock::new(ControlPhase::Starting),
                shutdown_tx,
                repair_tx,
                connection_permits: Arc::new(tokio::sync::Semaphore::new(connection_limit)),
                stream_permits: Arc::new(tokio::sync::Semaphore::new(CONTROL_CONNECTION_LIMIT)),
                preface_timeout,
            }),
            repair_rx,
        )
    }

    pub(crate) fn set_ready(&self, daemon: Arc<Daemon>) {
        *self
            .phase
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ControlPhase::Ready(daemon);
    }

    pub(crate) fn set_recovery(&self, recovery: DaemonRecovery) {
        *self
            .phase
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ControlPhase::Recovery(recovery);
    }

    pub(crate) fn set_shutting_down(&self) {
        *self
            .phase
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ControlPhase::ShuttingDown;
    }

    fn phase(&self) -> ControlPhase {
        self.phase
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) async fn run(
        self: Arc<Self>,
        listener: std::os::unix::net::UnixListener,
    ) -> anyhow::Result<()> {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        info!("control socket listening (filesystem-permission auth)");
        let mut shutdown = self.shutdown_tx.subscribe();
        let control = Arc::clone(&self);
        let listener_error = Arc::new(std::sync::Mutex::new(None::<std::io::Error>));
        let listener_error_for_stream = Arc::clone(&listener_error);
        let (incoming_tx, incoming_rx) =
            tokio::sync::mpsc::channel::<Result<ControlConnection, std::io::Error>>(
                self.connection_permits.available_permits().max(1),
            );
        let connection_permits = Arc::clone(&self.connection_permits);
        let preface_timeout = self.preface_timeout;
        let mut accept_shutdown = self.shutdown_tx.subscribe();
        let accept_task = tokio::spawn(async move {
            let mut preparations = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = accept_shutdown.changed() => break,
                    Some(result) = preparations.join_next(), if !preparations.is_empty() => {
                        if let Err(error) = result {
                            warn!(%error, "control peer preparation task failed");
                        }
                    },
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => match control.context.verify_control_peer(&stream) {
                            Ok(()) => {
                                let Ok(permit) = Arc::clone(&connection_permits).try_acquire_owned()
                                else {
                                    drop(stream);
                                    continue;
                                };
                                let incoming_tx = incoming_tx.clone();
                                preparations.spawn(async move {
                                    match prepare_control_connection(stream, permit, preface_timeout).await {
                                        Ok(stream) => {
                                            if incoming_tx.send(Ok(stream)).await.is_err() {
                                                return Err(anyhow::anyhow!("tonic server stopped accepting control connections"));
                                            }
                                        },
                                        Err(error) => {
                                            warn!(%error, "control peer failed HTTP/2 preface");
                                        },
                                    }
                                    Ok::<_, anyhow::Error>(())
                                });
                            }
                            Err(error) => {
                                warn!(%error, "control peer rejected");
                            },
                        },
                        Err(error) => {
                            *listener_error_for_stream
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                            break;
                        },
                    },
                }
            }
            preparations.abort_all();
            while preparations.join_next().await.is_some() {}
        });
        let incoming = ReceiverStream::new(incoming_rx);
        let serve_result = tonic::transport::Server::builder()
            .load_shed(true)
            .layer(ConcurrencyLimitLayer::new(CONTROL_CONNECTION_LIMIT))
            .add_service(
                wire::control_server::ControlServer::new(GrpcControlService::new(Arc::clone(
                    &self,
                )))
                .max_decoding_message_size(CONTROL_MESSAGE_MAX_BYTES)
                .max_encoding_message_size(CONTROL_MESSAGE_MAX_BYTES),
            )
            .serve_with_incoming_shutdown(incoming, async move {
                loop {
                    if shutdown.changed().await.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            })
            .await;
        accept_task.abort();
        let _ = accept_task.await;
        serve_result.context("serve control socket")?;
        if let Some(error) = listener_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return Err(error).context("control listener exited");
        }
        Ok(())
    }
}

pub(crate) fn grpc_code(code: ControlErrorCode) -> tonic::Code {
    match code {
        ControlErrorCode::InvalidRequest
        | ControlErrorCode::UnsupportedApiVersion
        | ControlErrorCode::InvalidResource
        | ControlErrorCode::DesiredDigestMismatch
        | ControlErrorCode::PlanTooLarge => tonic::Code::InvalidArgument,
        ControlErrorCode::Busy => tonic::Code::ResourceExhausted,
        ControlErrorCode::NotReady | ControlErrorCode::RecoveryRequired => {
            tonic::Code::FailedPrecondition
        },
        ControlErrorCode::Conflict
        | ControlErrorCode::StaleBaseRevision
        | ControlErrorCode::MutationIdReuseMismatch
        | ControlErrorCode::ActionIdReuseMismatch => tonic::Code::Aborted,
        ControlErrorCode::NotFound
        | ControlErrorCode::MissingProviderArtifact
        | ControlErrorCode::ActionUnavailable => tonic::Code::NotFound,
        ControlErrorCode::AlreadyExists => tonic::Code::AlreadyExists,
        ControlErrorCode::Internal => tonic::Code::Internal,
    }
}

pub(crate) fn grpc_status(error: ControlError) -> Status {
    let details = grpc::to_error_detail(&error).encode_to_vec();
    Status::with_details(grpc_code(error.code), error.message, Bytes::from(details))
}

pub(crate) fn grpc_internal(error: impl std::fmt::Display) -> Status {
    grpc_status(ControlError::new(
        ControlErrorCode::Internal,
        error.to_string(),
    ))
}

pub(crate) fn grpc_invalid(error: impl std::fmt::Display) -> Status {
    grpc_status(ControlError::new(
        ControlErrorCode::InvalidRequest,
        error.to_string(),
    ))
}

pub(crate) fn resource_grpc_error(error: &omnifs_api::grpc::FromGrpcError) -> Status {
    let code = match error {
        omnifs_api::grpc::FromGrpcError::UnsupportedApiVersion(_) => {
            ControlErrorCode::UnsupportedApiVersion
        },
        omnifs_api::grpc::FromGrpcError::TooManyResources { .. } => ControlErrorCode::PlanTooLarge,
        _ => ControlErrorCode::InvalidRequest,
    };
    grpc_status(ControlError::new(code, error.to_string()))
}

pub(crate) fn resource_control_status(
    error: &crate::resource_control::ResourceControlError,
) -> Status {
    grpc_status(resource_control_error(error))
}

pub(crate) fn required<T>(value: Option<T>, name: &'static str) -> Result<T, Status> {
    value.ok_or_else(|| grpc_invalid(format!("missing required field `{name}`")))
}

pub(crate) fn exact_bytes<const N: usize>(
    value: &[u8],
    name: &'static str,
) -> Result<[u8; N], Status> {
    value
        .try_into()
        .map_err(|_| grpc_invalid(format!("invalid {name} length")))
}

struct ControlConnection {
    inner: UnixStream,
    preface: [u8; CONTROL_HTTP2_PREFACE.len()],
    preface_offset: usize,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

async fn prepare_control_connection(
    mut inner: UnixStream,
    permit: tokio::sync::OwnedSemaphorePermit,
    timeout: std::time::Duration,
) -> anyhow::Result<ControlConnection> {
    let mut preface = [0_u8; CONTROL_HTTP2_PREFACE.len()];
    tokio::time::timeout(timeout, inner.read_exact(&mut preface))
        .await
        .context("control HTTP/2 preface timed out")??;
    anyhow::ensure!(
        &preface == CONTROL_HTTP2_PREFACE,
        "control HTTP/2 preface was invalid"
    );
    Ok(ControlConnection {
        inner,
        preface,
        preface_offset: 0,
        _permit: permit,
    })
}

impl AsyncRead for ControlConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.preface_offset < self.preface.len() {
            let length = (self.preface.len() - self.preface_offset).min(buf.remaining());
            buf.put_slice(&self.preface[self.preface_offset..self.preface_offset + length]);
            self.preface_offset += length;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for ControlConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
}

impl tonic::transport::server::Connected for ControlConnection {
    type ConnectInfo = tonic::transport::server::UdsConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        tonic::transport::server::Connected::connect_info(&self.inner)
    }
}

#[cfg(test)]
mod tests;
