//! Control API server:
//! `/v1/{ready,status,credentials,providers,mounts,reconcile,shutdown,events}`.
//!
//! Serves daemon runtime facts, mount reconciliation and shutdown, and the
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
    AddedField, ApiError, AuthDelta, AuthSchemeSurface, AuthSurface, CapabilityChange,
    CapabilityDirection, CredentialHealth, CredentialStatus, DaemonBackend, DaemonHealth,
    DaemonStatus, DaemonSubsystem, ErrorCode, FieldChange, FrontendInfo, FsType, HealthState,
    LimitChange, LimitDirection, MountFailure, MountInfo, MountOutcome, MountReport,
    MountUpdateRequest, ProviderArtifact, ProviderSummary, ReadyInfo, ReconcileReport,
    ReconcileRequest, StopReport, SubsystemHealth, UpgradeDelta,
};
use omnifs_auth::{
    CredentialHealth as AuthCredentialHealth, CredentialStatus as AuthCredentialStatus,
};
use omnifs_engine::{
    FailureKind, InspectorSink, MountRuntimes, ReconcileBusy, ReconcileOutcome, RegistryError,
};
use omnifs_workspace::authn::CredentialId;
use omnifs_workspace::mounts::materialize::materialize;
use omnifs_workspace::mounts::{Name as MountName, Registry, Spec, UpgradePlan};
use omnifs_workspace::provider::{Catalog, CatalogError, Provider};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::format_description::well_known::Rfc3339;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{info, warn};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::context::DaemonContext;
use crate::frontends::Frontend;

const CONTROL_TOKEN_BYTES: usize = 32;
const BEARER_PREFIX: &str = "Bearer ";

#[derive(Clone)]
pub(crate) struct ControlToken {
    path: Arc<PathBuf>,
    value: Arc<str>,
}

impl ControlToken {
    pub(crate) fn generate(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut random = [0_u8; CONTROL_TOKEN_BYTES];
        getrandom::fill(&mut random).context("generate daemon control token")?;
        let value = hex::encode(random);

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("create control token file {}", path.display()))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set control token file mode {}", path.display()))?;
        file.write_all(value.as_bytes())
            .with_context(|| format!("write control token file {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flush control token file {}", path.display()))?;

        Ok(Self {
            path: Arc::new(path),
            value: Arc::from(value),
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

    fn remove_file(&self) {
        match std::fs::remove_file(self.path.as_ref()) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => {
                warn!(%error, path = %self.path.display(), "failed to remove control token file");
            },
        }
    }

    #[cfg(test)]
    fn from_test_value(value: impl Into<Arc<str>>) -> Self {
        Self {
            path: Arc::new(PathBuf::from("control-token")),
            value: value.into(),
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    info(title = "omnifs daemon control API", version = env!("CARGO_PKG_VERSION")),
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
        FsType,
        DaemonBackend,
        MountInfo,
        CredentialHealth,
        CredentialStatus,
        ProviderArtifact,
        ProviderSummary,
        MountFailure,
        MountReport,
        MountOutcome,
        MountUpdateRequest,
        UpgradeDelta,
        AddedField,
        FieldChange,
        CapabilityChange,
        CapabilityDirection,
        LimitChange,
        LimitDirection,
        AuthDelta,
        AuthSurface,
        AuthSchemeSurface,
        ReconcileReport,
        ReconcileRequest,
        StopReport,
    ))
)]
struct ApiDoc;

pub struct Daemon {
    context: DaemonContext,
    registry: Arc<MountRuntimes>,
    sink: Option<Arc<InspectorSink>>,
    frontends: Frontend,
    control_token: ControlToken,
    /// The last reconcile's failed mounts, surfaced in `status` so a dark mount
    /// is visible with its reason instead of silently absent.
    last_failed: std::sync::Mutex<Vec<MountFailure>>,
}

