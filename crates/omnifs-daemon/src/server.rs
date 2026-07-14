//! Control API server:
//! `/v1/{ready,status,credentials,providers,mounts,shutdown,events}`.
//!
//! Serves daemon runtime facts and shutdown, and the
//! inspector event stream over HTTP on the control listener. See
//! `docs/contracts/50-control-plane.md`.

use anyhow::Context as _;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use omnifs_api::{
    ApiError, CredentialHealth, CredentialStatus, DaemonBackend, DaemonHealth, DaemonStatus,
    DaemonSubsystem, ErrorCode, FrontendAttachTargetReport, FrontendAttachTargetRequest,
    FrontendAttachTargetVsockReport, FrontendDelivery, FrontendInfo, FsType, HealthState,
    MountInfo, ProviderArtifact, ProviderSummary, ReadyInfo, StopReport, SubsystemHealth,
};
use omnifs_auth::{
    CredentialHealth as AuthCredentialHealth, CredentialStatus as AuthCredentialStatus,
};
use omnifs_engine::{InspectorSink, MountRuntimes};
use omnifs_workspace::authn::CredentialId;
use omnifs_workspace::provider::{Catalog, CatalogError, Provider};
use omnifs_workspace::runtime_record::{
    AttachRecord, FrontendKind as RecordFrontendKind, FrontendRecord, RuntimeRecord, Via,
};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use time::format_description::well_known::Rfc3339;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{info, warn};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::context::DaemonContext;
use crate::frontends::Frontends;

const CONTROL_TOKEN_BYTES: usize = 32;
const BEARER_PREFIX: &str = "Bearer ";

/// Environment variable that hands a TCP-serving daemon a token the caller
/// already knows, instead of generating one in memory. Set only on the debug
/// `OMNIFS_DAEMON_ADDR` path; the ordinary host-native daemon serves the
/// token-free Unix socket and never sees this variable.
const CONTROL_TOKEN_ENV: &str = "OMNIFS_CONTROL_TOKEN";

/// Attach-token byte length: 16 raw bytes, hex-encoded to the 32 hex characters
/// the spec calls for.
const ATTACH_TOKEN_BYTES: usize = 16;

/// A random 32-lowercase-hex-character attach token, generated once per daemon
/// start the first time TCP attach is requested (`--attach-tcp` or
/// `POST /v1/frontend/attach-target`). Unlike the daemon's per-start instance id, a
/// failure here is security-relevant (a weak or predictable token would defeat
/// the TCP listener's only auth), so it bails rather than silently downgrading.
fn generate_attach_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; ATTACH_TOKEN_BYTES];
    getrandom::fill(&mut bytes).context("generate attach token")?;
    Ok(hex::encode(bytes))
}

/// The bearer token guarding the TCP control listener. It lives in memory only;
/// the daemon no longer writes a token file. Its value comes from
/// `OMNIFS_CONTROL_TOKEN` when the launcher injects one, else is generated per
/// start. The Unix socket does not check it (filesystem permissions gate that
/// listener).
#[derive(Clone)]
pub(crate) struct ControlToken {
    value: Arc<str>,
}

impl ControlToken {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        if let Ok(value) = std::env::var(CONTROL_TOKEN_ENV) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Self {
                    value: Arc::from(trimmed),
                });
            }
        }
        let mut random = [0_u8; CONTROL_TOKEN_BYTES];
        getrandom::fill(&mut random).context("generate daemon control token")?;
        Ok(Self {
            value: Arc::from(hex::encode(random)),
        })
    }

    fn authorizes(&self, headers: &axum::http::HeaderMap) -> bool {
        let Some(presented) = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix(BEARER_PREFIX))
        else {
            return false;
        };

        constant_time_eq::constant_time_eq(presented.as_bytes(), self.value.as_bytes())
    }

    #[cfg(test)]
    fn from_test_value(value: impl Into<Arc<str>>) -> Self {
        Self {
            value: value.into(),
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    info(title = "omnifs daemon control API", version = "7.0"),
    components(schemas(
        ReadyInfo,
        ApiError,
        ErrorCode,
        DaemonStatus,
        DaemonHealth,
        SubsystemHealth,
        DaemonSubsystem,
        HealthState,
        FrontendInfo,
        FrontendDelivery,
        FsType,
        DaemonBackend,
        MountInfo,
        CredentialHealth,
        CredentialStatus,
        ProviderArtifact,
        ProviderSummary,
        StopReport,
        FrontendAttachTargetRequest,
        FrontendAttachTargetReport,
        FrontendAttachTargetVsockReport,
    ))
)]
struct ApiDoc;

/// A bound TCP namespace attach listener: its address and per-instance token,
/// handed back verbatim by [`Daemon::ensure_attach_tcp`] on a repeat call
/// (binding is a one-time, idempotent action).
#[derive(Debug, Clone)]
pub(crate) struct AttachTcpState {
    addr: SocketAddr,
    token: String,
}

/// A bound token-checking UDS namespace attach listener (the krunkit
/// vsock-proxy path): its socket path and per-instance token, handed back
/// verbatim by [`Daemon::ensure_attach_uds`] on a repeat call.
#[derive(Debug, Clone)]
pub(crate) struct AttachUdsState {
    socket_path: PathBuf,
    token: String,
}

/// A host address approved for the namespace attach listener. Loopback is
/// always valid. On native Linux, the only additional authority is the IPv4
/// address assigned to Docker's default `docker0` bridge.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AttachBindAddr(Ipv4Addr);

