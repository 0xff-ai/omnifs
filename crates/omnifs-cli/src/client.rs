//! HTTP client for the daemon control API.
//!
//! The client only ever dials an endpoint within its own workspace: an endpoint
//! read from the runtime record (`$OMNIFS_HOME/daemon.json`), the fixed control
//! socket (`$OMNIFS_HOME/control.sock`). It never dials a default port blind,
//! so a daemon owned by a different `OMNIFS_HOME` is structurally
//! unaddressable.
//!
//! Resolution (per request, so the launcher can poll for the record to appear):
//! - read the record: a unix endpoint connects the socket; a refused/missing
//!   socket is a stale record, which is removed and reported.
//! - no record -> fall back to the fixed control socket if it exists on disk, so
//!   a daemon that outlived its record stays reachable; otherwise the daemon is
//!   not running (exit 3).
//! - the instance id echoed by `/v1/status` is asserted equal to the record's
//!   (the control-socket fallback carries no instance id, so this check is
//!   skipped for it), so a record overwritten by a restart mid-command is caught.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use hyperlocal::{UnixConnector, Uri as UnixUri};
use omnifs_api::{
    API_MAJOR, API_MINOR, ApiError, CredentialStatus, DaemonStatus, ErrorCode,
    FrontendAttachTargetReport, FrontendAttachTargetRequest, FrontendAttachTargetVsockReport,
    FrontendDelivery, StopReport,
};
use omnifs_workspace::authn::CredentialId;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::runtime_record::{Endpoint, RuntimeRecord};
use serde::de::DeserializeOwned;

use crate::error::{ExitCode, WithExitCode, WithHint};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Where and how to reach the daemon, resolved fresh for each request.
enum Target {
    /// No runtime record: the daemon is not running.
    Absent,
    /// Unix-socket endpoint read from the record (the daemon's only production
    /// transport).
    Unix {
        socket: PathBuf,
        instance: Option<String>,
    },
}

impl Target {
    /// A human label for the endpoint, used in error and diagnostic messages.
    fn label(&self) -> String {
        match self {
            Self::Absent => "the daemon".to_string(),
            Self::Unix { socket, .. } => format!("unix:{}", socket.display()),
        }
    }

    /// The record's instance id, for the mid-flight-overwrite assertion.
    fn instance(&self) -> Option<&str> {
        match self {
            Self::Unix { instance, .. } => instance.as_deref(),
            Self::Absent => None,
        }
    }
}

/// A buffered control-API response: status plus the whole body.
struct RawResponse {
    status: StatusCode,
    body: Bytes,
}

pub(crate) struct DaemonClient {
    /// Layout for record-based resolution. `None` only when the ambient
    /// workspace could not be resolved (then the client behaves as absent).
    record_path: Option<PathBuf>,
    /// The daemon's fixed control socket, dialed when the runtime record is
    /// absent. `None` in record-only test clients. See [`Self::resolve`].
    control_socket: Option<PathBuf>,
    unix: OnceLock<HyperClient<UnixConnector, Full<Bytes>>>,
    /// Set once a stale unix socket has been cleaned, so the unavailable error
    /// can report the record was removed.
    cleaned_stale: AtomicBool,
}

