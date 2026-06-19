//! Control API server: `/v1/{ready,version,status,mounts,reconcile,shutdown,events}`.
//!
//! Serves daemon runtime facts, mount reconciliation and shutdown, and the
//! inspector event stream over HTTP on the control listener. See
//! `docs/design/daemon-cli-split.md`.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use omnifs_api::{
    API_VERSION, DaemonStatus, FrontendInfo, LaunchKind, MountFailure, MountInfo, ReadyInfo,
    ReconcileReport, StopReport, VersionInfo,
};
use omnifs_host::inspector::InspectorSink;
use omnifs_host::registry::ProviderRegistry;
use omnifs_inspector::serialize_record;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
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
    paths(
        ready,
        version,
        status,
        mounts_list,
        mount_inspect,
        reconcile,
        shutdown,
        events
    ),
    components(schemas(
        VersionInfo,
        ReadyInfo,
        DaemonStatus,
        FrontendInfo,
        LaunchKind,
        MountInfo,
        MountFailure,
        ReconcileReport,
        StopReport,
    ))
)]
struct ApiDoc;

pub struct Daemon {
    registry: Arc<ProviderRegistry>,
    sink: Option<Arc<InspectorSink>>,
    frontends: Frontends,
    root_symlinks: bool,
    /// Whether this daemon serves a host-native mount (preopens opened
    /// directly) versus a containerized one (preopens rewritten to bind paths).
    /// Selects the materialization mode for `POST /v1/reconcile` and the
    /// `LaunchKind` reported in status.
    host_native: bool,
    /// The last reconcile's failed mounts, surfaced in `status` so a dark mount
    /// is visible with its reason instead of silently absent.
    last_failed: std::sync::Mutex<Vec<MountFailure>>,
}

impl Daemon {
    pub fn new(
        registry: Arc<ProviderRegistry>,
        sink: Option<Arc<InspectorSink>>,
        frontends: Frontends,
        root_symlinks: bool,
        host_native: bool,
    ) -> Self {
        Self {
            registry,
            sink,
            frontends,
            root_symlinks,
            host_native,
            last_failed: std::sync::Mutex::new(Vec::new()),
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
        let identity = version_info();
        DaemonStatus {
            version: identity.version,
            api_version: identity.api_version,
            pid: identity.pid,
            executable: identity.executable,
            mount_point: self.frontends.mount_point().to_path_buf(),
            config_dir: dirs.config_dir.to_path_buf(),
            cache_dir: dirs.cache_dir.to_path_buf(),
            providers_dir: dirs.providers_dir.to_path_buf(),
            frontend: self.frontends.serving(),
            launch: if self.host_native {
                LaunchKind::HostNative
            } else {
                LaunchKind::Container
            },
            mounts,
            failed: self
                .last_failed
                .lock()
                .map(|failed| failed.clone())
                .unwrap_or_default(),
        }
    }

    /// Converge the running mount set to `mounts/*.json`, synchronously. Runs
    /// `registry.reconcile`, then reflects the result into the frontend: added
    /// and updated mounts (re)create the root symlink, removed and updated
    /// mounts invalidate the root child so a torn-down mount does not linger as
    /// a phantom directory. Callable directly from the blocking startup path; the
    /// `POST /v1/reconcile` handler wraps it in a blocking task.
    pub fn reconcile_blocking(&self, handle: &tokio::runtime::Handle) -> ReconcileReport {
        let outcome = self.registry.reconcile(handle, self.host_native);
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
        let failed: Vec<MountFailure> = outcome
            .failed
            .into_iter()
            .map(|failure| MountFailure {
                mount: failure.mount,
                reason: failure.reason,
            })
            .collect();
        // Remember the failures so `status` can show a dark mount and why,
        // instead of it simply being absent from `mounts`.
        if let Ok(mut last) = self.last_failed.lock() {
            *last = failed.clone();
        }
        ReconcileReport {
            added: outcome.added,
            updated: outcome.updated,
            removed: outcome.removed,
            failed,
        }
    }

    /// Reconcile on a blocking task, since it compiles WASM for added or changed
    /// mounts.
    pub async fn reconcile(self: &Arc<Self>) -> ReconcileReport {
        let daemon = Arc::clone(self);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || daemon.reconcile_blocking(&handle))
            .await
            .unwrap_or_else(|join_error| {
                warn!(%join_error, "reconcile task failed");
                ReconcileReport::default()
            })
    }

    /// Snapshot of what a shutdown will tear down, captured before unmounting.
    pub fn stop_report(&self) -> StopReport {
        StopReport {
            frontend: self.frontends.serving(),
            mount_point: self.frontends.mount_point().to_path_buf(),
            providers_dropped: self.registry.runtime_entries().len(),
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
            .route("/v1/mounts", get(mounts_list))
            .route("/v1/mounts/{name}", get(mount_inspect))
            .route("/v1/reconcile", axum::routing::post(reconcile))
            .route("/v1/shutdown", axum::routing::post(shutdown))
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
    Json(version_info())
}

fn version_info() -> VersionInfo {
    VersionInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        api_version: API_VERSION,
        pid: std::process::id(),
        executable: current_executable(),
    }
}

fn current_executable() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::new())
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

#[utoipa::path(
    get,
    path = "/v1/mounts",
    operation_id = "mounts_list",
    responses((status = 200, description = "loaded provider mounts", body = [MountInfo])),
)]
async fn mounts_list(State(daemon): State<Arc<Daemon>>) -> Json<Vec<MountInfo>> {
    Json(daemon.status().mounts)
}

#[utoipa::path(
    get,
    path = "/v1/mounts/{name}",
    operation_id = "mount_inspect",
    params(("name" = String, Path, description = "mount name")),
    responses(
        (status = 200, description = "the mount", body = MountInfo),
        (status = 404, description = "mount not found", content_type = "text/plain", body = String),
    ),
)]
async fn mount_inspect(
    State(daemon): State<Arc<Daemon>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    match daemon.status().mounts.into_iter().find(|m| m.mount == name) {
        Some(info) => Json(info).into_response(),
        None => (StatusCode::NOT_FOUND, format!("mount `{name}` not found\n")).into_response(),
    }
}

/// `POST /v1/reconcile`: converge the running mount set to `mounts/*.json`.
#[utoipa::path(
    post,
    path = "/v1/reconcile",
    operation_id = "reconcile",
    responses((status = 200, description = "what the reconcile changed", body = ReconcileReport)),
)]
async fn reconcile(State(daemon): State<Arc<Daemon>>) -> Json<ReconcileReport> {
    Json(daemon.reconcile().await)
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
    let report = daemon.stop_report();
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
