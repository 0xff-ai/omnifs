//! Control API server: `/v1/{ready,version,status,mounts,events}`.
//!
//! Serves daemon runtime facts and the inspector event stream over HTTP on
//! the control listener; mount mutation side effects live in
//! [`crate::mounts`]. See `docs/design/daemon-cli-split.md`.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use omnifs_api::{API_VERSION, DaemonStatus, FrontendInfo, MountInfo, ReadyInfo, VersionInfo};
use omnifs_host::inspector::InspectorSink;
use omnifs_host::registry::{ProviderRegistry, RegistryError};
use omnifs_inspector::serialize_record;
use omnifs_mount_schema::mounts::Spec;
use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{info, warn};
use utoipa::OpenApi;

use crate::frontends::Frontends;

#[derive(OpenApi)]
#[openapi(
    info(title = "omnifs daemon control API", version = env!("CARGO_PKG_VERSION")),
    paths(ready, version, status, add_mount, remove_mount, events),
    components(schemas(
        VersionInfo,
        ReadyInfo,
        DaemonStatus,
        FrontendInfo,
        MountInfo,
        Spec,
        omnifs_mount_schema::Auth,
        omnifs_mount_schema::StaticToken,
        omnifs_mount_schema::OAuth,
        omnifs_mount_schema::ProviderConfig,
        omnifs_mount_schema::ProviderCapabilities,
        omnifs_mount_schema::PreopenedPath,
        omnifs_mount_schema::PreopenMode,
    ))
)]
struct ApiDoc;

pub struct Daemon {
    registry: Arc<ProviderRegistry>,
    sink: Option<Arc<InspectorSink>>,
    frontends: Frontends,
    root_symlinks: bool,
}

impl Daemon {
    pub fn new(
        registry: Arc<ProviderRegistry>,
        sink: Option<Arc<InspectorSink>>,
        frontends: Frontends,
        root_symlinks: bool,
    ) -> Self {
        Self {
            registry,
            sink,
            frontends,
            root_symlinks,
        }
    }

    pub fn mount_point(&self) -> &Path {
        self.frontends.mount_point()
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

    pub fn ready(&self) -> ReadyInfo {
        ReadyInfo {
            ready: self.frontends.serving().is_some(),
        }
    }

    pub fn status(&self) -> DaemonStatus {
        let root_mount = self.registry.root_mount_name();
        let mut mounts: Vec<MountInfo> = self
            .registry
            .runtime_entries()
            .into_iter()
            .map(|(mount, runtime)| MountInfo {
                root_mount: root_mount.as_deref() == Some(mount.as_str()),
                provider_id: runtime.provider_id().to_string(),
                mount,
            })
            .collect();
        mounts.sort_by(|a, b| a.mount.cmp(&b.mount));

        let dirs = self.registry.dirs();
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            mount_point: self.frontends.mount_point().to_path_buf(),
            config_dir: dirs.config_dir.to_path_buf(),
            cache_dir: dirs.cache_dir.to_path_buf(),
            providers_dir: dirs.providers_dir.to_path_buf(),
            frontend: self.frontends.serving(),
            mounts,
        }
    }

    /// Resolve and load one mount. Provider instantiation compiles WASM, so it
    /// runs on a blocking task, never on an API worker.
    pub async fn add_mount(
        self: &Arc<Self>,
        spec: Spec,
    ) -> Result<omnifs_api::MountInfo, crate::mounts::Error> {
        let mount = spec.mount.clone();
        let handle = tokio::runtime::Handle::current();
        let registry = Arc::clone(&self.registry);
        let runtime = tokio::task::spawn_blocking(move || registry.add_mount(spec, &handle))
            .await
            .map_err(|join_error| {
                warn!(%join_error, mount = mount.as_str(), "mount load task failed");
                crate::mounts::Error::TaskFailed
            })??;
        self.update_root_symlink(&mount, true);
        Ok(omnifs_api::MountInfo {
            root_mount: self.registry.root_mount_name().as_deref() == Some(mount.as_str()),
            provider_id: runtime.provider_id().to_string(),
            mount,
        })
    }

    /// Shut down and unregister one mount. Guest shutdown can block on an
    /// in-flight provider call, so it runs on a blocking task; the serving
    /// frontend is invalidated afterwards so the mount does not linger as a
    /// phantom directory.
    pub async fn remove_mount(self: &Arc<Self>, name: String) -> Result<(), crate::mounts::Error> {
        let registry = Arc::clone(&self.registry);
        let mount = name.clone();
        tokio::task::spawn_blocking(move || registry.remove_mount(&mount))
            .await
            .map_err(|join_error| {
                warn!(%join_error, mount = name.as_str(), "mount remove task failed");
                crate::mounts::Error::TaskFailed
            })??;
        self.frontends.invalidate_root_child(&name);
        self.update_root_symlink(&name, false);
        Ok(())
    }