impl Daemon {
    pub(crate) fn new(
        context: DaemonContext,
        registry: Arc<MountRuntimes>,
        sink: Option<Arc<InspectorSink>>,
        frontends: Frontend,
        control_token: ControlToken,
    ) -> Self {
        Self {
            context,
            registry,
            sink,
            frontends,
            control_token,
            last_failed: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn mount_point(&self) -> &Path {
        self.context.mount_point()
    }

    pub fn spawn_control(
        self: &Arc<Self>,
        listener: std::net::TcpListener,
        rt: &tokio::runtime::Handle,
    ) -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let addr = listener.local_addr()?;
        info!(%addr, "control API listening");
        let app = Self::router(Arc::clone(self));
        rt.spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                warn!(%error, "control API server exited");
            }
        });
        Ok(())
    }

    pub fn serve(&self, rt: &tokio::runtime::Handle) -> anyhow::Result<()> {
        self.frontends.serve(rt)
    }

    pub fn remove_control_token(&self) {
        self.control_token.remove_file();
    }

    fn control_status(&self) -> DaemonStatus {
        let root_mount = self.registry.root_mount_name();
        let entries = self.registry.runtime_entries();
        let mut mounts = Vec::with_capacity(entries.len());
        let mut credential_degraded = Vec::new();
        for (mount, runtime) in entries {
            if let Some(warning) = runtime.credential_warning() {
                credential_degraded.push((mount.clone(), warning.to_string()));
            }
            mounts.push(MountInfo {
                root_mount: root_mount.as_deref() == Some(mount.as_str()),
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

        let failed = self
            .last_failed
            .lock()
            .map(|failed| failed.clone())
            .unwrap_or_default();
        self.context.status(
            self.frontends.serving(),
            mounts,
            failed,
            &credential_degraded,
        )
    }

    /// Converge the running mount set to `mounts/*.json`, synchronously. Runs
    /// `registry.reconcile`, then reflects the result into the frontend: added
    /// and updated mounts (re)create the root symlink, removed and updated
    /// mounts invalidate the root child so a torn-down mount does not linger as
    /// a phantom directory. Callable directly from the blocking startup path.
    pub fn reconcile_blocking(&self, handle: &tokio::runtime::Handle) -> ReconcileReport {
        let outcome = self
            .registry
            .reconcile(handle, self.context.materialization_mode());
        self.apply_reconcile_outcome(outcome)
    }

    pub async fn try_reconcile(
        self: &Arc<Self>,
        mounts: Option<Vec<String>>,
    ) -> Result<ReconcileReport, ReconcileBusy> {
        let daemon = Arc::clone(self);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let outcome = daemon.registry.try_reconcile_scoped(
                &handle,
                daemon.context.materialization_mode(),
                mounts,
            )?;
            Ok(daemon.apply_reconcile_outcome(outcome))
        })
        .await
        .unwrap_or_else(|join_error| {
            warn!(%join_error, "reconcile task failed");
            Ok(ReconcileReport::default())
        })
    }

    pub async fn converge_spec(
        self: &Arc<Self>,
        spec: Spec,
        approved: Option<UpgradePlan>,
    ) -> ReconcileReport {
        let daemon = Arc::clone(self);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let outcome = daemon.registry.converge_spec(
                &handle,
                daemon.context.materialization_mode(),
                spec,
                approved,
            );
            daemon.apply_reconcile_outcome(outcome)
        })
        .await
        .unwrap_or_else(|join_error| {
            warn!(%join_error, "mount converge task failed");
            ReconcileReport::default()
        })
    }

    pub async fn remove_mount(self: &Arc<Self>, mount: &str) -> ReconcileReport {
        let daemon = Arc::clone(self);
        let mount = mount.to_string();
        tokio::task::spawn_blocking(move || {
            let mut outcome = ReconcileOutcome::default();
            match daemon.registry.remove_mount(&mount) {
                // Removing an already-absent mount is convergent: report removed.
                Ok(()) | Err(RegistryError::MountNotFound(_)) => outcome.removed.push(mount),
                Err(error) => outcome.failed.push(omnifs_engine::MountFailure {
                    mount,
                    kind: error_kind(&error),
                    reason: error.to_string(),
                    detail: None,
                }),
            }
            daemon.apply_reconcile_outcome(outcome)
        })
        .await
        .unwrap_or_else(|join_error| {
            warn!(%join_error, "mount removal task failed");
            ReconcileReport::default()
        })
    }

    fn validate_spec(&self, spec: &Spec) -> Result<(), Box<Response>> {
        let catalog = Catalog::open(self.context.providers_dir());
        materialize(spec.clone(), &catalog, self.context.materialization_mode())
            .map(|_| ())
            .map_err(|error| {
                Box::new(error_response(
                    StatusCode::BAD_REQUEST,
                    ErrorCode::SpecInvalid,
                    error.to_string(),
                ))
            })
    }

    fn apply_reconcile_outcome(&self, outcome: ReconcileOutcome) -> ReconcileReport {
        for name in &outcome.added {
            self.update_root_symlink(name, true);
        }
        for name in &outcome.updated {
            self.frontends.invalidate_root_child(name);
            self.update_root_symlink(name, true);
        }
        for name in &outcome.removed {
            self.frontends.invalidate_root_child(name);
            self.update_root_symlink(name, false);
        }
        let failed: Vec<MountFailure> = outcome.failed.into_iter().map(api_mount_failure).collect();
        if let Ok(mut last) = self.last_failed.lock() {
            last.clone_from(&failed);
        }
        ReconcileReport {
            added: outcome.added,
            updated: outcome.updated,
            removed: outcome.removed,
            failed,
        }
    }

    /// Unmount the frontend from a detached task so the HTTP response flushes
    /// first. The unmount unblocks the `serve` loop on the main thread, which
    /// then drops providers and exits. The brief delay keeps the response ahead
    /// of the process teardown on the localhost connection.
    pub fn trigger_shutdown(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        // `unmount` shells out (a blocking syscall), so run it on a blocking
        // thread rather than an async worker. The brief delay lets the HTTP
        // response flush on the localhost connection before the mount drops and
        // `serve` unblocks the process toward exit.
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            daemon.frontends.unmount();
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
                Ok(record) => record_line(&record),
                Err(BroadcastStreamRecvError::Lagged(n)) => Some(format!("# dropped {n} events\n")),
            });
        let body = Body::from_stream(stream.map(Ok::<_, Infallible>));

        Response::builder()
            .header(header::CONTENT_TYPE, "application/x-ndjson")
            .body(body)
            .expect("static response parts are valid")
    }

    /// Maintain the container-image convenience symlink `/<mount>` →
    /// `<mount-point>/<mount>`. Best-effort: failures are logged, never fatal.
    /// Only entries that are symlinks into the mount point are ever removed,
    /// so a mount named `bin` or `lib` cannot clobber real root entries.
    fn update_root_symlink(&self, mount: &str, present: bool) {
        if !self.context.root_symlinks() {
            return;
        }
        let mount_point = self.context.mount_point();
        let link = std::path::Path::new("/").join(mount);
        let target = mount_point.join(mount);
        let ours =
            std::fs::read_link(&link).is_ok_and(|existing| existing.starts_with(mount_point));
        match (present, ours) {
            (true, _) => {
                if ours {
                    let _ = std::fs::remove_file(&link);
                } else if link.exists() || link.is_symlink() {
                    warn!(link = %link.display(), "not replacing existing root entry with mount symlink");
                    return;
                }
                #[cfg(unix)]
                if let Err(error) = std::os::unix::fs::symlink(&target, &link) {
                    warn!(%error, link = %link.display(), "failed to create root symlink");
                }
            },
            (false, true) => {
                if let Err(error) = std::fs::remove_file(&link)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(%error, link = %link.display(), "failed to remove root symlink");
                }
            },
            (false, false) => {},
        }
    }

    fn api_router() -> OpenApiRouter<Arc<Self>> {
        OpenApiRouter::new()
            .routes(routes!(ready))
            .routes(routes!(status))
            .routes(routes!(credentials_list))
            .routes(routes!(credential_reload))
            .routes(routes!(providers_list))
            .routes(routes!(mounts_list))
            .routes(routes!(mount_create))
            .routes(routes!(mount_inspect))
            .routes(routes!(mount_update))
            .routes(routes!(mount_delete))
            .routes(routes!(mount_export))
            .routes(routes!(reconcile))
            .routes(routes!(shutdown))
            .routes(routes!(events))
    }

    fn router(state: Arc<Self>) -> Router {
        let control_token = state.control_token.clone();
        let (router, _) = Self::api_router().with_state(state).split_for_parts();
        router
            .fallback(route_not_found)
            .method_not_allowed_fallback(method_not_allowed)
            .layer(middleware::from_fn_with_state(
                control_token,
                authenticate_control_request,
            ))
    }
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
        (status = 200, description = "filesystem frontend is serving", body = ReadyInfo),
        (status = 503, description = "filesystem frontend is not serving yet", body = ApiError),
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
            "filesystem frontend is not serving yet",
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
    post,
    path = "/v1/mounts",
    operation_id = "mount_create",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "mount create result", body = MountReport),
        (status = 400, description = "invalid mount spec", body = ApiError),
    ),
)]
async fn mount_create(
    State(daemon): State<Arc<Daemon>>,
    Json(spec_json): Json<serde_json::Value>,
) -> Response {
    let spec = match parse_spec_json(spec_json) {
        Ok(spec) => spec,
        Err(response) => return *response,
    };
    let name = match validate_spec_mount_name(&spec) {
        Ok(name) => name,
        Err(response) => return *response,
    };
    if let Err(response) = daemon.validate_spec(&spec) {
        return *response;
    }
    let mounts_dir = daemon.context.mounts_dir().to_path_buf();
    let spec_for_write = spec.clone();
    let existing = match Registry::load(&mounts_dir).map(|registry| registry.get(&name).is_some()) {
        Ok(existing) => existing,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                error.to_string(),
            );
        },
    };
    if existing {
        return error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::SpecInvalid,
            format!("mount `{name}` already exists; use PUT to update it"),
        );
    }
    match tokio::task::spawn_blocking(move || {
        let mut registry = Registry::load(&mounts_dir)?;
        registry.put(&spec_for_write)
    })
    .await
    {
        Ok(Ok(())) => {},
        Ok(Err(error)) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                error.to_string(),
            );
        },
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                format!("mount create task failed: {error}"),
            );
        },
    }

    let report = daemon.converge_spec(spec, None).await;
    Json(mount_report(name.as_str(), &report, None)).into_response()
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
    put,
    path = "/v1/mounts/{name}",
    operation_id = "mount_update",
    params(("name" = String, Path, description = "mount name")),
    request_body = MountUpdateRequest,
    responses(
        (status = 200, description = "mount update result", body = MountReport),
        (status = 400, description = "invalid mount spec or approval", body = ApiError),
        (status = 404, description = "mount not found", body = ApiError),
    ),
)]
async fn mount_update(
    State(daemon): State<Arc<Daemon>>,
    UrlPath(name): UrlPath<String>,
    Json(request): Json<MountUpdateRequest>,
) -> Response {
    let mount_name = match MountName::new(name.clone()) {
        Ok(name) => name,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                format!("invalid mount name `{name}`: {error}"),
            );
        },
    };
    let spec = match parse_spec_json(request.spec) {
        Ok(spec) => spec,
        Err(response) => return *response,
    };
    if spec.mount != mount_name.as_str() {
        return error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::SpecInvalid,
            format!(
                "request path mount `{}` does not match spec mount `{}`",
                mount_name, spec.mount
            ),
        );
    }
    if let Err(response) = daemon.validate_spec(&spec) {
        return *response;
    }
    let approved = match request
        .approved
        .as_ref()
        .map(upgrade_plan_from_api)
        .transpose()
    {
        Ok(approved) => approved,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                format!("invalid approved upgrade delta: {error}"),
            );
        },
    };
    let mounts_dir = daemon.context.mounts_dir().to_path_buf();
    let spec_for_write = spec.clone();
    let exists = match Registry::load(&mounts_dir) {
        Ok(registry) => registry.get(&mount_name).is_some(),
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                error.to_string(),
            );
        },
    };
    if !exists {
        return error_response(
            StatusCode::NOT_FOUND,
            ErrorCode::MountNotFound,
            format!("mount `{mount_name}` not found"),
        );
    }
    match tokio::task::spawn_blocking(move || {
        let mut registry = Registry::load(&mounts_dir)?;
        registry.put(&spec_for_write)
    })
    .await
    {
        Ok(Ok(())) => {},
        Ok(Err(error)) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                error.to_string(),
            );
        },
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                format!("mount update task failed: {error}"),
            );
        },
    }

    let report = daemon.converge_spec(spec, approved).await;
    Json(mount_report(mount_name.as_str(), &report, request.approved)).into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/mounts/{name}",
    operation_id = "mount_delete",
    params(("name" = String, Path, description = "mount name")),
    responses(
        (status = 200, description = "mount delete result", body = MountReport),
        (status = 404, description = "mount not found", body = ApiError),
    ),
)]
async fn mount_delete(
    State(daemon): State<Arc<Daemon>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    let mount_name = match MountName::new(name.clone()) {
        Ok(name) => name,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                format!("invalid mount name `{name}`: {error}"),
            );
        },
    };
    let mounts_dir = daemon.context.mounts_dir().to_path_buf();
    let mount_for_task = mount_name.clone();
    match tokio::task::spawn_blocking(move || {
        let mut registry = Registry::load(&mounts_dir)?;
        registry.remove(&mount_for_task)
    })
    .await
    {
        Ok(Ok(true)) => {},
        Ok(Ok(false)) => {
            return error_response(
                StatusCode::NOT_FOUND,
                ErrorCode::MountNotFound,
                format!("mount `{mount_name}` not found"),
            );
        },
        Ok(Err(error)) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                error.to_string(),
            );
        },
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                format!("mount delete task failed: {error}"),
            );
        },
    }

    let report = daemon.remove_mount(mount_name.as_str()).await;
    Json(mount_report(mount_name.as_str(), &report, None)).into_response()
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