impl DaemonClient {
    pub(crate) fn for_layout(layout: &WorkspaceLayout) -> Self {
        Self {
            record_path: Some(layout.runtime_record_file()),
            control_socket: Some(layout.control_socket()),
            unix: OnceLock::new(),
            cleaned_stale: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn with_record_path(record_path: Option<PathBuf>) -> Self {
        Self {
            record_path,
            control_socket: None,
            unix: OnceLock::new(),
            cleaned_stale: AtomicBool::new(false),
        }
    }

    fn unix_client(&self) -> &HyperClient<UnixConnector, Full<Bytes>> {
        self.unix
            .get_or_init(|| HyperClient::builder(TokioExecutor::new()).build(UnixConnector))
    }

    /// Resolve the endpoint to dial for this request. A corrupt record cannot
    /// strand a daemon that is still
    /// serving on the workspace's fixed control socket, so resolution falls
    /// back to that socket without trusting any fields from the record.
    fn resolve(&self) -> Result<Target> {
        let record = match &self.record_path {
            Some(record_path) => match RuntimeRecord::read(record_path) {
                Ok(record) => record,
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    if let Some(target) = self.control_socket_target() {
                        return Ok(target);
                    }
                    self.clean_stale_record();
                    return Ok(Target::Absent);
                },
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("read runtime record {}", record_path.display()));
                },
            },
            None => None,
        };
        if let Some(record) = record {
            let Endpoint::Unix { path } = record.endpoint;
            return Ok(Target::Unix {
                socket: path,
                instance: Some(record.instance_id),
            });
        }
        // No runtime record: it was never written, or it was lost while the
        // daemon process lived (a crash or a botched teardown). The control
        // socket sits at a fixed path independent of the record, so probe it
        // directly. A live daemon that outlived its record is reached and
        // reported (so `up` refuses to spawn a second daemon onto the
        // same locked cache, and `down`/`status` can see it); a stale socket
        // file refuses the connection and resolves to absence.
        Ok(self.control_socket_target().unwrap_or(Target::Absent))
    }

    /// The fixed control socket as a dial target, when it exists on disk. The
    /// existence check keeps a fresh workspace (no socket yet) resolving to
    /// [`Target::Absent`] rather than dialing a path that was never bound.
    fn control_socket_target(&self) -> Option<Target> {
        let socket = self.control_socket.as_ref()?;
        socket.exists().then(|| Target::Unix {
            socket: socket.clone(),
            instance: None,
        })
    }

    /// The endpoint the inspector's event stream should attach to.
    pub(crate) fn event_endpoint(&self) -> Result<Option<EventEndpoint>> {
        Ok(match self.resolve()? {
            Target::Absent => None,
            Target::Unix { socket, .. } => Some(EventEndpoint::Unix { socket }),
        })
    }

    /// One request primitive over both transports. `Ok(None)` means the daemon
    /// is unreachable (connection refused/timeout, an absent record, or a stale
    /// unix socket that was cleaned). Other transport failures are errors.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> Result<Option<RawResponse>> {
        let target = self.resolve()?;
        match &target {
            Target::Absent => Ok(None),
            Target::Unix { socket, .. } => {
                self.request_unix(socket, method, path, body, timeout).await
            },
        }
    }

    async fn request_unix(
        &self,
        socket: &std::path::Path,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> Result<Option<RawResponse>> {
        let uri: hyper::Uri = UnixUri::new(socket, path).into();
        let body_bytes = match body {
            Some(value) => {
                Bytes::from(serde_json::to_vec(value).context("serialize request body")?)
            },
            None => Bytes::new(),
        };
        let mut builder = hyper::Request::builder().method(method.clone()).uri(uri);
        // Only claim a JSON content type when there is a body. A bodyless POST
        // with this header set anyway
        // makes axum's `Option<Json<T>>` extractor attempt (and fail) to parse
        // zero bytes as JSON, rejecting the request with 400 instead of the
        // `None` the handler expects for "no request body".
        if body.is_some() {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
        }
        let request = builder
            .body(Full::new(body_bytes))
            .context("build unix-socket request")?;

        let send = self.unix_client().request(request);
        let response = match tokio::time::timeout(timeout, send).await {
            // Timed out: treat the local control socket as unreachable.
            Err(_) => return Ok(None),
            Ok(Ok(response)) => response,
            // A connect error means the socket is gone or refused: the record is
            // stale. Remove it and report absence so the caller falls to the
            // offline path or the "not running" error.
            Ok(Err(error)) if error.is_connect() => {
                self.clean_stale_record();
                return Ok(None);
            },
            Ok(Err(error)) => {
                return Err(anyhow::anyhow!(
                    "request {method} over control socket {}: {error}",
                    socket.display()
                ));
            },
        };
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .with_context(|| {
                format!(
                    "read response body from control socket {}",
                    socket.display()
                )
            })?
            .to_bytes();
        Ok(Some(RawResponse { status, body }))
    }

    /// Remove a stale runtime record after a refused/missing unix socket, so the
    /// next command sees the daemon as absent rather than dialing a dead socket.
    fn clean_stale_record(&self) {
        if let Some(path) = &self.record_path {
            let _ = RuntimeRecord::remove(path);
        }
        self.cleaned_stale.store(true, Ordering::Relaxed);
    }

    /// The daemon-not-running error, tailored to whether a stale record was just
    /// cleaned. Exit code 3 (`DaemonUnavailable`).
    fn unavailable_error(&self) -> anyhow::Error {
        let message = if self.cleaned_stale.load(Ordering::Relaxed) {
            "daemon not running (cleaned up a stale record)"
        } else {
            "daemon not running"
        };
        match Err::<(), _>(anyhow::anyhow!("{message}")).with_exit_code(ExitCode::DaemonUnavailable)
        {
            Ok(()) => unreachable!("daemon unavailable construction always starts from Err"),
            Err(error) => error,
        }
    }

    /// Probe for a daemon and classify its control state in one step.
    async fn probe(&self) -> DaemonControlState {
        let target = match self.resolve() {
            Ok(target) => target,
            Err(error) => return DaemonControlState::Sick { error },
        };
        let raw = match self
            .request(Method::GET, "/v1/status", None, REQUEST_TIMEOUT)
            .await
        {
            Ok(Some(raw)) => raw,
            Ok(None) => return DaemonControlState::Absent,
            Err(error) => return DaemonControlState::Sick { error },
        };
        match self.status_from_raw(&raw, &target) {
            Ok(status) => DaemonControlState::from_status(status),
            Err(error) => DaemonControlState::Sick { error },
        }
    }

    /// Raw daemon status probe. Connection absence is `None`; a reachable
    /// daemon's HTTP status and JSON errors are propagated.
    pub(crate) async fn status_optional(&self) -> Result<Option<DaemonStatus>> {
        let target = self.resolve()?;
        let Some(raw) = self
            .request(Method::GET, "/v1/status", None, REQUEST_TIMEOUT)
            .await?
        else {
            return Ok(None);
        };
        self.status_from_raw(&raw, &target).map(Some)
    }

    /// Verify the daemon is reachable and speaks this CLI's control API.
    pub(crate) async fn require_compatible(&self) -> Result<DaemonStatus> {
        let label = self.resolve().map(|t| t.label()).unwrap_or_default();
        match self.probe().await.compatible_optional(&label)? {
            Some(status) => Ok(status),
            None => Err(self.unavailable_error()),
        }
    }

    /// Daemon status when a compatible daemon answers; `None` when no daemon
    /// answered.
    pub(crate) async fn compatible_status_optional(&self) -> Result<Option<DaemonStatus>> {
        let label = self.resolve().map(|t| t.label()).unwrap_or_default();
        self.probe().await.compatible_optional(&label)
    }

    /// Daemon runtime facts from a reachable, compatible daemon.
    pub(crate) async fn status(&self) -> Result<DaemonStatus> {
        match self.status_optional().await? {
            Some(status) => Ok(status),
            None => Err(self.unavailable_error()),
        }
    }

    /// Parse a `/v1/status` response, mapping HTTP errors to hints and asserting
    /// the daemon's instance id against the record we resolved from.
    fn status_from_raw(&self, raw: &RawResponse, target: &Target) -> Result<DaemonStatus> {
        let status: DaemonStatus = Self::parse_ok_json(raw, "daemon status request failed")?;
        self.verify_instance(&status, target)?;
        Ok(status)
    }

    /// Assert the connected daemon's instance id matches the record we dialed.
    /// On mismatch (a restart overwrote the record mid-command), re-read the
    /// record once; if it has caught up to the live daemon, accept, else error.
    fn verify_instance(&self, status: &DaemonStatus, target: &Target) -> Result<()> {
        let Some(expected) = target.instance() else {
            return Ok(());
        };
        if status.instance_id == expected {
            return Ok(());
        }
        if let Some(path) = &self.record_path
            && let Ok(Some(record)) = RuntimeRecord::read(path)
            && record.instance_id == status.instance_id
        {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "the daemon runtime record was overwritten mid-command \
             (daemon instance {}, record instance {expected}); rerun the command",
            status.instance_id,
        ))
    }

    /// Deserialize a successful JSON response, mapping a non-success status to an
    /// `ApiError`-derived error with hints.
    fn parse_ok_json<T: DeserializeOwned>(raw: &RawResponse, context: &'static str) -> Result<T> {
        if raw.status.is_success() {
            return serde_json::from_slice(&raw.body).context("parse daemon response JSON");
        }
        Err(Self::error_from_body(raw, context))
    }

    fn error_from_body(raw: &RawResponse, context: &'static str) -> anyhow::Error {
        match serde_json::from_slice::<ApiError>(&raw.body) {
            Ok(api_error) => Self::api_error(context, &api_error),
            Err(error) => anyhow::anyhow!(
                "{context}: daemon returned {} with invalid ApiError JSON: {error}",
                raw.status
            ),
        }
    }

    fn api_error(context: &'static str, api_error: &ApiError) -> anyhow::Error {
        let error = anyhow::anyhow!("{context}: {}", api_error.message);
        match Err::<(), _>(error)
            .with_hint(hint_for(api_error.code))
            .with_exit_code(exit_code_for(api_error.code))
        {
            Ok(()) => unreachable!("api error construction always starts from Err"),
            Err(error) => error,
        }
    }

    /// Fetch the TCP attach target a frontend dials, binding the daemon's
    /// listener if needed (idempotent: a repeat call returns the already-bound
    /// address and token). Native Linux supplies its Docker bridge gateway;
    /// Docker Desktop uses the default loopback bind.
    pub(crate) async fn frontend_attach_target(
        &self,
        bind_ip: Option<std::net::Ipv4Addr>,
    ) -> Result<FrontendAttachTargetReport> {
        let body = serde_json::to_value(FrontendAttachTargetRequest {
            bind_ip,
            // The only driver this route serves today; the daemon rejects
            // anything else with a 400 (krunkit attaches over vsock instead,
            // through the separate `.../attach-target/vsock` route).
            driver: FrontendDelivery::Docker,
        })
        .context("serialize attach-target request")?;
        let raw = self
            .request(
                Method::POST,
                "/v1/frontend/attach-target",
                Some(&body),
                REQUEST_TIMEOUT,
            )
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon attach-target request failed")
    }

    /// Fetch the vsock attach target a frontend dials, binding the daemon's
    /// token-checking UDS namespace attach listener if needed (idempotent: a
    /// repeat call returns the already-bound path and token). This is the
    /// krunkit-on-macOS path: the guest dials host vsock and krunkit proxies
    /// every connection onto this socket, so `token` (not filesystem
    /// permissions) authenticates every attach handshake it carries.
    pub(crate) async fn frontend_attach_target_vsock(
        &self,
    ) -> Result<FrontendAttachTargetVsockReport> {
        let raw = self
            .request(
                Method::POST,
                "/v1/frontend/attach-target/vsock",
                None,
                REQUEST_TIMEOUT,
            )
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon attach-target vsock request failed")
    }

    pub(crate) async fn reload_credential_if_ready(
        &self,
        id: &CredentialId,
    ) -> Result<Option<CredentialStatus>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let path = format!("/v1/credentials/{id}/reload");
        let raw = self
            .request(Method::POST, &path, None, REQUEST_TIMEOUT)
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon credential reload request failed").map(Some)
    }

    /// Ask the daemon to unmount its frontend and exit. `None` when no daemon
    /// answered, so the caller can fall back to a stale-mount sweep.
    pub(crate) async fn shutdown(&self) -> Result<Option<StopReport>> {
        let Some(raw) = self
            .request(Method::POST, "/v1/shutdown", None, REQUEST_TIMEOUT)
            .await?
        else {
            return Ok(None);
        };
        Self::parse_ok_json(&raw, "daemon shutdown request failed").map(Some)
    }

    /// True once the daemon reports the filesystem is serving.
    pub(crate) async fn ready(&self) -> bool {
        matches!(
            self.request(Method::GET, "/v1/ready", None, REQUEST_TIMEOUT).await,
            Ok(Some(raw)) if raw.status.is_success()
        )
    }
}