impl AttachBindAddr {
    pub(crate) const fn loopback() -> Self {
        Self(Ipv4Addr::LOCALHOST)
    }

    fn requested(candidate: Option<Ipv4Addr>) -> anyhow::Result<Self> {
        let candidate = candidate.unwrap_or(Ipv4Addr::LOCALHOST);
        if candidate == Ipv4Addr::LOCALHOST {
            return Ok(Self::loopback());
        }

        #[cfg(target_os = "linux")]
        if nix::ifaddrs::getifaddrs()
            .context("enumerate host network interfaces")?
            .any(|interface| {
                interface.interface_name == "docker0"
                    && interface
                        .address
                        .as_ref()
                        .and_then(nix::sys::socket::SockaddrStorage::as_sockaddr_in)
                        .is_some_and(|address| address.ip() == candidate)
            })
        {
            return Ok(Self(candidate));
        }

        anyhow::bail!(
            "attach listener may bind only to loopback or Linux's default Docker bridge gateway, not {candidate}"
        )
    }
}

/// The outcome of binding either attach transport. `NamespaceNotReady` is not an
/// error: it is the same transient window `/v1/ready` already reports before
/// startup loading finishes, so the caller renders it as a 503 rather than a
/// 500.
pub(crate) enum AttachOutcome<T> {
    Bound(T),
    NamespaceNotReady,
}

pub(crate) struct RuntimeRecordStore {
    path: PathBuf,
    record: Mutex<Option<RuntimeRecord>>,
}

impl RuntimeRecordStore {
    pub(crate) fn new(path: PathBuf, record: RuntimeRecord) -> Arc<Self> {
        Arc::new(Self {
            path,
            record: Mutex::new(Some(record)),
        })
    }

    pub(crate) fn write(&self) {
        let guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = guard.as_ref()
            && let Err(error) = record.write(&self.path)
        {
            warn!(%error, path = %self.path.display(), "failed to write runtime record");
        }
    }

    fn set_attach(&self, target: AttachRecord) {
        let mut guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = guard.as_mut() {
            record.set_attach(target);
            if let Err(error) = record.write(&self.path) {
                warn!(%error, path = %self.path.display(), "failed to persist attach listener");
            }
        }
    }

    fn set_frontends(&self, frontends: Vec<FrontendInfo>) {
        let frontends = frontends
            .into_iter()
            .map(|frontend| FrontendRecord {
                kind: match frontend.fs_type {
                    FsType::Fuse => RecordFrontendKind::Fuse,
                    FsType::Nfs => RecordFrontendKind::Nfs,
                },
                mount_point: frontend.mount_point,
                via: match frontend.delivery {
                    FrontendDelivery::Local => Via::Local,
                    FrontendDelivery::Docker => Via::Docker,
                    FrontendDelivery::Krunkit => Via::Krunkit,
                },
            })
            .collect();
        let mut guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = guard.as_mut() {
            record.set_frontends(frontends);
            if let Err(error) = record.write(&self.path) {
                warn!(%error, path = %self.path.display(), "failed to persist attached frontends");
            }
        }
    }

    pub(crate) fn remove(&self) {
        let mut guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.take();
        if let Err(error) = RuntimeRecord::remove(&self.path) {
            warn!(%error, path = %self.path.display(), "failed to remove runtime record");
        }
    }
}

pub struct Daemon {
    context: DaemonContext,
    registry: Arc<MountRuntimes>,
    sink: Option<Arc<InspectorSink>>,
    frontends: Frontends,
    runtime_record: Arc<RuntimeRecordStore>,
    control_token: ControlToken,
    /// Set once all startup namespace listeners are serving.
    attach_serving: std::sync::atomic::AtomicBool,
    /// The shared namespace every attach listener serves. Set once via
    /// [`Self::set_namespace`], right after startup
    /// startup loading builds it (see `run` in `app.rs`); read by
    /// [`Self::ensure_attach_tcp`] so a `POST /v1/frontend/attach-target` call can
    /// bind a TCP attach listener on a running daemon without a restart.
    namespace: OnceLock<Arc<omnifs_engine::TreeNamespace>>,
    /// The bound TCP attach listener, if any: bound eagerly at start via
    /// `--attach-tcp`, or later via `POST /v1/frontend/attach-target`. A listener
    /// cannot be re-pointed once serving, so a repeat request returns the
    /// existing binding rather than rebinding.
    attach_tcp: Mutex<Option<AttachTcpState>>,
    /// The bound token-checking UDS attach listener, if any: bound on demand
    /// via `POST /v1/frontend/attach-target/vsock` for the krunkit vsock-proxy path.
    /// Same idempotency as `attach_tcp`.
    attach_uds: Mutex<Option<AttachUdsState>>,
}

impl Daemon {
    pub(crate) fn new(
        context: DaemonContext,
        registry: Arc<MountRuntimes>,
        sink: Option<Arc<InspectorSink>>,
        runtime_record: Arc<RuntimeRecordStore>,
        control_token: ControlToken,
    ) -> Self {
        let record = Arc::clone(&runtime_record);
        let frontends = Frontends::new(Arc::new(move |frontends| {
            record.set_frontends(frontends);
        }));
        Self {
            context,
            registry,
            sink,
            frontends,
            runtime_record,
            control_token,
            attach_serving: std::sync::atomic::AtomicBool::new(false),
            namespace: OnceLock::new(),
            attach_tcp: Mutex::new(None),
            attach_uds: Mutex::new(None),
        }
    }

