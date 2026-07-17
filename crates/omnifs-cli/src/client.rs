//! Typed client for the daemon's local control socket.
//!
//! The client resolves a workspace-local Unix socket for every operation,
//! sends one versioned JSON line, and reads one versioned reply line. Inspector
//! uses the same socket with a persistent subscription connection.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use omnifs_api::{
    CONTROL_MAX_LINE_BYTES, CONTROL_PROTOCOL_VERSION, CONTROL_REQUEST_TIMEOUT_SECS, ControlError,
    ControlErrorCode, ControlOperation, ControlOutcome, ControlReply, ControlRequest, DaemonStatus,
    TcpAttachTarget, VsockAttachTarget,
};
use omnifs_workspace::Workspace;
use omnifs_workspace::daemon_record::{DaemonRecord, Endpoint};
use omnifs_workspace::mounts::Revision;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

use crate::error::{ExitCode, WithExitCode};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS);
const OFFLINE_VALIDATION_TIMEOUT: Duration = Duration::from_mins(5);

#[derive(Debug)]
enum Target {
    Absent,
    Unix {
        socket: PathBuf,
        instance: Option<String>,
    },
}

impl Target {
    fn label(&self) -> String {
        match self {
            Self::Absent => "the daemon".to_string(),
            Self::Unix { socket, .. } => format!("unix:{}", socket.display()),
        }
    }

    fn instance(&self) -> Option<&str> {
        match self {
            Self::Unix { instance, .. } => instance.as_deref(),
            Self::Absent => None,
        }
    }
}

pub(crate) struct DaemonClient {
    record_path: Option<PathBuf>,
    control_socket: Option<PathBuf>,
    config_dir: PathBuf,
    cache_dir: PathBuf,
    log_file: PathBuf,
    cleaned_stale: AtomicBool,
}

impl DaemonClient {
    pub(crate) fn for_workspace(workspace: &Workspace) -> Self {
        let files = workspace.daemon();
        Self {
            record_path: Some(files.record_file().to_path_buf()),
            control_socket: Some(files.control_socket().to_path_buf()),
            config_dir: files.config_dir().to_path_buf(),
            cache_dir: files.cache_dir().to_path_buf(),
            log_file: files.log_file(),
            cleaned_stale: AtomicBool::new(false),
        }
    }

    pub(crate) fn record(&self) -> anyhow::Result<Option<DaemonRecord>> {
        Ok(match &self.record_path {
            Some(path) => DaemonRecord::read(path)?,
            None => None,
        })
    }

    pub(crate) fn remove_record(&self) -> anyhow::Result<()> {
        if let Some(path) = &self.record_path {
            DaemonRecord::remove(path)?;
        }
        Ok(())
    }

    pub(crate) fn log_file(&self) -> PathBuf {
        self.log_file.clone()
    }

    pub(crate) fn matches_status(&self, status: &DaemonStatus) -> bool {
        same_path(&status.config_dir, &self.config_dir)
            && same_path(&status.cache_dir, &self.cache_dir)
    }

    pub(crate) fn config_display(&self) -> String {
        self.config_dir.display().to_string()
    }

    pub(crate) fn cache_display(&self) -> String {
        self.cache_dir.display().to_string()
    }

    #[cfg(test)]
    fn with_record_path(record_path: Option<PathBuf>) -> Self {
        Self {
            record_path,
            control_socket: None,
            config_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            log_file: PathBuf::new(),
            cleaned_stale: AtomicBool::new(false),
        }
    }