/// `POST /v1/reconcile`: converge the running mount set to `mounts/*.json`.
#[utoipa::path(
    post,
    path = "/v1/reconcile",
    operation_id = "reconcile",
    request_body = Option<ReconcileRequest>,
    responses(
        (status = 200, description = "what the reconcile changed", body = ReconcileReport),
        (status = 409, description = "another reconcile is already in progress", body = ApiError),
    ),
)]
async fn reconcile(
    State(daemon): State<Arc<Daemon>>,
    request: Option<Json<ReconcileRequest>>,
) -> Response {
    let mounts = request.map(|Json(request)| request.mounts);
    if let Some(mounts) = &mounts {
        for mount in mounts {
            if let Err(error) = MountName::new(mount.clone()) {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    ErrorCode::SpecInvalid,
                    format!("invalid mount name `{mount}`: {error}"),
                );
            }
        }
    }
    match daemon.try_reconcile(mounts).await {
        Ok(report) => Json(report).into_response(),
        Err(ReconcileBusy) => reconcile_busy_response(),
    }
}

/// `POST /v1/shutdown`: unmount the frontend and exit. The daemon holds the
/// frontend handle, so it tears itself down; `omnifs down` no longer infers the
/// teardown from configuration.
#[utoipa::path(
    post,
    path = "/v1/shutdown",
    operation_id = "shutdown",
    responses((status = 200, description = "what the daemon tore down before exiting", body = StopReport)),
)]
async fn shutdown(State(daemon): State<Arc<Daemon>>) -> Json<StopReport> {
    let status = daemon.control_status();
    let report = StopReport {
        frontend: status.frontend,
        mount_point: status.mount_point,
        providers_dropped: status.mounts.len(),
    };
    daemon.trigger_shutdown();
    Json(report)
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
        unauthorized_response()
    }
}