    /// Record the shared namespace once it is built after atomic startup load.
    /// A second call is a no-op: the namespace is built exactly once per daemon
    /// start.
    pub fn set_namespace(&self, namespace: Arc<omnifs_engine::TreeNamespace>) {
        let _ = self.namespace.set(namespace);
    }

    /// Bind the TCP namespace attach listener at `bind_ip:port` (`0` = ephemeral)
    /// unless one is already bound, in which case the existing binding is
    /// returned unchanged (idempotent: a listener cannot be re-pointed once
    /// serving). Used both by the eager `--attach-tcp` startup path and by the
    /// `POST /v1/frontend/attach-target` route on an already-running daemon.
    ///
    /// Persists the binding into the daemon's on-disk runtime record.
    pub fn ensure_attach_tcp(
        self: &Arc<Self>,
        bind_addr: AttachBindAddr,
        port: u16,
        rt: &tokio::runtime::Handle,
    ) -> anyhow::Result<AttachOutcome<AttachTcpState>> {
        let mut guard = self
            .attach_tcp
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = guard.as_ref() {
            return Ok(AttachOutcome::Bound(state.clone()));
        }
        let Some(namespace) = self.namespace.get() else {
            return Ok(AttachOutcome::NamespaceNotReady);
        };

        let std_listener = std::net::TcpListener::bind((bind_addr.0, port))
            .with_context(|| format!("bind attach TCP listener on {}:{port}", bind_addr.0))?;
        std_listener
            .set_nonblocking(true)
            .context("set attach TCP listener non-blocking")?;
        let addr = std_listener
            .local_addr()
            .context("read attach TCP listener address")?;
        let listener = tokio::net::TcpListener::from_std(std_listener)
            .context("hand the attach TCP listener to tokio")?;
        let token = generate_attach_token()?;

        let ns = Arc::clone(namespace) as Arc<dyn omnifs_engine::Namespace>;
        let instance_id = self.context.instance_id().to_string();
        let serve_token = token.clone();
        // Docker is the only delivery mechanism this route serves today (see
        // `frontend_attach_target`'s `driver` validation); the label lives
        // here, at bind time, rather than trusting anything a connecting
        // guest claims about itself.
        let observer = self.frontends.attach_observer(FrontendDelivery::Docker);
        info!(%addr, "serving namespace attach listener (tcp, token-authenticated)");
        rt.spawn(omnifs_vfs_wire::serve_listener_tcp(
            ns,
            listener,
            instance_id,
            serve_token,
            Some(observer),
        ));

        let state = AttachTcpState { addr, token };
        *guard = Some(state.clone());
        drop(guard);
        self.runtime_record.set_attach(AttachRecord::Tcp {
            addr: state.addr.to_string(),
            token: state.token.clone(),
        });
        Ok(AttachOutcome::Bound(state))
    }

    /// Bind the token-checking UDS namespace attach listener at
    /// `frontends/vsock-attach.sock` unless one is already bound, in which case
    /// the existing binding is returned unchanged (idempotent, mirroring
    /// [`Self::ensure_attach_tcp`]). This is the krunkit vsock-proxy path: the
    /// guest has no shared host Unix socket and no Docker-style loopback
    /// either, so it dials host vsock and krunkit proxies every connection onto
    /// this socket, looking like the same trusted local peer each time, so
    /// `token` (not filesystem permissions) is the real auth here, checked the
    /// same way [`Self::ensure_attach_tcp`]'s token is.
    ///
    /// The target is persisted in the daemon-owned runtime record, and its
    /// socket path uses the same stale-socket policy as the local listener.
    pub fn ensure_attach_uds(
        self: &Arc<Self>,
        rt: &tokio::runtime::Handle,
    ) -> anyhow::Result<AttachOutcome<AttachUdsState>> {
        let mut guard = self
            .attach_uds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = guard.as_ref() {
            return Ok(AttachOutcome::Bound(state.clone()));
        }
        let Some(namespace) = self.namespace.get() else {
            return Ok(AttachOutcome::NamespaceNotReady);
        };

        let (std_listener, socket_path) = self.context.bind_vsock_attach_socket()?;
        std_listener
            .set_nonblocking(true)
            .context("set attach UDS listener non-blocking")?;
        let listener = tokio::net::UnixListener::from_std(std_listener)
            .context("hand the attach UDS listener to tokio")?;
        let token = generate_attach_token()?;

        let ns = Arc::clone(namespace) as Arc<dyn omnifs_engine::Namespace>;
        let instance_id = self.context.instance_id().to_string();
        let serve_token = token.clone();
        let observer = self.frontends.attach_observer(FrontendDelivery::Krunkit);
        info!(path = %socket_path.display(), "serving namespace attach listener (uds, token-authenticated)");
        rt.spawn(omnifs_vfs_wire::serve_listener(
            ns,
            listener,
            instance_id,
            Some(serve_token),
            Some(observer),
        ));

        let state = AttachUdsState { socket_path, token };
        *guard = Some(state.clone());
        drop(guard);
        self.runtime_record.set_attach(AttachRecord::Vsock {
            socket_path: state.socket_path.clone(),
            token: state.token.clone(),
        });
        Ok(AttachOutcome::Bound(state))
    }

    /// Build the [`omnifs_vfs_wire::AttachObserver`] for one wire listener,
    /// labeled with the delivery mechanism the caller assigned it at bind
    /// time. Exposed so `app.rs` can wire it into the fixed local listener.
    pub(crate) fn attach_observer(
        &self,
        delivery: FrontendDelivery,
    ) -> Arc<dyn omnifs_vfs_wire::AttachObserver> {
        self.frontends.attach_observer(delivery)
    }