    fn resolve(&self) -> Result<Target> {
        let record = match &self.record_path {
            Some(record_path) => match DaemonRecord::read(record_path) {
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
                        .with_context(|| format!("read daemon record {}", record_path.display()));
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
        Ok(self.control_socket_target().unwrap_or(Target::Absent))
    }

    fn control_socket_target(&self) -> Option<Target> {
        let socket = self.control_socket.as_ref()?;
        socket.exists().then(|| Target::Unix {
            socket: socket.clone(),
            instance: None,
        })
    }

    pub(crate) fn event_endpoint(&self) -> Result<Option<EventEndpoint>> {
        Ok(match self.resolve()? {
            Target::Absent => None,
            Target::Unix { socket, .. } => Some(EventEndpoint::Unix { socket }),
        })
    }

    async fn request(&self, operation: ControlOperation) -> Result<Option<ControlReply>> {
        self.request_with_timeout(operation, REQUEST_TIMEOUT).await
    }

    async fn request_with_timeout(
        &self,
        operation: ControlOperation,
        timeout: Duration,
    ) -> Result<Option<ControlReply>> {
        let target = self.resolve()?;
        let Target::Unix { socket, .. } = &target else {
            return Ok(None);
        };
        let request = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            operation,
        };
        let result = tokio::time::timeout(timeout, async {
            let mut stream = UnixStream::connect(socket).await?;
            let mut line = serde_json::to_vec(&request).context("serialize control request")?;
            line.push(b'\n');
            stream.write_all(&line).await?;
            let reply = read_control_line(&mut stream).await?;
            serde_json::from_slice(&reply).context("parse control reply")
        })
        .await;
        match result {
            Err(_) => Ok(None),
            Ok(Ok(reply)) => Ok(Some(reply)),
            Ok(Err(error)) if is_connection_error(&error) => {
                self.clean_stale_record();
                Ok(None)
            },
            Ok(Err(error)) => Err(error)
                .with_context(|| format!("request over control socket {}", socket.display())),
        }
    }

    fn clean_stale_record(&self) {
        if let Some(path) = &self.record_path {
            let _ = DaemonRecord::remove(path);
        }
        self.cleaned_stale.store(true, Ordering::Relaxed);
    }

    fn unavailable_error(&self) -> anyhow::Error {
        let message = if self.cleaned_stale.load(Ordering::Relaxed) {
            "daemon not running (cleaned up a stale record)"
        } else {
            "daemon not running"
        };
        match Err::<(), _>(anyhow::anyhow!(message)).with_exit_code(ExitCode::DaemonUnavailable) {
            Ok(()) => unreachable!("daemon unavailable construction always starts from Err"),
            Err(error) => error,
        }
    }

    async fn probe(&self) -> DaemonControlState {
        let target = match self.resolve() {
            Ok(target) => target,
            Err(error) => return DaemonControlState::Sick { error },
        };
        let reply = match self.request(ControlOperation::Status).await {
            Ok(Some(reply)) => reply,
            Ok(None) => return DaemonControlState::Absent,
            Err(error) => return DaemonControlState::Sick { error },
        };
        match self.status_from_reply(&reply, &target) {
            Ok(status) => DaemonControlState::Responding(Box::new(status)),
            Err(error) => DaemonControlState::Sick { error },
        }
    }

    pub(crate) async fn status_optional(&self) -> Result<Option<DaemonStatus>> {
        let target = self.resolve()?;
        let Some(reply) = self.request(ControlOperation::Status).await? else {
            return Ok(None);
        };
        self.status_from_reply(&reply, &target).map(Some)
    }

    pub(crate) async fn require_status(&self) -> Result<DaemonStatus> {
        let label = self
            .resolve()
            .map(|target| target.label())
            .unwrap_or_default();
        match self.probe().await.into_optional(&label)? {
            Some(status) => Ok(status),
            None => Err(self.unavailable_error()),
        }
    }

    pub(crate) async fn status_optional_checked(&self) -> Result<Option<DaemonStatus>> {
        let label = self
            .resolve()
            .map(|target| target.label())
            .unwrap_or_default();
        self.probe().await.into_optional(&label)
    }

    pub(crate) async fn status(&self) -> Result<DaemonStatus> {
        match self.status_optional().await? {
            Some(status) => Ok(status),
            None => Err(self.unavailable_error()),
        }
    }

    fn status_from_reply(&self, reply: &ControlReply, target: &Target) -> Result<DaemonStatus> {
        Self::check_version(reply)?;
        if let ControlOutcome::Error(error) = &reply.outcome {
            return Err(control_error("daemon status request failed", error));
        }
        let ControlOutcome::Status(status) = &reply.outcome else {
            return Err(unexpected_reply("status"));
        };
        self.verify_instance(status, target)?;
        Ok(status.clone())
    }

    fn verify_instance(&self, status: &DaemonStatus, target: &Target) -> Result<()> {
        let Some(expected) = target.instance() else {
            return Ok(());
        };
        if status.instance_id == expected {
            return Ok(());
        }
        if let Some(path) = &self.record_path
            && let Ok(Some(record)) = DaemonRecord::read(path)
            && record.instance_id == status.instance_id
        {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "the daemon daemon record was overwritten mid-command (daemon instance {}, record instance {expected}); rerun the command",
            status.instance_id,
        ))
    }