fn unauthorized_response() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        ErrorCode::Unauthorized,
        "control API authorization required",
    )
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

fn reconcile_busy_response() -> Response {
    (
        StatusCode::CONFLICT,
        [(header::RETRY_AFTER, "2")],
        Json(ApiError {
            code: ErrorCode::ReconcileBusy,
            message: "another reconcile is already in progress".to_string(),
            detail: None,
        }),
    )
        .into_response()
}

fn parse_spec_json(value: serde_json::Value) -> Result<Spec, Box<Response>> {
    serde_json::from_value(value).map_err(|error| {
        Box::new(error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::SpecInvalid,
            format!("invalid mount spec: {error}"),
        ))
    })
}

fn validate_spec_mount_name(spec: &Spec) -> Result<MountName, Box<Response>> {
    MountName::new(spec.mount.clone()).map_err(|error| {
        Box::new(error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::SpecInvalid,
            format!("invalid mount name `{}`: {error}", spec.mount),
        ))
    })
}

fn mount_report(
    mount: &str,
    report: &ReconcileReport,
    approved: Option<UpgradeDelta>,
) -> MountReport {
    let failure = report
        .failed
        .iter()
        .find(|failure| failure.mount == mount)
        .cloned();
    let outcome = if failure.is_some() {
        MountOutcome::Failed
    } else if report.added.iter().any(|name| name == mount) {
        MountOutcome::Added
    } else if report.updated.iter().any(|name| name == mount) {
        MountOutcome::Updated
    } else if report.removed.iter().any(|name| name == mount) {
        MountOutcome::Removed
    } else {
        MountOutcome::Unchanged
    };
    MountReport {
        mount: mount.to_string(),
        outcome,
        failure,
        approved,
    }
}