    /// Mark all startup namespace listeners as serving. Called once after mount
    /// loading and every requested listener bind succeeds.
    pub fn mark_attach_serving(&self) {
        self.attach_serving
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Serve the control API over TCP with the bearer-token middleware. Used by
    /// the container and the `--listen` debug path.
    pub fn spawn_control_tcp(
        self: &Arc<Self>,
        listener: std::net::TcpListener,
        rt: &tokio::runtime::Handle,
    ) -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let addr = listener.local_addr()?;
        info!(%addr, "control API listening (tcp, token-authenticated)");
        let app = Self::router(Arc::clone(self), Auth::BearerToken);
        rt.spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                warn!(%error, "control API server exited");
            }
        });
        Ok(())
    }

    /// Serve the control API over the Unix socket, where auth is filesystem
    /// permissions and the bearer middleware is omitted.
    pub fn spawn_control_unix(
        self: &Arc<Self>,
        listener: std::os::unix::net::UnixListener,
        rt: &tokio::runtime::Handle,
    ) -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        info!("control API listening (unix socket, filesystem-permission auth)");
        let app = Self::router(Arc::clone(self), Auth::FilesystemPermissions);
        rt.spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                warn!(%error, "control API server exited");
            }
        });
        Ok(())
    }

    pub fn serve(&self) {
        self.frontends.serve();
    }

    fn control_status(&self) -> DaemonStatus {
        let entries = self.registry.runtime_entries();
        let mut mounts = Vec::with_capacity(entries.len());
        let mut credential_degraded = Vec::new();
        for (mount, runtime) in entries {
            if let Some(warning) = runtime.credential_warning() {
                credential_degraded.push((mount.clone(), warning.to_string()));
            }
            mounts.push(MountInfo {
                provider_name: runtime.provider_name().to_string(),
                provider_id: runtime.provider_id().to_string(),
                auth_health: runtime
                    .auth_health()
                    .map(|health| api_credential_health_kind(&health)),
                mount,
            });
        }
        mounts.sort_by(|a, b| a.mount.cmp(&b.mount));
        credential_degraded.sort_by(|a, b| a.0.cmp(&b.0));

        self.context.status(
            self.attach_serving
                .load(std::sync::atomic::Ordering::Acquire),
            self.frontends.attached(),
            mounts,
            &credential_degraded,
        )
    }

    /// Release the process-lifetime serving latch after giving the HTTP response
    /// a brief chance to flush.
    pub fn trigger_shutdown(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            daemon.frontends.shutdown();
        });
    }

    fn event_stream(&self) -> Response {
        let Some(sink) = self.sink.clone() else {
            return error_response(
                StatusCode::NOT_FOUND,
                ErrorCode::Internal,
                "inspector stream disabled (OMNIFS_INSPECTOR=0)",
            );
        };

        let subscription = sink.subscribe();
        let stream = tokio_stream::iter(subscription.history)
            .map(Ok)
            .chain(BroadcastStream::new(subscription.live))
            .filter_map(|item| match item {
                Ok(record) => match record.to_json_line() {
                    Ok(line) => Some(line),
                    Err(error) => {
                        warn!(%error, "failed to serialize inspector record");
                        None
                    },
                },
                Err(BroadcastStreamRecvError::Lagged(n)) => Some(format!("# dropped {n} events\n")),
            });
        let body = Body::from_stream(stream.map(Ok::<_, Infallible>));

        Response::builder()
            .header(header::CONTENT_TYPE, "application/x-ndjson")
            .body(body)
            .expect("static response parts are valid")
    }

    fn api_router() -> OpenApiRouter<Arc<Self>> {
        OpenApiRouter::new()
            .routes(routes!(ready))
            .routes(routes!(status))
            .routes(routes!(credentials_list))
            .routes(routes!(credential_reload))
            .routes(routes!(providers_list))
            .routes(routes!(mounts_list))
            .routes(routes!(mount_inspect))
            .routes(routes!(mount_export))
            .routes(routes!(shutdown))
            .routes(routes!(events))
            .routes(routes!(frontend_attach_target))
            .routes(routes!(frontend_attach_target_vsock))
    }

    fn router(state: Arc<Self>, auth: Auth) -> Router {
        let control_token = state.control_token.clone();
        let (router, _) = Self::api_router().with_state(state).split_for_parts();
        let router = router
            .fallback(route_not_found)
            .method_not_allowed_fallback(method_not_allowed);
        match auth {
            Auth::BearerToken => router.layer(middleware::from_fn_with_state(
                control_token,
                authenticate_control_request,
            )),
            Auth::FilesystemPermissions => router,
        }
    }
}

/// Which auth policy a control listener enforces. The TCP listener checks the
/// bearer token; the Unix socket relies on filesystem permissions and omits the
/// middleware entirely.
#[derive(Clone, Copy)]
enum Auth {
    BearerToken,
    FilesystemPermissions,
}

pub fn openapi() -> utoipa::openapi::OpenApi {
    let mut openapi = ApiDoc::openapi();
    let (_, paths) = Daemon::api_router().split_for_parts();
    openapi.merge(paths);
    openapi
}

pub fn openapi_json() -> String {
    openapi()
        .to_pretty_json()
        .expect("OpenAPI document serializes")
}

#[utoipa::path(
    get,
    path = "/v1/ready",
    operation_id = "ready",
    responses(
        (status = 200, description = "namespace listeners are serving", body = ReadyInfo),
        (status = 503, description = "namespace listeners are not serving yet", body = ApiError),
    ),
)]
async fn ready(State(daemon): State<Arc<Daemon>>) -> Response {
    let ready = daemon.control_status().ready();
    if ready {
        Json(ReadyInfo { ready }).into_response()
    } else {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "namespace listeners are not serving yet",
        )
    }
}

