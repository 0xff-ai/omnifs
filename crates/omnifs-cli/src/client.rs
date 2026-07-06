//! HTTP client for the daemon control API.
//!
//! The daemon listens on the container's published loopback port (or its
//! own loopback when running natively). `OMNIFS_DAEMON_ADDR` overrides the
//! `host:port` for non-default setups.

use anyhow::{Context as _, Result};
use omnifs_api::{
    API_MAJOR, API_MINOR, ApiError, CredentialStatus, DaemonStatus, ErrorCode, MountReport,
    MountUpdateRequest, ProviderSummary, ReconcileReport, StopReport, UpgradeDelta,
};
use omnifs_workspace::authn::CredentialId;
use omnifs_workspace::layout::{CONTROL_TOKEN_FILE, WorkspaceLayout};
use omnifs_workspace::mounts::{Spec, UpgradePlan};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::control::addr::daemon_addr;
use crate::error::{ExitCode, WithExitCode, WithHint};

const EXPORT_API_MINOR: u16 = 2;

pub(crate) struct DaemonClient {
    base: String,
    http: reqwest::Client,
    token_file: PathBuf,
}

#[derive(Debug)]
pub(crate) enum DaemonControlState {
    Absent,
    /// A daemon answered the control port but its status could not be read.
    /// Carries the original error (chain and hints intact) so the final
    /// rendering shows the cause once, not a pre-flattened `{:#}` string.
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

    fn compatible_optional(self, base: &str) -> Result<Option<DaemonStatus>> {
        match self {
            Self::Compatible(status) => Ok(Some(*status)),
            Self::Absent => Ok(None),
            Self::Sick { error } => Err(error.context(format!(
                "a daemon answered on the control port at {base}, but its status could not be read \
                 (it may belong to a different OMNIFS_HOME)"
            ))),
            Self::Incompatible(status) => Err(incompatible_daemon_error(&status)),
        }
    }

    fn require_compatible(self, base: &str) -> Result<DaemonStatus> {
        match self.compatible_optional(base)? {
            Some(status) => Ok(status),
            None => Err(daemon_unavailable_error(base)),
        }
    }
}

impl DaemonClient {
    pub(crate) fn new() -> Self {
        let token_file = WorkspaceLayout::resolve().map_or_else(
            |_| PathBuf::from(CONTROL_TOKEN_FILE),
            |layout| layout.control_token_file(),
        );
        Self::from_token_file(token_file)
    }

    pub(crate) fn for_layout(layout: &WorkspaceLayout) -> Self {
        Self::from_token_file(layout.control_token_file())
    }

    fn from_token_file(token_file: PathBuf) -> Self {
        Self {
            base: format!("http://{}", daemon_addr()),
            http: Self::http_client(),
            token_file,
        }
    }

    fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client with static config")
    }

    /// Probe for a daemon and classify its control state in one step.
    ///
    /// A compatible daemon has the same control API major. Minor skew is
    /// reported as a note because it is additive. Incompatible and sick daemons
    /// are returned as typed states so callers do not re-create probe policy.
    async fn probe(&self) -> DaemonControlState {
        let response = match self.get_optional("/v1/status", "query daemon status").await {
            Ok(Some(response)) => response,
            Ok(None) => return DaemonControlState::Absent,
            Err(error) => {
                return DaemonControlState::Sick { error };
            },
        };
        let state = match Self::parse_status(response).await {
            Ok(status) => DaemonControlState::from_status(status),
            Err(error) => DaemonControlState::Sick { error },
        };
        state.warn_minor_skew();
        state
    }

    /// Raw daemon status probe. Connection absence is `None`; a reachable
    /// daemon's HTTP status and JSON errors are propagated.
    pub(crate) async fn status_optional(&self) -> Result<Option<DaemonStatus>> {
        let Some(response) = self
            .get_optional("/v1/status", "query daemon status")
            .await?
        else {
            return Ok(None);
        };
        Self::parse_status(response).await.map(Some)
    }

    /// Verify the daemon is reachable and speaks this CLI's control API.
    pub(crate) async fn require_compatible(&self) -> Result<DaemonStatus> {
        self.probe().await.require_compatible(&self.base)
    }

    /// Daemon status when a compatible daemon answers; `None` when no daemon
    /// answered on the control port.
    pub(crate) async fn compatible_status_optional(&self) -> Result<Option<DaemonStatus>> {
        self.probe().await.compatible_optional(&self.base)
    }

    /// Daemon runtime facts from a reachable, compatible daemon.
    pub(crate) async fn status(&self) -> Result<DaemonStatus> {
        self.status_optional()
            .await?
            .ok_or_else(|| daemon_unavailable_error(&self.base))
    }

    async fn get_optional(
        &self,
        path: &str,
        context: &'static str,
    ) -> Result<Option<reqwest::Response>> {
        let token = match read_control_token_file(&self.token_file) {
            Ok(token) => token,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.ready().await {
                    return Err(self.control_token_unavailable_error(&error));
                }
                return Ok(None);
            },
            Err(error) => return Err(self.control_token_unavailable_error(&error)),
        };
        match self
            .http
            .get(format!("{}{}", self.base, path))
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(response) => Ok(Some(response)),
            Err(error) if error.is_connect() || error.is_timeout() => Ok(None),
            Err(error) => Err(error).with_context(|| format!("{context} at {}", self.base)),
        }
    }

    fn authenticated(&self, request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let token = read_control_token_file(&self.token_file)
            .map_err(|error| self.control_token_unavailable_error(&error))?;
        Ok(request.bearer_auth(token))
    }

    fn control_token_unavailable_error(&self, error: &std::io::Error) -> anyhow::Error {
        control_token_unavailable_error(&self.base, &self.token_file, error)
    }

    async fn parse_status(response: reqwest::Response) -> Result<DaemonStatus> {
        let response = Self::ensure_success(response, "daemon status request failed").await?;
        response.json().await.context("parse daemon status")
    }

    async fn ensure_success(
        response: reqwest::Response,
        context: &'static str,
    ) -> Result<reqwest::Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        Err(Self::response_error(response, context).await)
    }

    async fn response_error(response: reqwest::Response, context: &'static str) -> anyhow::Error {
        let status = response.status();
        match response.json::<ApiError>().await {
            Ok(api_error) => Self::api_error(context, &api_error),
            Err(error) => {
                anyhow::anyhow!(
                    "{context}: daemon returned {status} with invalid ApiError JSON: {error}"
                )
            },
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

    /// Converge the running daemon's mount set to the on-disk desired state
    /// under `mounts/*.json`. Reconcile compiles WASM for added or changed
    /// mounts, so it gets the long mount-load timeout rather than the default.
    pub(crate) async fn reconcile(&self) -> Result<ReconcileReport> {
        let response = self
            .authenticated(
                self.http
                    .post(format!("{}/v1/reconcile", self.base))
                    .timeout(Duration::from_mins(3)),
            )?
            .send()
            .await
            .with_context(|| format!("reconcile mounts on daemon at {}", self.base))?;
        let response = Self::ensure_success(response, "daemon reconcile request failed").await?;
        response.json().await.context("parse reconcile report")
    }

    pub(crate) async fn create_mount_if_ready(&self, spec: &Spec) -> Result<Option<MountReport>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let response = self
            .authenticated(
                self.http
                    .post(format!("{}/v1/mounts", self.base))
                    .json(&serde_json::to_value(spec).context("serialize mount spec")?)
                    .timeout(Duration::from_mins(3)),
            )?
            .send()
            .await
            .with_context(|| format!("create mount `{}` on daemon at {}", spec.mount, self.base))?;
        let response = Self::ensure_success(response, "daemon mount create request failed").await?;
        response
            .json()
            .await
            .context("parse mount create report")
            .map(Some)
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
        let response = self
            .authenticated(
                self.http
                    .put(format!("{}/v1/mounts/{}", self.base, spec.mount))
                    .json(&request)
                    .timeout(Duration::from_mins(3)),
            )?
            .send()
            .await
            .with_context(|| format!("update mount `{}` on daemon at {}", spec.mount, self.base))?;
        let response = Self::ensure_success(response, "daemon mount update request failed").await?;
        response
            .json()
            .await
            .context("parse mount update report")
            .map(Some)
    }

    pub(crate) async fn delete_mount_if_ready(&self, mount: &str) -> Result<Option<MountReport>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let response = self
            .authenticated(
                self.http
                    .delete(format!("{}/v1/mounts/{mount}", self.base))
                    .timeout(Duration::from_mins(3)),
            )?
            .send()
            .await
            .with_context(|| format!("delete mount `{mount}` on daemon at {}", self.base))?;
        let response = Self::ensure_success(response, "daemon mount delete request failed").await?;
        response
            .json()
            .await
            .context("parse mount delete report")
            .map(Some)
    }

    pub(crate) async fn reload_credential_if_ready(
        &self,
        id: &CredentialId,
    ) -> Result<Option<CredentialStatus>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let mut url = reqwest::Url::parse(&self.base).context("parse daemon base URL")?;
        let id = id.to_string();
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("daemon base URL cannot be used as a path base"))?
            .extend(["v1", "credentials", id.as_str(), "reload"]);
        let response = self
            .authenticated(self.http.post(url))?
            .send()
            .await
            .with_context(|| format!("reload credential `{id}` on daemon at {}", self.base))?;
        let response =
            Self::ensure_success(response, "daemon credential reload request failed").await?;
        response
            .json()
            .await
            .context("parse credential reload status")
            .map(Some)
    }

    pub(crate) async fn providers_if_ready(&self) -> Result<Option<Vec<ProviderSummary>>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let response = self
            .authenticated(self.http.get(format!("{}/v1/providers", self.base)))?
            .send()
            .await
            .with_context(|| format!("list providers on daemon at {}", self.base))?;
        let response = Self::ensure_success(response, "daemon providers request failed").await?;
        response
            .json()
            .await
            .context("parse daemon providers")
            .map(Some)
    }

    pub(crate) async fn credentials_if_ready(&self) -> Result<Option<Vec<CredentialStatus>>> {
        if !self.ready().await {
            return Ok(None);
        }
        self.require_compatible().await?;
        let response = self
            .authenticated(self.http.get(format!("{}/v1/credentials", self.base)))?
            .send()
            .await
            .with_context(|| format!("list credentials on daemon at {}", self.base))?;
        let response = Self::ensure_success(response, "daemon credentials request failed").await?;
        response
            .json()
            .await
            .context("parse daemon credentials")
            .map(Some)
    }

    /// Export a mount snapshot tar only when a compatible daemon is running.
    pub(crate) async fn export_mount_if_running(&self, mount: &str) -> Result<Option<Vec<u8>>> {
        let Some(status) = self.compatible_status_optional().await? else {
            return Ok(None);
        };
        if status.api_minor < EXPORT_API_MINOR {
            return Ok(None);
        }
        let response = self
            .authenticated(
                self.http
                    .get(format!("{}/v1/mounts/{mount}/export", self.base))
                    .timeout(Duration::from_mins(1)),
            )?
            .send()
            .await
            .with_context(|| format!("export mount `{mount}` from daemon at {}", self.base))?
            .error_for_status()
            .context("daemon snapshot export request failed")?;
        let bytes = response
            .bytes()
            .await
            .context("read snapshot tar response")?;
        Ok(Some(bytes.to_vec()))
    }

    /// Ask the daemon to unmount its frontend and exit, returning what it tore
    /// down. `None` when no daemon answered, so the caller can fall back to a
    /// stale-mount sweep.
    pub(crate) async fn shutdown(&self) -> Result<Option<StopReport>> {
        let request = match self.authenticated(self.http.post(format!("{}/v1/shutdown", self.base)))
        {
            Ok(request) => request,
            Err(error) if self.ready().await => return Err(error),
            Err(_) => return Ok(None),
        };

        match request.send().await {
            Ok(response) => {
                let report = Self::ensure_success(response, "daemon shutdown request failed")
                    .await?
                    .json()
                    .await
                    .context("parse stop report")?;
                Ok(Some(report))
            },
            Err(error) if error.is_connect() || error.is_timeout() => Ok(None),
            Err(error) => Err(error).with_context(|| format!("shutdown daemon at {}", self.base)),
        }
    }

    /// True once the daemon reports the filesystem is serving.
    pub(crate) async fn ready(&self) -> bool {
        match self
            .http
            .get(format!("{}/v1/ready", self.base))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => true,
            Ok(response) => {
                let _ = Self::response_error(response, "daemon ready request failed").await;
                false
            },
            Err(_) => false,
        }
    }
}