fn api_mount_failure(failure: omnifs_engine::MountFailure) -> MountFailure {
    MountFailure {
        mount: failure.mount,
        kind: api_error_code(failure.kind),
        reason: failure.reason,
        detail: failure.detail.as_ref().map(|plan| {
            serde_json::to_value(upgrade_delta_from_plan(plan))
                .expect("upgrade approval DTO serializes")
        }),
    }
}

fn api_error_code(kind: FailureKind) -> ErrorCode {
    match kind {
        FailureKind::ConsentRequired => ErrorCode::ConsentRequired,
        FailureKind::SpecInvalid => ErrorCode::SpecInvalid,
        FailureKind::ProviderMissing => ErrorCode::ProviderMissing,
        FailureKind::Internal => ErrorCode::Internal,
    }
}

fn error_kind(error: &RegistryError) -> FailureKind {
    match error {
        RegistryError::ConfigError(_)
        | RegistryError::DuplicateMount(_)
        | RegistryError::MountNotFound(_) => FailureKind::SpecInvalid,
        RegistryError::ProviderNotFound(_) => FailureKind::ProviderMissing,
        RegistryError::RuntimeError(_) => FailureKind::Internal,
    }
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
            let latest = catalog
                .latest_by_name(&name)?
                .map(|provider| api_provider_artifact(&provider));
            Ok(ProviderSummary {
                installed: by_name.remove(&name).unwrap_or_default(),
                name: name.to_string(),
                latest,
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

fn upgrade_delta_from_plan(plan: &UpgradePlan) -> UpgradeDelta {
    serde_json::from_value(serde_json::to_value(plan).expect("workspace upgrade plan serializes"))
        .expect("workspace and API upgrade delta shapes match")
}

fn upgrade_plan_from_api(delta: &UpgradeDelta) -> Result<UpgradePlan, serde_json::Error> {
    serde_json::from_value(serde_json::to_value(delta)?)
}

fn record_line(record: &omnifs_api::events::InspectorRecord) -> Option<String> {
    match record.to_json() {
        Ok(mut line) => {
            line.push('\n');
            Some(line)
        },
        Err(error) => {
            warn!(%error, "failed to serialize inspector record");
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{StatusCode, header};
    use axum::middleware;
    use axum::routing::get;
    use omnifs_api::{ApiError, ErrorCode};
    use std::os::unix::fs::PermissionsExt as _;
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
    fn create_mount_rejects_secret_field_in_auth_block() {
        let spec = serde_json::json!({
            "provider": {
                "id": "0000000000000000000000000000000000000000000000000000000000000000",
                "meta": { "name": "demo" }
            },
            "mount": "demo",
            "auth": {
                "type": "oauth",
                "scheme": "oauth",
                "clientSecret": "literal-secret"
            }
        });

        let response = super::parse_spec_json(spec).expect_err("spec must be rejected");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn reconcile_busy_response_sets_retry_after() {
        let response = super::reconcile_busy_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.headers().get(axum::http::header::RETRY_AFTER),
            Some(&axum::http::HeaderValue::from_static("2"))
        );
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

    #[test]
    fn control_token_file_is_0600_and_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-token");

        let first = super::ControlToken::generate(path.clone()).unwrap();
        let first_value = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first_value.as_str(), first.value.as_ref());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let second = super::ControlToken::generate(path.clone()).unwrap();
        let second_value = std::fs::read_to_string(&path).unwrap();
        assert_eq!(second_value.as_str(), second.value.as_ref());
        assert_ne!(first_value, second_value);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        second.remove_file();
        assert!(!path.exists());
    }

    #[test]
    fn create_mount_rejects_unknown_top_level_spec_field() {
        let spec = serde_json::json!({
            "provider": {
                "id": "0000000000000000000000000000000000000000000000000000000000000000",
                "meta": { "name": "demo" }
            },
            "mount": "demo",
            "moutn": "typo"
        });

        let response = super::parse_spec_json(spec).expect_err("spec must be rejected");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