#[utoipa::path(
    get,
    path = "/v1/status",
    operation_id = "status",
    responses((status = 200, description = "daemon runtime facts", body = DaemonStatus)),
)]
async fn status(State(daemon): State<Arc<Daemon>>) -> Json<DaemonStatus> {
    Json(daemon.control_status())
}

#[utoipa::path(
    get,
    path = "/v1/credentials",
    operation_id = "credentials_list",
    responses((status = 200, description = "registered credential health", body = [CredentialStatus])),
)]
async fn credentials_list(State(daemon): State<Arc<Daemon>>) -> Json<Vec<CredentialStatus>> {
    let mut statuses = daemon
        .registry
        .credential_service()
        .health()
        .into_iter()
        .map(api_credential_status)
        .collect::<Vec<_>>();
    statuses.sort_by(|a, b| a.id.cmp(&b.id));
    Json(statuses)
}

#[utoipa::path(
    post,
    path = "/v1/credentials/{id}/reload",
    operation_id = "credential_reload",
    params(("id" = String, Path, description = "credential storage key")),
    responses(
        (status = 200, description = "refreshed credential health", body = CredentialStatus),
        (status = 400, description = "invalid credential id", body = ApiError),
        (status = 404, description = "credential not registered with the daemon", body = ApiError),
    ),
)]
async fn credential_reload(
    State(daemon): State<Arc<Daemon>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let id = match id.parse::<CredentialId>() {
        Ok(id) => id,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                format!("invalid credential id `{id}`: {error}"),
            );
        },
    };
    match daemon.registry.credential_service().reload(&id).await {
        Some(status) => Json(api_credential_status(status)).into_response(),
        None => error_response(
            StatusCode::NOT_FOUND,
            ErrorCode::CredentialNotFound,
            format!("credential `{id}` is not registered with the daemon"),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/v1/providers",
    operation_id = "providers_list",
    responses(
        (status = 200, description = "installed provider catalog", body = [ProviderSummary]),
        (status = 500, description = "provider catalog unavailable", body = ApiError),
    ),
)]
async fn providers_list(State(daemon): State<Arc<Daemon>>) -> Response {
    let catalog = Catalog::open(daemon.context.providers_dir());
    match provider_summaries(&catalog) {
        Ok(providers) => Json(providers).into_response(),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            format!("provider catalog unavailable: {error}"),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/v1/mounts",
    operation_id = "mounts_list",
    responses((status = 200, description = "loaded provider mounts", body = [MountInfo])),
)]
async fn mounts_list(State(daemon): State<Arc<Daemon>>) -> Json<Vec<MountInfo>> {
    Json(daemon.control_status().mounts)
}

#[utoipa::path(
    get,
    path = "/v1/mounts/{name}",
    operation_id = "mount_inspect",
    params(("name" = String, Path, description = "mount name")),
    responses(
        (status = 200, description = "the mount", body = MountInfo),
        (status = 404, description = "mount not found", body = ApiError),
    ),
)]
async fn mount_inspect(
    State(daemon): State<Arc<Daemon>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    match daemon
        .control_status()
        .mounts
        .into_iter()
        .find(|mount| mount.mount == name)
    {
        Some(info) => Json(info).into_response(),
        None => error_response(
            StatusCode::NOT_FOUND,
            ErrorCode::MountNotFound,
            format!("mount `{name}` not found"),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/v1/mounts/{name}/export",
    operation_id = "mount_export",
    params(("name" = String, Path, description = "mount name")),
    responses(
        (status = 200, description = "canonical-store snapshot tar", content_type = "application/x-tar", body = String),
        (status = 404, description = "mount not found", content_type = "text/plain", body = String),
        (status = 500, description = "snapshot export failed", content_type = "text/plain", body = String),
    ),
)]
async fn mount_export(
    State(daemon): State<Arc<Daemon>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    let registry = Arc::clone(&daemon.registry);
    let task_name = name.clone();
    match tokio::task::spawn_blocking(move || {
        registry
            .snapshot_mount(&task_name)
            .and_then(|snapshot| snapshot.map(|snapshot| snapshot.to_tar_vec()).transpose())
    })
    .await
    {
        Ok(Ok(Some(bytes))) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/x-tar")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}-snapshot.tar\""),
            )
            .body(Body::from(bytes))
            .expect("static response parts are valid"),
        Ok(Ok(None)) => {
            (StatusCode::NOT_FOUND, format!("mount `{name}` not found\n")).into_response()
        },
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("snapshot export failed for mount `{name}`: {error:#}\n"),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("snapshot export task failed for mount `{name}`: {error}\n"),
        )
            .into_response(),
    }
}

/// `POST /v1/shutdown`: release the daemon's serving latch and exit. Frontend
/// processes have independent lifetimes and are torn down by the CLI.
#[utoipa::path(
    post,
    path = "/v1/shutdown",
    operation_id = "shutdown",
    responses((status = 200, description = "daemon state at shutdown", body = StopReport)),
)]
async fn shutdown(State(daemon): State<Arc<Daemon>>) -> Json<StopReport> {
    let status = daemon.control_status();
    let report = StopReport {
        frontends: status.frontends,
        providers_dropped: status.mounts.len(),
    };
    daemon.trigger_shutdown();
    Json(report)
}

