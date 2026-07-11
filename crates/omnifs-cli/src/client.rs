//! HTTP client for the daemon control API.
//!
//! The client only ever dials an endpoint it read from its own workspace's
//! runtime record (`$OMNIFS_HOME/daemon.json`), or an explicit
//! `OMNIFS_DAEMON_ADDR`. It never dials a default port blind, so a daemon owned
//! by a different `OMNIFS_HOME` is structurally unaddressable.
//!
//! Resolution (per request, so the launcher can poll for the record to appear):
//! - `OMNIFS_DAEMON_ADDR` set: dial TCP, bearer token from `OMNIFS_CONTROL_TOKEN`
//!   when set (the debug/test path; the ordinary host-native daemon never sets
//!   this).
//! - else read the record:
//!   - absent -> the daemon is not running (exit 3).
//!   - unix endpoint -> connect the socket; a refused/missing socket is a stale
//!     record, which is removed and reported.
//! - the instance id echoed by `/v1/status` is asserted equal to the record's,
//!   so a record overwritten by a restart mid-command is caught.

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
    MountReport, MountUpdateRequest, ProviderSummary, ReconcileReport, StopReport, UpgradeDelta,
};
use omnifs_workspace::authn::CredentialId;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::{Spec, UpgradePlan};
use omnifs_workspace::runtime_record::{Endpoint, RuntimeRecord};
use serde::de::DeserializeOwned;

use crate::error::{ExitCode, WithExitCode, WithHint};

const EXPORT_API_MINOR: u16 = 2;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Where and how to reach the daemon, resolved fresh for each request.
enum Target {
    /// No runtime record and no `OMNIFS_DAEMON_ADDR`: the daemon is not running.
    Absent,
    /// `OMNIFS_DAEMON_ADDR` override (debug/test path). Foreign-daemon copy
    /// applies only on this path.
    Env { base: String, token: Option<String> },
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
            Self::Env { base, .. } => base.clone(),
            Self::Unix { socket, .. } => format!("unix:{}", socket.display()),
        }
    }

    /// The record's instance id, for the mid-flight-overwrite assertion.
    fn instance(&self) -> Option<&str> {
        match self {
            Self::Unix { instance, .. } => instance.as_deref(),
            Self::Absent | Self::Env { .. } => None,
        }
    }
}

/// A buffered control-API response: status plus the whole body. Every daemon
/// method reads a bounded JSON or tar payload, so buffering keeps one primitive
/// over both transports.
struct RawResponse {
    status: StatusCode,
    body: Bytes,
}

pub(crate) struct DaemonClient {
    /// Layout for record-based resolution. `None` only when the ambient
    /// workspace could not be resolved (then the client behaves as absent).
    record_path: Option<PathBuf>,
    unix: OnceLock<HyperClient<UnixConnector, Full<Bytes>>>,
    /// Set once a stale unix socket has been cleaned, so the unavailable error
    /// can report the record was removed.
    cleaned_stale: AtomicBool,
}

impl DaemonClient {
    pub(crate) fn for_layout(layout: &WorkspaceLayout) -> Self {
        Self::with_record_path(Some(layout.runtime_record_file()))
    }

    fn with_record_path(record_path: Option<PathBuf>) -> Self {
        Self {
            record_path,
            unix: OnceLock::new(),
            cleaned_stale: AtomicBool::new(false),
        }
    }