pub(crate) fn read_control_token_file(path: &Path) -> std::io::Result<String> {
    let token = std::fs::read_to_string(path)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control token file is empty",
        ));
    }
    Ok(token.to_string())
}

/// A daemon is answering the control address but will not accept this
/// workspace's credentials. This is almost always a daemon owned by a different
/// `OMNIFS_HOME` (the `just dev` sandbox at `~/.omnifs-dev`, or another
/// worktree) holding the shared control port, so pointing the user at `omnifs
/// up` would mislead: `up` cannot bind a port a foreign daemon already holds
/// (it fails with "another omnifs daemon is already serving").
pub(crate) fn foreign_daemon_error(base: &str) -> anyhow::Error {
    match Err::<(), _>(anyhow::anyhow!(
        "a daemon is serving on the control address at {base}, but it does not accept this workspace's credentials"
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

fn control_token_unavailable_error(
    base: &str,
    path: &Path,
    error: &std::io::Error,
) -> anyhow::Error {
    // A missing or unreadable token here means we cannot authenticate to
    // whatever daemon is answering `base`; fold the token-file detail into the
    // foreign-daemon diagnosis rather than promising a restart that cannot help.
    match Err::<(), _>(foreign_daemon_error(base)).with_hint(format!(
        "this workspace's control token at {} is missing or unreadable ({error})",
        path.display()
    )) {
        Ok(()) => unreachable!("control token error construction always starts from Err"),
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

fn daemon_unavailable_error(base: &str) -> anyhow::Error {
    match Err::<(), _>(anyhow::anyhow!(
        "no daemon answered on the control port at {base}"
    ))
    .with_exit_code(ExitCode::DaemonUnavailable)
    {
        Ok(()) => unreachable!("daemon unavailable construction always starts from Err"),
        Err(error) => error,
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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

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

        let (client, _home) = test_client(addr, "test-token");
        let status = client.status_optional().await.unwrap().unwrap();
        assert_eq!(status.version, "test-daemon");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missing_control_token_with_ready_daemon_is_auth_required() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(
                request.starts_with("GET /v1/ready "),
                "missing-token classification should probe ready, got:\n{request}"
            );
            let response = json_response(r#"{"ready":true}"#);
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let home = tempfile::tempdir().unwrap();
        let missing_token = home.path().join("control-token");
        let client = DaemonClient {
            base: format!("http://{addr}"),
            http: DaemonClient::http_client(),
            token_file: missing_token.clone(),
        };

        let error = client.status_optional().await.unwrap_err();
        assert_eq!(crate::error::exit_code(&error), ExitCode::AuthRequired);
        let rendered = crate::error::render(&error);
        assert!(rendered.contains(&missing_token.display().to_string()));
        // A ready daemon that rejects our token is a foreign daemon: point at
        // `omnifs down`, never `omnifs up` (which cannot bind the held port).
        assert!(rendered.contains("omnifs down"));
        assert!(!rendered.contains("omnifs up"));
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

        let (client, _home) = test_client(addr, "test-token");

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

        let (client, _home) = test_client(addr, "test-token");

        let err = client
            .probe()
            .await
            .require_compatible(&client.base)
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("control API"),
            "error should mention control API mismatch: {msg}"
        );
    }

    /// A daemon reporting the same major but a different minor must proceed (with a warning).
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

        let (client, _home) = test_client(addr, "test-token");

        // Minor skew: probe must succeed (return Compatible).
        assert!(matches!(
            client.probe().await,
            DaemonControlState::Compatible(_)
        ));
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

    fn test_client(addr: std::net::SocketAddr, token: &str) -> (DaemonClient, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let token_file = home.path().join("control-token");
        std::fs::write(&token_file, token).unwrap();
        (
            DaemonClient {
                base: format!("http://{addr}"),
                http: DaemonClient::http_client(),
                token_file,
            },
            home,
        )
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