/// `POST /v1/frontend/attach-target`: bind the TCP namespace attach listener on a
/// running daemon, so a containerized frontend (the Docker Desktop path, which
/// cannot share a host Unix socket into its Linux VM) can be started later
/// without restarting the daemon. Docker Desktop uses loopback; native Linux
/// asks for the Docker bridge gateway so the container can cross network
/// namespaces without exposing the listener on every host interface.
/// Idempotent: a repeat call returns the already-bound address and token
/// unchanged, since a listener cannot be re-pointed once serving.
#[utoipa::path(
    post,
    path = "/v1/frontend/attach-target",
    operation_id = "frontend_attach_target",
    request_body = Option<FrontendAttachTargetRequest>,
    responses(
        (status = 200, description = "the TCP attach listener's address and per-instance token", body = FrontendAttachTargetReport),
        (status = 400, description = "the requested address is not an approved attach boundary", body = ApiError),
        (status = 503, description = "the namespace is not ready yet", body = ApiError),
        (status = 500, description = "failed to bind the attach listener", body = ApiError),
    ),
)]
async fn frontend_attach_target(
    State(daemon): State<Arc<Daemon>>,
    request: Option<Json<FrontendAttachTargetRequest>>,
) -> Response {
    let request = request.map(|Json(request)| request).unwrap_or_default();
    if request.driver != FrontendDelivery::Docker {
        return error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::SpecInvalid,
            format!(
                "unsupported attach driver `{}`; only `docker` is accepted on this route \
                 (krunkit attaches over vsock instead, via /v1/frontend/attach-target/vsock)",
                request.driver
            ),
        );
    }
    let bind_addr = match AttachBindAddr::requested(request.bind_ip) {
        Ok(bind_addr) => bind_addr,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                error.to_string(),
            );
        },
    };
    let rt = tokio::runtime::Handle::current();
    match daemon.ensure_attach_tcp(bind_addr, 0, &rt) {
        Ok(AttachOutcome::Bound(state)) => Json(FrontendAttachTargetReport {
            addr: state.addr.to_string(),
            token: state.token,
        })
        .into_response(),
        Ok(AttachOutcome::NamespaceNotReady) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "the namespace is not ready yet",
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            error.to_string(),
        ),
    }
}

/// `POST /v1/frontend/attach-target/vsock`: bind the token-checking UDS namespace
/// attach listener on a running daemon, for the krunkit vsock-proxy path (a
/// macOS guest VM with no shared host Unix socket and no Docker-style loopback
/// either; it dials host vsock instead, and krunkit proxies every connection
/// onto this socket). Takes no request body: unlike the TCP listener there is
/// no bind address to choose, only the daemon-picked path under the
/// workspace. Idempotent: a repeat call returns the already-bound path and
/// token unchanged, since a listener cannot be re-pointed once serving.
#[utoipa::path(
    post,
    path = "/v1/frontend/attach-target/vsock",
    operation_id = "frontend_attach_target_vsock",
    responses(
        (status = 200, description = "the UDS attach listener's socket path and per-instance token", body = FrontendAttachTargetVsockReport),
        (status = 503, description = "the namespace is not ready yet", body = ApiError),
        (status = 500, description = "failed to bind the attach listener", body = ApiError),
    ),
)]
async fn frontend_attach_target_vsock(State(daemon): State<Arc<Daemon>>) -> Response {
    let rt = tokio::runtime::Handle::current();
    match daemon.ensure_attach_uds(&rt) {
        Ok(AttachOutcome::Bound(state)) => Json(FrontendAttachTargetVsockReport {
            socket_path: state.socket_path.display().to_string(),
            token: state.token,
        })
        .into_response(),
        Ok(AttachOutcome::NamespaceNotReady) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "the namespace is not ready yet",
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            error.to_string(),
        ),
    }
}

/// Stream the inspector history snapshot followed by live records as
/// newline-framed JSON using the same wire format the raw TCP listener used to
/// speak, now chunk-encoded by HTTP. A lagged subscriber gets a
/// `# dropped N events` comment line and resumes from the newest record.
#[utoipa::path(
    get,
    path = "/v1/events",
    operation_id = "events",
    responses(
        (status = 200, description = "newline-framed inspector event stream", content_type = "application/x-ndjson", body = String),
        (status = 404, description = "inspector stream disabled", body = ApiError),
    ),
)]
async fn events(State(daemon): State<Arc<Daemon>>) -> Response {
    daemon.event_stream()
}

async fn authenticate_control_request(
    State(control_token): State<ControlToken>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() == Method::GET && request.uri().path() == "/v1/ready" {
        return next.run(request).await;
    }

    if control_token.authorizes(request.headers()) {
        next.run(request).await
    } else {
        error_response(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "control API authorization required",
        )
    }
}

async fn route_not_found() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        ErrorCode::Internal,
        "control route not found",
    )
}

async fn method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorCode::Internal,
        "method not allowed",
    )
}

fn error_response(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            code,
            message: message.into(),
            detail: None,
        }),
    )
        .into_response()
}

fn api_credential_status(status: AuthCredentialStatus) -> CredentialStatus {
    let refresh_failed_attempts = match &status.health {
        AuthCredentialHealth::RefreshFailed { attempts } => Some(*attempts),
        _ => None,
    };
    CredentialStatus {
        id: status.id.to_string(),
        health: api_credential_health_kind(&status.health),
        refresh_failed_attempts,
        expires_at: status.expires_at.map(|expires_at| {
            expires_at
                .format(&Rfc3339)
                .expect("OffsetDateTime formats as RFC3339")
        }),
        scopes: status.scopes,
    }
}