    /// Build the TCP control-client, on demand, only for the
    /// `OMNIFS_DAEMON_ADDR` override path. The host-native daemon's only
    /// production transport is the unix socket (`unix_client`, no TLS
    /// backend involved), so this is never constructed for `status`,
    /// `down`, `shell`, or any other command that resolves the daemon
    /// through the runtime record.
    ///
    /// Building a `reqwest::Client` initializes its TLS backend, which
    /// probes the system certificate store; on a CA-less minimal Linux that
    /// probe can fail. Building it here, lazily, turns that failure into an
    /// actionable error at the one call site that actually needs TLS,
    /// instead of a startup panic for every command.
    fn http_client() -> Result<reqwest::Client> {
        Self::build_http_client(
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT),
        )
    }

    /// The `.build()` call this wraps is where a CA-less system fails: it
    /// constructs the TLS backend (rustls-platform-verifier loads the
    /// system root store), and `build()` returns `Err` rather than
    /// panicking when that store is empty. Split out from `http_client` so
    /// the failure path is exercisable with an arbitrary builder in tests,
    /// without depending on an actually CA-less host.
    fn build_http_client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client> {
        builder.build().context(
            "build TLS-capable HTTP client for OMNIFS_DAEMON_ADDR; \
             no system certificate authorities found, install ca-certificates \
             (the host-native daemon's unix-socket transport does not need this)",
        )
    }

    fn unix_client(&self) -> &HyperClient<UnixConnector, Full<Bytes>> {
        self.unix
            .get_or_init(|| HyperClient::builder(TokioExecutor::new()).build(UnixConnector))
    }

    /// Resolve the endpoint to dial for this request. `OMNIFS_DAEMON_ADDR` wins
    /// over the record; an unparseable record is a hard error.
    fn resolve(&self) -> Result<Target> {
        if let Some(addr) = env_daemon_addr() {
            let token = std::env::var("OMNIFS_CONTROL_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty());
            return Ok(Target::Env {
                base: format!("http://{addr}"),
                token,
            });
        }
        let Some(record_path) = &self.record_path else {
            return Ok(Target::Absent);
        };
        match RuntimeRecord::read(record_path)
            .with_context(|| format!("read runtime record {}", record_path.display()))?
        {
            None => Ok(Target::Absent),
            Some(record) => {
                let Endpoint::Unix { path } = record.endpoint;
                Ok(Target::Unix {
                    socket: path,
                    instance: Some(record.instance_id),
                })
            },
        }
    }

    /// The endpoint the inspector's event stream should attach to.
    pub(crate) fn event_endpoint(&self) -> Result<Option<EventEndpoint>> {
        Ok(match self.resolve()? {
            Target::Absent => None,
            Target::Env { base, token } => Some(EventEndpoint::Tcp { base, token }),
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
            Target::Env { base, token } => {
                self.request_tcp(base, token.as_deref(), method, path, body, timeout)
                    .await
            },
            Target::Unix { socket, .. } => {
                self.request_unix(socket, method, path, body, timeout).await
            },
        }
    }

    async fn request_tcp(
        &self,
        base: &str,
        token: Option<&str>,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> Result<Option<RawResponse>> {
        let http = Self::http_client()?;
        let mut builder = http
            .request(method.clone(), format!("{base}{path}"))
            .timeout(timeout);
        if let Some(token) = token {
            builder = builder.bearer_auth(token);
        }
        if let Some(body) = body {
            builder = builder.json(body);
        }
        match builder.send().await {
            Ok(response) => {
                let status = response.status();
                let body = response
                    .bytes()
                    .await
                    .with_context(|| format!("read response body from {base}{path}"))?;
                Ok(Some(RawResponse { status, body }))
            },
            Err(error) if error.is_connect() || error.is_timeout() => Ok(None),
            Err(error) => Err(error).with_context(|| format!("request {method} {base}{path}")),
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
        // (e.g. `reconcile()`'s no-argument call) with this header set anyway
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
            // Timed out: treat as unreachable, same as the TCP path.
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
        let state = match self.status_from_raw(&raw, &target) {
            Ok(status) => DaemonControlState::from_status(status),
            Err(error) => DaemonControlState::Sick { error },
        };
        state.warn_minor_skew();
        state
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

    /// Converge the running daemon's mount set to the on-disk desired state.
    pub(crate) async fn reconcile(&self) -> Result<ReconcileReport> {
        let raw = self
            .request(Method::POST, "/v1/reconcile", None, Duration::from_mins(3))
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon reconcile request failed")
    }

    /// Fetch the TCP attach target a frontend dials, binding the daemon's
    /// listener if needed (idempotent: a repeat call returns the already-bound
    /// address and token). Native Linux supplies its Docker bridge gateway;
    /// Docker Desktop uses the default loopback bind.
    pub(crate) async fn frontend_attach_target(
        &self,
        bind_ip: Option<std::net::Ipv4Addr>,
    ) -> Result<FrontendAttachTargetReport> {
        let body = serde_json::to_value(FrontendAttachTargetRequest { bind_ip })
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

    pub(crate) async fn create_mount_if_ready(&self, spec: &Spec) -> Result<Option<MountReport>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let body = serde_json::to_value(spec).context("serialize mount spec")?;
        let raw = self
            .request(
                Method::POST,
                "/v1/mounts",
                Some(&body),
                Duration::from_mins(3),
            )
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon mount create request failed").map(Some)
    }

    pub(crate) async fn update_mount_if_ready(
        &self,
        spec: &Spec,
        approved: Option<&UpgradePlan>,
    ) -> Result<Option<MountReport>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let request = MountUpdateRequest {
            spec: serde_json::to_value(spec).context("serialize mount spec")?,
            approved: approved.map(upgrade_delta_to_api).transpose()?,
        };
        let body = serde_json::to_value(&request).context("serialize mount update request")?;
        let path = format!("/v1/mounts/{}", spec.mount);
        let raw = self
            .request(Method::PUT, &path, Some(&body), Duration::from_mins(3))
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon mount update request failed").map(Some)
    }

    pub(crate) async fn delete_mount_if_ready(&self, mount: &str) -> Result<Option<MountReport>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let path = format!("/v1/mounts/{mount}");
        let raw = self
            .request(Method::DELETE, &path, None, Duration::from_mins(3))
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon mount delete request failed").map(Some)
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

    pub(crate) async fn providers_if_ready(&self) -> Result<Option<Vec<ProviderSummary>>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let raw = self
            .request(Method::GET, "/v1/providers", None, REQUEST_TIMEOUT)
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon providers request failed").map(Some)
    }

    pub(crate) async fn credentials_if_ready(&self) -> Result<Option<Vec<CredentialStatus>>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let raw = self
            .request(Method::GET, "/v1/credentials", None, REQUEST_TIMEOUT)
            .await?
            .ok_or_else(|| self.unavailable_error())?;
        Self::parse_ok_json(&raw, "daemon credentials request failed").map(Some)
    }

    /// Export a mount snapshot tar only when a compatible daemon is running.
    pub(crate) async fn export_mount_if_running(&self, mount: &str) -> Result<Option<Vec<u8>>> {
        let Some(status) = self.compatible_status_optional().await? else {
            return Ok(None);
        };
        if status.api_minor < EXPORT_API_MINOR {
            return Ok(None);
        }
        let path = format!("/v1/mounts/{mount}/export");
        let Some(raw) = self
            .request(Method::GET, &path, None, Duration::from_mins(1))
            .await?
        else {
            return Ok(None);
        };
        if !raw.status.is_success() {
            return Err(Self::error_from_body(
                &raw,
                "daemon snapshot export request failed",
            ));
        }
        Ok(Some(raw.body.to_vec()))
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

/// Read `OMNIFS_DAEMON_ADDR` from the environment. There is no default port:
/// an unset value means "use the runtime record".
pub(crate) fn env_daemon_addr() -> Option<String> {
    std::env::var("OMNIFS_DAEMON_ADDR")
        .ok()
        .map(|addr| addr.trim().to_string())
        .filter(|addr| !addr.is_empty())
}

/// The endpoint the inspector's `GET /v1/events` stream should attach to, in the
/// same resolution order as the control client. `None` means no daemon is
/// running (no record, no override).
#[derive(Clone)]
pub(crate) enum EventEndpoint {
    Tcp { base: String, token: Option<String> },
    Unix { socket: PathBuf },
}

impl EventEndpoint {
    /// A short human label for status lines in the inspector.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Tcp { base, .. } => base.clone(),
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

    fn warn_minor_skew(&self) {
        let Self::Compatible(status) = self else {
            return;
        };
        if status.api_minor != API_MINOR {
            anstream::eprintln!(
                "note: daemon API minor v{}.{}, CLI expects v{API_MAJOR}.{API_MINOR}; \
                 proceeding (minor skew is non-breaking)",
                status.api_major,
                status.api_minor,
            );
        }
    }

    fn compatible_optional(self, label: &str) -> Result<Option<DaemonStatus>> {
        match self {
            Self::Compatible(status) => Ok(Some(*status)),
            Self::Absent => Ok(None),
            Self::Sick { error } => Err(error.context(format!(
                "a daemon answered at {label}, but its status could not be read"
            ))),
            Self::Incompatible(status) => Err(incompatible_daemon_error(&status)),
        }
    }
}

/// A daemon is answering `OMNIFS_DAEMON_ADDR` but will not accept this
/// workspace's credentials. This survives only on the explicit override path:
/// it is almost always a daemon owned by a different `OMNIFS_HOME`.
pub(crate) fn foreign_daemon_error(base: &str) -> anyhow::Error {
    match Err::<(), _>(anyhow::anyhow!(
        "a daemon is serving at {base}, but it does not accept this workspace's credentials"
    ))
    .with_exit_code(ExitCode::AuthRequired)
    .with_hint(
        "It likely belongs to a different OMNIFS_HOME (for example the `just dev` sandbox at ~/.omnifs-dev, or another worktree)",
    )
    .with_hint(
        "Stop it with `omnifs down` from the workspace that owns it, or point this CLI elsewhere by setting OMNIFS_DAEMON_ADDR",
    ) {
        Ok(()) => unreachable!("foreign daemon error construction always starts from Err"),
        Err(error) => error,
    }
}

fn upgrade_delta_to_api(plan: &UpgradePlan) -> Result<UpgradeDelta> {
    serde_json::from_value(serde_json::to_value(plan).context("serialize approved upgrade delta")?)
        .context("convert approved upgrade delta to API DTO")
}

fn hint_for(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::AuthRequired | ErrorCode::ConsentRequired => "Try: omnifs mounts reauth <name>",
        ErrorCode::CredentialNotFound => "Try: omnifs mounts reauth <mount>",
        ErrorCode::MountNotFound => "Try: omnifs mounts ls",
        ErrorCode::SpecInvalid => {
            "Try: edit the mount spec or recreate it with `omnifs init <provider> --as <name>`"
        },
        ErrorCode::ProviderMissing => "Try: just providers build",
        ErrorCode::ReconcileBusy => "Try: rerun the command after reconcile finishes",
        ErrorCode::Unauthorized => {
            "Try: a daemon on this control address rejected this workspace's credentials; \
             run `omnifs down` from the workspace that owns it, or set OMNIFS_DAEMON_ADDR to point elsewhere"
        },
        ErrorCode::DaemonShuttingDown => "Try: omnifs up",
        ErrorCode::Internal => "Try: omnifs doctor",
    }
}

fn exit_code_for(code: ErrorCode) -> ExitCode {
    match code {
        ErrorCode::Unauthorized
        | ErrorCode::AuthRequired
        | ErrorCode::ConsentRequired
        | ErrorCode::CredentialNotFound => ExitCode::AuthRequired,
        ErrorCode::DaemonShuttingDown => ExitCode::DaemonUnavailable,
        ErrorCode::MountNotFound
        | ErrorCode::SpecInvalid
        | ErrorCode::ProviderMissing
        | ErrorCode::ReconcileBusy
        | ErrorCode::Internal => ExitCode::GenericFailure,
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
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// Serializes tests that set `OMNIFS_DAEMON_ADDR`/`OMNIFS_CONTROL_TOKEN`
    /// (process-global state) and restores their prior values on drop. This is
    /// the only transport a test can force onto without a real runtime record:
    /// the daemon's production transport is a Unix socket a live daemon binds,
    /// which these unit tests don't spin up.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        #[allow(unsafe_code)] // env::set_var requires unsafe; guarded by ENV_LOCK.
        fn set(vars: &[(&'static str, &str)]) -> Self {
            let vars: Vec<(&'static str, Option<&str>)> = vars
                .iter()
                .map(|(key, value)| (*key, Some(*value)))
                .collect();
            Self::apply(&vars)
        }

        /// Force `OMNIFS_DAEMON_ADDR`/`OMNIFS_CONTROL_TOKEN` unset for the
        /// guard's lifetime, so a test asserting on the record-only path is not
        /// racing another test's env-forced target.
        #[allow(unsafe_code)] // env::remove_var requires unsafe; guarded by ENV_LOCK.
        fn unset_daemon_addr() -> Self {
            Self::apply(&[("OMNIFS_DAEMON_ADDR", None), ("OMNIFS_CONTROL_TOKEN", None)])
        }

        #[allow(unsafe_code)] // env::set_var/remove_var require unsafe; guarded by ENV_LOCK.
        fn apply(vars: &[(&'static str, Option<&str>)]) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = vars
                .iter()
                .map(|(key, _)| (*key, std::env::var(*key).ok()))
                .collect();
            // SAFETY: ENV_LOCK is held for the guard's whole lifetime.
            for (key, value) in vars {
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        #[allow(unsafe_code)] // env::set_var/remove_var require unsafe; guarded by ENV_LOCK.
        fn drop(&mut self) {
            // SAFETY: ENV_LOCK is still held (it is a field on `self`).
            for (key, original) in &self.saved {
                match original {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    #[tokio::test]
    async fn status_optional_attaches_control_token_header() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let lower = request.to_ascii_lowercase();
            assert!(
                lower.contains("\r\nauthorization: bearer test-token\r\n"),
                "status request must carry bearer token, got:\n{request}"
            );
            let response = json_response(&status_body("test-daemon", API_MAJOR, API_MINOR));
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let _env = EnvGuard::set(&[
            ("OMNIFS_DAEMON_ADDR", &addr.to_string()),
            ("OMNIFS_CONTROL_TOKEN", "test-token"),
        ]);
        let client = DaemonClient::with_record_path(None);
        let status = client.status_optional().await.unwrap().unwrap();
        assert_eq!(status.version, "test-daemon");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn status_optional_maps_api_errors_to_hints() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let response = if request.starts_with("GET /v1/status ") {
                api_error_response(
                    "401 Unauthorized",
                    &ApiError {
                        code: ErrorCode::AuthRequired,
                        message: "credential required".to_string(),
                        detail: None,
                    },
                )
            } else {
                api_error_response(
                    "404 Not Found",
                    &ApiError {
                        code: ErrorCode::Internal,
                        message: "not found".to_string(),
                        detail: None,
                    },
                )
            };
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let _env = EnvGuard::set(&[
            ("OMNIFS_DAEMON_ADDR", &addr.to_string()),
            ("OMNIFS_CONTROL_TOKEN", "test-token"),
        ]);
        let client = DaemonClient::with_record_path(None);

        let error = client.status_optional().await.unwrap_err();
        let rendered = crate::error::render(&error);
        assert!(rendered.contains("daemon status request failed: credential required"));
        assert!(rendered.contains("Try: omnifs mounts reauth <name>"));
        server.await.unwrap();
    }

    #[test]
    fn hint_table_covers_every_error_code() {
        assert!(hint_for(ErrorCode::Unauthorized).contains("omnifs down"));
        assert!(hint_for(ErrorCode::Unauthorized).contains("OMNIFS_DAEMON_ADDR"));
        assert_eq!(
            hint_for(ErrorCode::AuthRequired),
            "Try: omnifs mounts reauth <name>"
        );
        assert_eq!(
            hint_for(ErrorCode::ConsentRequired),
            "Try: omnifs mounts reauth <name>"
        );
        assert_eq!(
            hint_for(ErrorCode::CredentialNotFound),
            "Try: omnifs mounts reauth <mount>"
        );
        assert_eq!(hint_for(ErrorCode::MountNotFound), "Try: omnifs mounts ls");
        assert_eq!(
            hint_for(ErrorCode::SpecInvalid),
            "Try: edit the mount spec or recreate it with `omnifs init <provider> --as <name>`"
        );
        assert_eq!(
            hint_for(ErrorCode::ProviderMissing),
            "Try: just providers build"
        );
        assert_eq!(
            hint_for(ErrorCode::ReconcileBusy),
            "Try: rerun the command after reconcile finishes"
        );
        assert_eq!(hint_for(ErrorCode::DaemonShuttingDown), "Try: omnifs up");
        assert_eq!(hint_for(ErrorCode::Internal), "Try: omnifs doctor");
    }

    /// A daemon reporting a different major must be refused.
    #[tokio::test]
    async fn probe_refuses_on_major_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let read = stream.read(&mut request).await.unwrap();
            let _ = String::from_utf8_lossy(&request[..read]);
            let response = json_response(&status_body("old-daemon", API_MAJOR + 1, 0));
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let _env = EnvGuard::set(&[
            ("OMNIFS_DAEMON_ADDR", &addr.to_string()),
            ("OMNIFS_CONTROL_TOKEN", "test-token"),
        ]);
        let client = DaemonClient::with_record_path(None);
        let err = client.require_compatible().await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("control API"),
            "error should mention control API mismatch: {msg}"
        );
    }

    /// A daemon reporting the same major but a different minor must proceed.
    #[tokio::test]
    async fn probe_proceeds_on_minor_skew() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let read = stream.read(&mut request).await.unwrap();
            let _ = String::from_utf8_lossy(&request[..read]);
            let response = json_response(&status_body("newer-daemon", API_MAJOR, API_MINOR + 1));
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let _env = EnvGuard::set(&[
            ("OMNIFS_DAEMON_ADDR", &addr.to_string()),
            ("OMNIFS_CONTROL_TOKEN", "test-token"),
        ]);
        let client = DaemonClient::with_record_path(None);
        assert!(matches!(
            client.probe().await,
            DaemonControlState::Compatible(_)
        ));
    }

    /// A CA-less Linux fails inside `ClientBuilder::build()` before any
    /// network I/O (rustls-platform-verifier's `Verifier::new` returns
    /// `rustls::Error::General("No CA certificates were loaded from the
    /// system")` when the native root store comes back empty). An empty
    /// root store isn't reproducible on this platform, so this forces the
    /// sibling rejection `build()` makes at the same step (hostname
    /// verification requires `tls_certs_only()`) to prove `build_http_client`
    /// turns a `build()` failure into a `Result`, never a panic, and that the
    /// error is actionable.
    #[test]
    fn build_http_client_surfaces_tls_backend_failure_as_error_not_panic() {
        let builder = reqwest::Client::builder().danger_accept_invalid_hostnames(true);
        let error = DaemonClient::build_http_client(builder).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("build TLS-capable HTTP client"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("ca-certificates"),
            "error should hint at installing ca-certificates: {rendered}"
        );
    }

    /// With no record and no override, the client is absent and require exits 3.
    #[tokio::test]
    async fn absent_record_is_daemon_unavailable() {
        let _env = EnvGuard::unset_daemon_addr();
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        let client = DaemonClient::with_record_path(Some(record));
        assert!(client.status_optional().await.unwrap().is_none());
        let error = client.require_compatible().await.unwrap_err();
        assert_eq!(crate::error::exit_code(&error), ExitCode::DaemonUnavailable);
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = [0; 2048];
        let read = stream.read(&mut request).await.unwrap();
        String::from_utf8_lossy(&request[..read]).to_string()
    }

    fn api_error_response(status: &str, error: &ApiError) -> String {
        let body = serde_json::to_string(error).unwrap();
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn status_body(version: &str, api_major: u16, api_minor: u16) -> String {
        format!(
            r#"{{
                "version":"{version}",
                "api_major":{api_major},
                "api_minor":{api_minor},
                "instance_id":"testinstance0000",
                "mount_point":"/tmp/omnifs",
                "config_dir":"/tmp/omnifs-home",
                "cache_dir":"/tmp/omnifs-home/cache",
                "providers_dir":"/tmp/omnifs-home/providers",
                "frontend":null,
                "mounts":[]
            }}"#
        )
    }
}