/// The endpoint the inspector's `GET /v1/events` stream should attach to.
/// `None` means no daemon is running.
#[derive(Clone)]
pub(crate) enum EventEndpoint {
    Unix { socket: PathBuf },
}

impl EventEndpoint {
    /// A short human label for status lines in the inspector.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Unix { socket } => format!("unix:{}", socket.display()),
        }
    }
}

#[derive(Debug)]
pub(crate) enum DaemonControlState {
    Absent,
    /// A daemon answered but its status could not be read.
    Sick {
        error: anyhow::Error,
    },
    Incompatible(Box<DaemonStatus>),
    Compatible(Box<DaemonStatus>),
}

impl DaemonControlState {
    fn from_status(status: DaemonStatus) -> Self {
        if status.api_major == API_MAJOR {
            Self::Compatible(Box::new(status))
        } else {
            Self::Incompatible(Box::new(status))
        }
    }

    fn compatible_optional(self, label: &str) -> Result<Option<DaemonStatus>> {
        match self {
            Self::Compatible(status) => Ok(Some(*status)),
            Self::Absent => Ok(None),
            Self::Sick { error } => Err(error.context(if label.is_empty() {
                "a daemon answered, but its status could not be read".to_string()
            } else {
                format!("a daemon answered at {label}, but its status could not be read")
            })),
            Self::Incompatible(status) => Err(incompatible_daemon_error(&status)),
        }
    }
}