    fn check_version(reply: &ControlReply) -> Result<()> {
        if reply.version != CONTROL_PROTOCOL_VERSION {
            return Err(anyhow::anyhow!(
                "daemon speaks control protocol v{}, this CLI speaks v{}; stop it with `omnifs down`, then rerun",
                reply.version,
                CONTROL_PROTOCOL_VERSION,
            ));
        }
        Ok(())
    }

    fn reply_result<'a>(reply: &'a ControlReply, operation: &str) -> Result<&'a ControlOutcome> {
        Self::check_version(reply)?;
        if let ControlOutcome::Error(error) = &reply.outcome {
            return Err(control_error(operation, error));
        }
        Ok(&reply.outcome)
    }

    pub(crate) async fn frontend_attach_target(
        &self,
        bind_ip: Option<std::net::Ipv4Addr>,
    ) -> Result<TcpAttachTarget> {
        let Some(reply) = self
            .request(ControlOperation::AttachTcp { bind_ip })
            .await?
        else {
            return Err(self.unavailable_error());
        };
        match Self::reply_result(&reply, "daemon attach-target request failed")? {
            ControlOutcome::AttachTcp(target) => Ok(target.clone()),
            _ => Err(unexpected_reply("attach_tcp")),
        }
    }

    pub(crate) async fn frontend_attach_target_vsock(&self) -> Result<VsockAttachTarget> {
        let Some(reply) = self.request(ControlOperation::AttachVsock).await? else {
            return Err(self.unavailable_error());
        };
        match Self::reply_result(&reply, "daemon attach-target vsock request failed")? {
            ControlOutcome::AttachVsock(target) => Ok(target.clone()),
            _ => Err(unexpected_reply("attach_vsock")),
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<Option<()>> {
        let Some(reply) = self.request(ControlOperation::Shutdown).await? else {
            return Ok(None);
        };
        match Self::reply_result(&reply, "daemon shutdown request failed")? {
            ControlOutcome::Shutdown => Ok(Some(())),
            _ => Err(unexpected_reply("shutdown")),
        }
    }

    pub(crate) async fn validate_offline(&self, revision: &Revision) -> Result<()> {
        let Some(reply) = self
            .request_with_timeout(
                ControlOperation::ValidateOffline {
                    revision: revision.to_string(),
                },
                OFFLINE_VALIDATION_TIMEOUT,
            )
            .await?
        else {
            return Err(self.unavailable_error());
        };
        match Self::reply_result(&reply, "offline projection validation failed")? {
            ControlOutcome::OfflineValidated => Ok(()),
            _ => Err(unexpected_reply("validate_offline")),
        }
    }

    pub(crate) async fn ready(&self) -> bool {
        let Ok(Some(reply)) = self.request(ControlOperation::Ready).await else {
            return false;
        };
        matches!(
            Self::reply_result(&reply, "daemon readiness request failed"),
            Ok(ControlOutcome::Ready)
        )
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

#[derive(Clone)]
pub(crate) enum EventEndpoint {
    Unix { socket: PathBuf },
}

impl EventEndpoint {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Unix { socket } => format!("unix:{}", socket.display()),
        }
    }
}

#[derive(Debug)]
pub(crate) enum DaemonControlState {
    Absent,
    Responding(Box<DaemonStatus>),
    Sick { error: anyhow::Error },
}

impl DaemonControlState {
    fn into_optional(self, label: &str) -> Result<Option<DaemonStatus>> {
        match self {
            Self::Absent => Ok(None),
            Self::Responding(status) => Ok(Some(*status)),
            Self::Sick { error } => Err(error.context(if label.is_empty() {
                "a daemon answered, but its status could not be read".to_string()
            } else {
                format!("a daemon answered at {label}, but its status could not be read")
            })),
        }
    }
}

fn control_error(context: &str, error: &ControlError) -> anyhow::Error {
    let exit_code = match error.code {
        ControlErrorCode::NotReady => ExitCode::DaemonUnavailable,
        ControlErrorCode::UnsupportedVersion
        | ControlErrorCode::MalformedJson
        | ControlErrorCode::UnknownOperation
        | ControlErrorCode::LineTooLarge
        | ControlErrorCode::InvalidRequest
        | ControlErrorCode::OfflineValidationFailed
        | ControlErrorCode::Internal => ExitCode::GenericFailure,
    };
    match Err::<(), _>(anyhow::anyhow!("{context}: {}", error.message)).with_exit_code(exit_code) {
        Ok(()) => unreachable!("control error construction starts from Err"),
        Err(error) => error,
    }
}

fn unexpected_reply(operation: &str) -> anyhow::Error {
    anyhow::anyhow!("daemon returned an unexpected reply for {operation}")
}

fn is_connection_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        )
    })
}