    fn event_stream(&self) -> Response {
        let Some(sink) = self.sink.clone() else {
            return (
                StatusCode::NOT_FOUND,
                "inspector stream disabled (OMNIFS_INSPECTOR=0)\n",
            )
                .into_response();
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
        if !self.root_symlinks {
            return;
        }
        let mount_point = self.frontends.mount_point();
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

    fn router(state: Arc<Self>) -> Router {
        Router::new()
            .route("/v1/ready", get(ready))
            .route("/v1/version", get(version))
            .route("/v1/status", get(status))
            .route("/v1/mounts", axum::routing::post(add_mount))
            .route("/v1/mounts/{name}", axum::routing::delete(remove_mount))
            .route("/v1/events", get(events))
            .with_state(state)
    }
}

pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
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
        (status = 503, description = "filesystem frontend is not serving yet", body = ReadyInfo),
    ),
)]
async fn ready(State(daemon): State<Arc<Daemon>>) -> Response {
    let info = daemon.ready();
    let status = if info.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(info)).into_response()
}

#[utoipa::path(
    get,
    path = "/v1/version",
    operation_id = "version",
    responses((status = 200, description = "daemon control API version", body = VersionInfo)),
)]
async fn version() -> Json<VersionInfo> {
    Json(VersionInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        api_version: API_VERSION,
    })
}

#[utoipa::path(
    get,
    path = "/v1/status",
    operation_id = "status",
    responses((status = 200, description = "daemon runtime facts", body = DaemonStatus)),
)]
async fn status(State(daemon): State<Arc<Daemon>>) -> Json<DaemonStatus> {
    Json(daemon.status())
}

/// `POST /v1/mounts`: resolve and load one mount from a raw mount spec.
/// Secret material never rides this call: specs reference credential files
/// materialized into the session by the CLI.
#[utoipa::path(
    post,
    path = "/v1/mounts",
    operation_id = "add_mount",
    request_body(content = Spec, content_type = "application/json"),
    responses(
        (status = 201, description = "mount loaded", body = MountInfo),
        (status = 400, description = "invalid mount spec", content_type = "text/plain", body = String),
        (status = 404, description = "provider not found", content_type = "text/plain", body = String),
        (status = 409, description = "mount already loaded", content_type = "text/plain", body = String),
        (status = 500, description = "mount load failed", content_type = "text/plain", body = String),
    ),
)]
async fn add_mount(State(daemon): State<Arc<Daemon>>, Json(spec): Json<Spec>) -> Response {
    match daemon.add_mount(spec).await {
        Ok(info) => (StatusCode::CREATED, Json(info)).into_response(),
        Err(error) => error_response(&error),
    }
}

/// `DELETE /v1/mounts/{name}`: shut down and unregister a mount.
#[utoipa::path(
    delete,
    path = "/v1/mounts/{name}",
    operation_id = "remove_mount",
    params(("name" = String, Path, description = "mount name")),
    responses(
        (status = 204, description = "mount removed"),
        (status = 404, description = "mount not found", content_type = "text/plain", body = String),
        (status = 500, description = "mount remove failed", content_type = "text/plain", body = String),
    ),
)]
async fn remove_mount(
    State(daemon): State<Arc<Daemon>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    match daemon.remove_mount(name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(&error),
    }
}

fn error_response(error: &crate::mounts::Error) -> Response {
    let status = match error {
        crate::mounts::Error::Registry(registry) => match registry {
            RegistryError::ConfigError(_) => StatusCode::BAD_REQUEST,
            RegistryError::DuplicateMount(_) => StatusCode::CONFLICT,
            RegistryError::MountNotFound(_) | RegistryError::ProviderNotFound(_) => {
                StatusCode::NOT_FOUND
            },
            RegistryError::RuntimeError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        },
        crate::mounts::Error::TaskFailed => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, format!("{error}\n")).into_response()
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
        (status = 404, description = "inspector stream disabled", content_type = "text/plain", body = String),
    ),
)]
async fn events(State(daemon): State<Arc<Daemon>>) -> Response {
    daemon.event_stream()
}

fn record_line(record: &omnifs_inspector::InspectorRecord) -> Option<String> {
    match serialize_record(record) {
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
    #[test]
    fn checked_in_openapi_matches_implementation() {
        let checked_in: serde_json::Value =
            serde_json::from_str(include_str!("../../omnifs-api/openapi/daemon.json"))
                .expect("checked-in OpenAPI spec parses");
        let generated: serde_json::Value =
            serde_json::from_str(&super::openapi_json()).expect("generated OpenAPI spec parses");

        assert_eq!(checked_in, generated);
    }
}