fn api_credential_health_kind(health: &AuthCredentialHealth) -> CredentialHealth {
    match health {
        AuthCredentialHealth::Ready => CredentialHealth::Ready,
        AuthCredentialHealth::ExpiringSoon => CredentialHealth::ExpiringSoon,
        AuthCredentialHealth::Expired => CredentialHealth::Expired,
        AuthCredentialHealth::RefreshFailed { .. } => CredentialHealth::RefreshFailed,
        AuthCredentialHealth::NeedsConsent => CredentialHealth::NeedsConsent,
        AuthCredentialHealth::Missing => CredentialHealth::Missing,
        AuthCredentialHealth::StaticUnvalidated => CredentialHealth::StaticUnvalidated,
    }
}

fn provider_summaries(catalog: &Catalog) -> Result<Vec<ProviderSummary>, CatalogError> {
    let mut by_name = BTreeMap::new();
    for provider in catalog.installed()? {
        by_name
            .entry(provider.meta.name.clone())
            .or_insert_with(Vec::new)
            .push(api_provider_artifact(&provider));
    }
    for artifacts in by_name.values_mut() {
        artifacts.sort_by(|a, b| {
            a.version
                .cmp(&b.version)
                .then_with(|| a.id_hash.cmp(&b.id_hash))
        });
    }

    let mut names = catalog
        .installable()?
        .into_iter()
        .map(|provider| provider.meta.name)
        .collect::<BTreeSet<_>>();
    names.extend(by_name.keys().cloned());

    names
        .into_iter()
        .map(|name| {
            Ok(ProviderSummary {
                installed: by_name.remove(&name).unwrap_or_default(),
                name: name.to_string(),
            })
        })
        .collect()
}