fn hint_for(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::AuthRequired => "Try: omnifs mount reauth <name>",
        ErrorCode::CredentialNotFound => "Try: omnifs mount reauth <mount>",
        ErrorCode::DaemonShuttingDown => "Try: omnifs up",
        ErrorCode::MountNotFound => "Try: omnifs status",
        ErrorCode::SpecInvalid => "Try: inspect the mount spec and rerun omnifs up",
        ErrorCode::Internal => "Try: omnifs doctor",
    }
}

fn exit_code_for(code: ErrorCode) -> ExitCode {
    match code {
        ErrorCode::AuthRequired | ErrorCode::CredentialNotFound => ExitCode::AuthRequired,
        ErrorCode::DaemonShuttingDown => ExitCode::DaemonUnavailable,
        ErrorCode::MountNotFound | ErrorCode::SpecInvalid | ErrorCode::Internal => {
            ExitCode::GenericFailure
        },
    }
}

fn incompatible_daemon_error(status: &DaemonStatus) -> anyhow::Error {
    let detail = if status.api_major == 0 {
        "this daemon predates major/minor API versioning".to_string()
    } else {
        format!(
            "daemon speaks control API v{}.{}",
            status.api_major, status.api_minor
        )
    };
    anyhow::anyhow!(
        "{detail}; this CLI speaks v{API_MAJOR}.{API_MINOR} (daemon binary v{}). \
         Stop it with `omnifs down`, or upgrade the runtime image so the CLI and \
         daemon versions match, then rerun.",
        status.version,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_table_covers_every_error_code() {
        assert_eq!(
            hint_for(ErrorCode::AuthRequired),
            "Try: omnifs mount reauth <name>"
        );
        assert_eq!(
            hint_for(ErrorCode::CredentialNotFound),
            "Try: omnifs mount reauth <mount>"
        );
        assert_eq!(hint_for(ErrorCode::DaemonShuttingDown), "Try: omnifs up");
        assert_eq!(hint_for(ErrorCode::Internal), "Try: omnifs doctor");
    }

    /// With no record and no control socket, the client is absent and require exits 3.
    #[tokio::test]
    async fn absent_record_is_daemon_unavailable() {
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        let client = DaemonClient::with_record_path(Some(record));
        assert!(client.status_optional().await.unwrap().is_none());
        let error = client.require_compatible().await.unwrap_err();
        assert_eq!(crate::error::exit_code(&error), ExitCode::DaemonUnavailable);
    }

    fn client_without_record(record: PathBuf, control_socket: PathBuf) -> DaemonClient {
        DaemonClient {
            record_path: Some(record),
            control_socket: Some(control_socket),
            unix: OnceLock::new(),
            cleaned_stale: AtomicBool::new(false),
        }
    }

    /// A daemon that outlived its runtime record still binds the fixed control
    /// socket. With the record gone, `resolve` must fall back to that socket so
    /// the daemon stays visible (rather than resolving to absence and letting a
    /// second daemon spawn onto the same locked cache).
    #[test]
    fn resolve_falls_back_to_control_socket_when_record_is_absent() {
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("control.sock");
        std::fs::write(&socket, b"").unwrap();
        let client = client_without_record(home.path().join("daemon.json"), socket.clone());
        match client.resolve().unwrap() {
            Target::Unix {
                socket: dialed,
                instance,
            } => {
                assert_eq!(dialed, socket);
                assert!(instance.is_none(), "fallback carries no record instance id");
            },
            _ => panic!("expected the control-socket fallback target"),
        }
    }

    /// A fresh workspace has neither a record nor a bound control socket: the
    /// fallback must not dial a path that was never created.
    #[test]
    fn resolve_is_absent_without_record_or_control_socket() {
        let home = tempfile::tempdir().unwrap();
        let client = client_without_record(
            home.path().join("daemon.json"),
            home.path().join("control.sock"),
        );
        assert!(matches!(client.resolve().unwrap(), Target::Absent));
    }

    #[test]
    fn corrupt_runtime_record_falls_back_to_fixed_control_socket() {
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        let socket = home.path().join("control.sock");
        std::fs::write(&record, b"not json").unwrap();
        std::fs::write(&socket, b"reserved").unwrap();
        let client = client_without_record(record, socket.clone());
        assert!(matches!(
            client.resolve().unwrap(),
            Target::Unix {
                socket: dialed,
                instance: None
            } if dialed == socket
        ));
    }

    #[test]
    fn corrupt_runtime_record_without_socket_is_cleaned_and_absent() {
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        std::fs::write(&record, b"not json").unwrap();
        let client = client_without_record(record.clone(), home.path().join("control.sock"));
        assert!(matches!(client.resolve().unwrap(), Target::Absent));
        assert!(!record.exists());
    }

    #[test]
    fn unreadable_runtime_record_io_error_is_not_treated_as_stale() {
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        std::fs::create_dir(&record).unwrap();
        let socket = home.path().join("control.sock");
        std::fs::write(&socket, b"reserved").unwrap();
        let client = client_without_record(record.clone(), socket);
        let Err(error) = client.resolve() else {
            panic!("directory runtime record must not resolve via socket fallback");
        };
        assert!(format!("{error:#}").contains("read runtime record"));
        assert!(record.exists(), "unreadable state must not be removed");
    }
}