pub(crate) async fn read_control_line<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Vec<u8>> {
    let mut line = Vec::with_capacity(256);
    loop {
        let mut byte = [0_u8; 1];
        let read = reader.read(&mut byte).await?;
        if read == 0 {
            if line.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "control connection closed before a line was received",
                ));
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control line is missing its newline terminator",
            ));
        }
        line.push(byte[0]);
        if line.len() > CONTROL_MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control line exceeds the maximum size",
            ));
        }
        if byte[0] == b'\n' {
            return Ok(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_record_is_daemon_unavailable() {
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        let client = DaemonClient::with_record_path(Some(record));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime.block_on(client.require_status()).unwrap_err();
        assert_eq!(crate::error::exit_code(&error), ExitCode::DaemonUnavailable);
    }

    fn client_without_record(record: PathBuf, control_socket: PathBuf) -> DaemonClient {
        DaemonClient {
            record_path: Some(record),
            control_socket: Some(control_socket),
            config_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            log_file: PathBuf::new(),
            cleaned_stale: AtomicBool::new(false),
        }
    }

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
                assert!(instance.is_none());
            },
            Target::Absent => panic!("expected control-socket fallback target"),
        }
    }

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
    fn corrupt_daemon_record_falls_back_to_fixed_control_socket() {
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        let socket = home.path().join("control.sock");
        std::fs::write(&record, b"not json").unwrap();
        std::fs::write(&socket, b"reserved").unwrap();
        let client = client_without_record(record, socket.clone());
        assert!(matches!(
            client.resolve().unwrap(),
            Target::Unix { socket: dialed, instance: None } if dialed == socket
        ));
    }

    #[test]
    fn corrupt_daemon_record_without_socket_is_cleaned_and_absent() {
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        std::fs::write(&record, b"not json").unwrap();
        let client = client_without_record(record.clone(), home.path().join("control.sock"));
        assert!(matches!(client.resolve().unwrap(), Target::Absent));
        assert!(!record.exists());
    }

    #[test]
    fn unreadable_daemon_record_is_not_treated_as_stale() {
        let home = tempfile::tempdir().unwrap();
        let record = home.path().join("daemon.json");
        std::fs::create_dir(&record).unwrap();
        let socket = home.path().join("control.sock");
        std::fs::write(&socket, b"reserved").unwrap();
        let client = client_without_record(record.clone(), socket);
        let error = client.resolve().unwrap_err();
        assert!(format!("{error:#}").contains("read daemon record"));
        assert!(record.exists());
    }
}