fn api_provider_artifact(provider: &Provider) -> ProviderArtifact {
    ProviderArtifact {
        version: provider.meta.version.as_ref().map(ToString::to_string),
        id_hash: provider.id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{StatusCode, header};
    use axum::middleware;
    use axum::routing::get;
    use omnifs_api::{ApiError, ErrorCode};
    use tower::ServiceExt as _;

    #[test]
    fn checked_in_openapi_matches_implementation() {
        let checked_in: serde_json::Value =
            serde_json::from_str(include_str!("../../omnifs-api/openapi/daemon.json"))
                .expect("checked-in OpenAPI spec parses");
        let generated: serde_json::Value =
            serde_json::from_str(&super::openapi_json()).expect("generated OpenAPI spec parses");

        assert_eq!(checked_in, generated);
    }

    #[test]
    fn runtime_record_store_fences_late_updates_after_removal() {
        use omnifs_workspace::runtime_record::{
            AttachRecord, Endpoint, RecordedBackend, RuntimeRecord,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let store = super::RuntimeRecordStore::new(
            path.clone(),
            RuntimeRecord::new(
                omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
                Endpoint::Unix {
                    path: dir.path().join("control.sock"),
                },
                RecordedBackend::Native { pid: 1 },
                "instance".to_string(),
                Vec::new(),
            ),
        );

        store.write();
        store.set_attach(AttachRecord::Tcp {
            addr: "127.0.0.1:1".to_string(),
            token: "a".repeat(32),
        });
        store.set_attach(AttachRecord::Vsock {
            socket_path: dir.path().join("vsock.sock"),
            token: "b".repeat(32),
        });
        assert_eq!(RuntimeRecord::read(&path).unwrap().unwrap().attach.len(), 2);

        store.remove();
        store.set_frontends(vec![omnifs_api::FrontendInfo {
            source: "wire".to_string(),
            fs_type: omnifs_api::FsType::Nfs,
            mount_point: PathBuf::from("/omnifs"),
            delivery: omnifs_api::FrontendDelivery::Local,
        }]);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn control_auth_protects_everything_except_ready() {
        let token = super::ControlToken::from_test_value("right-token");
        let app = Router::new()
            .route("/v1/ready", get(|| async { StatusCode::OK }))
            .route("/v1/status", get(|| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn_with_state(
                token,
                super::authenticate_control_request,
            ));

        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let error: ApiError = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, ErrorCode::Unauthorized);

        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status")
                    .header(header::AUTHORIZATION, "Bearer right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    /// With no `OMNIFS_CONTROL_TOKEN` in the environment the token is generated
    /// in memory and never touches disk. The env-injection path is covered by
    /// the launcher/daemon integration, not here, to avoid mutating process env
    /// under a parallel test runner.
    #[test]
    fn control_token_generates_in_memory_when_env_unset() {
        // Only assert the no-env behavior; reading a process-global env var here
        // would race other tests. When unset, `resolve` must synthesize a
        // non-empty hex token without writing any file.
        if std::env::var_os(super::CONTROL_TOKEN_ENV).is_some() {
            return;
        }
        let token = super::ControlToken::resolve().unwrap();
        assert_eq!(token.value.len(), super::CONTROL_TOKEN_BYTES * 2);
        assert!(token.value.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn attach_bind_accepts_only_loopback_without_a_verified_bridge() {
        assert_eq!(
            super::AttachBindAddr::requested(None).unwrap().0,
            std::net::Ipv4Addr::LOCALHOST
        );
        assert!(super::AttachBindAddr::requested(Some(std::net::Ipv4Addr::UNSPECIFIED)).is_err());
        assert!(
            super::AttachBindAddr::requested(Some(std::net::Ipv4Addr::new(192, 0, 2, 1))).is_err()
        );
    }

    /// Fetch and decode `/v1/status` from `router`.
    async fn fetch_status(router: &Router) -> omnifs_api::DaemonStatus {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// Poll `/v1/status` until `predicate` holds or the deadline passes. The
    /// registry update is asynchronous (the observer callback runs on the
    /// connection's own task), so a status assertion right after
    /// connect/disconnect needs to tolerate a short delay rather than racing
    /// it.
    async fn wait_for_status(
        router: &Router,
        mut predicate: impl FnMut(&omnifs_api::DaemonStatus) -> bool,
    ) -> omnifs_api::DaemonStatus {
        for _ in 0..200 {
            let status = fetch_status(router).await;
            if predicate(&status) {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("status did not converge to the expected shape within the deadline");
    }

    async fn assert_non_docker_attach_is_rejected(router: &Router) {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/frontend/attach-target")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"driver":"local"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    async fn shutdown_report(router: &Router) -> omnifs_api::StopReport {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/shutdown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        serde_json::from_slice(
            &to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    fn assert_attached_frontend(status: &omnifs_api::DaemonStatus) {
        let attached = status
            .frontends
            .iter()
            .find(|frontend| frontend.delivery == omnifs_api::FrontendDelivery::Docker)
            .unwrap();
        assert_eq!(attached.fs_type, omnifs_api::FsType::Fuse);
        assert_eq!(attached.source, "wire");
        assert_eq!(attached.mount_point, PathBuf::from("/guest/omnifs"));
        assert!(
            status
                .health
                .subsystem(omnifs_api::DaemonSubsystem::Frontend)
                .unwrap()
                .message
                .contains("attached fuse at /guest/omnifs via docker")
        );
    }

    /// A frontend attached through the TCP namespace listener appears in
    /// `/v1/status` with the listener-owned `docker` delivery label, then
    /// disappears when its connection closes.
    #[tokio::test]
    #[allow(unsafe_code)] // env::set_var requires unsafe; see SAFETY below.
    async fn attached_frontend_appears_and_disappears_in_status() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: cargo-nextest isolates each test into its own process (the
        // same pattern the omnifs-vfs-wire trace-propagation test documents
        // for OMNIFS_INSPECTOR), so mutating OMNIFS_HOME here cannot race a
        // parallel test.
        unsafe {
            std::env::set_var("OMNIFS_HOME", dir.path());
        }

        let args = crate::app::DaemonArgs {
            mount_revision: omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
            mount_snapshot: dir.path().join("mounts"),
            listen: None,
            attach_tcp: None,
        };
        std::fs::create_dir_all(&args.mount_snapshot).unwrap();
        let context = crate::context::DaemonContext::resolve(&args).unwrap();
        context.prepare_startup_dirs().unwrap();

        let cloner = Arc::new(omnifs_engine::GitCloner::new(
            context.cache_dir().to_path_buf(),
        ));
        let registry =
            Arc::new(omnifs_engine::MountRuntimes::new(context.host_context(), cloner).unwrap());
        let runtime_record =
            super::RuntimeRecordStore::new(context.runtime_record_file(), context.runtime_record());
        let control_token = super::ControlToken::from_test_value("test-token");
        let daemon = Arc::new(super::Daemon::new(
            context,
            Arc::clone(&registry),
            None,
            runtime_record,
            control_token,
        ));

        let rt = tokio::runtime::Handle::current();
        let namespace = omnifs_engine::TreeNamespace::new(Arc::clone(&registry), rt.clone());
        daemon.set_namespace(Arc::clone(&namespace));

        let bound = match daemon
            .ensure_attach_tcp(super::AttachBindAddr::loopback(), 0, &rt)
            .unwrap()
        {
            super::AttachOutcome::Bound(state) => state,
            super::AttachOutcome::NamespaceNotReady => {
                panic!("the namespace was set before binding the attach listener")
            },
        };

        let router = super::Daemon::router(Arc::clone(&daemon), super::Auth::FilesystemPermissions);

        assert_non_docker_attach_is_rejected(&router).await;

        let baseline = fetch_status(&router).await;
        assert!(
            baseline
                .frontends
                .iter()
                .all(|frontend| frontend.delivery != omnifs_api::FrontendDelivery::Docker),
            "no attached frontend before any client connects"
        );

        let identity = omnifs_vfs_wire::FrontendIdentity {
            kind: omnifs_vfs_wire::FrontendKind::Fuse,
            mount_point: PathBuf::from("/guest/omnifs"),
        };
        let wire = omnifs_vfs_wire::WireNamespace::attach(
            omnifs_vfs_wire::AttachTarget::Tcp {
                addr: bound.addr.to_string(),
                token: bound.token.clone(),
            },
            identity,
            rt.clone(),
        )
        .await
        .unwrap();

        let attached_status = wait_for_status(&router, |status| {
            status
                .frontends
                .iter()
                .any(|frontend| frontend.delivery == omnifs_api::FrontendDelivery::Docker)
        })
        .await;
        assert_attached_frontend(&attached_status);

        let report = shutdown_report(&router).await;
        assert_eq!(report.frontends.len(), 1);
        assert_eq!(
            report.frontends[0].delivery,
            omnifs_api::FrontendDelivery::Docker
        );

        drop(wire);

        let after_disconnect = wait_for_status(&router, |status| {
            status
                .frontends
                .iter()
                .all(|frontend| frontend.delivery != omnifs_api::FrontendDelivery::Docker)
        })
        .await;
        assert!(
            after_disconnect
                .frontends
                .iter()
                .all(|frontend| frontend.delivery != omnifs_api::FrontendDelivery::Docker)
        );
    }
}
