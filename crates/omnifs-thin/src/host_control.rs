use omnifs_mtab::{RunnerClaim, RunnerRecord, RunnerRecordFile};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};

pub const RUNNER_CONTROL_VERSION: u8 = 1;
pub const RUNNER_CONTROL_MAX_LINE_BYTES: usize = 64 * 1024;
pub const RUNNER_CONTROL_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPhase {
    Preflight,
    Attaching,
    Mounting,
    Mounted,
    Stopping,
    Busy,
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum RunnerRequest {
    Ping {
        instance_id: String,
    },
    Stop {
        instance_id: String,
    },
    /// Write the filehandle table, stop the server and end without an
    /// unmount, so that a new instance serves the mount in place.
    Restart {
        instance_id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerRequestEnvelope {
    version: u8,
    request: RunnerRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    tag = "kind",
    content = "value",
    rename_all = "snake_case"
)]
enum RunnerReply {
    /// The answer of a runner of a build before `Restart` has none.
    Pong(Option<PongAnswer>),
    Stop(StopOutcome),
    /// The runner released its mount and ends.
    Restart,
    Error {
        message: String,
    },
}

/// What a runner answers to `Ping` beyond its phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PongAnswer {
    /// Whether the runner answers `Restart`.
    restart: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerReplyEnvelope {
    version: u8,
    instance_id: String,
    phase: RunnerPhase,
    reply: RunnerReply,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopOutcome {
    Stopped,
    Busy { message: String },
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerState {
    pub instance_id: String,
    pub phase: RunnerPhase,
    /// Whether the runner answers `Restart`, which a runner of a build before
    /// it does not.
    pub restart: bool,
}

pub(crate) struct StopRequest {
    outcome: oneshot::Sender<StopOutcome>,
    flushed: oneshot::Receiver<()>,
}

impl StopRequest {
    pub(crate) async fn complete(self, outcome: StopOutcome) {
        let _ = self.outcome.send(outcome);
        let _ = self.flushed.await;
    }
}

/// A `Restart` for the owner of the mount, which completes it once the server
/// has stopped and the filehandle table is written.
pub(crate) struct RestartRequest {
    outcome: oneshot::Sender<Result<(), String>>,
    flushed: oneshot::Receiver<()>,
}

impl RestartRequest {
    pub(crate) async fn complete(self, outcome: Result<(), String>) {
        let _ = self.outcome.send(outcome);
        let _ = self.flushed.await;
    }
}

/// The lifecycle requests that the control of a runner hands to the owner of
/// its mount.
#[derive(Clone)]
pub(crate) struct ControlRequests {
    pub(crate) stop: mpsc::Sender<StopRequest>,
    /// The `Restart` of a runner that answers it; none for one that does not.
    pub(crate) restart: Option<mpsc::Sender<RestartRequest>>,
}

pub(crate) struct HostControl {
    listener: Option<UnixListener>,
    socket: PathBuf,
    _record: RunnerRecordFile,
}

impl HostControl {
    pub(crate) fn bind(state_dir: &Path, record: &RunnerRecord) -> anyhow::Result<Self> {
        let claim = RunnerClaim::acquire(state_dir)?;
        if RunnerRecord::read(state_dir)?.is_some() {
            anyhow::bail!(
                "filesystem state already exists at {}; run `omnifs doctor`",
                state_dir.display()
            );
        }
        let control_socket = record.control_socket.clone();
        if let Some(parent) = control_socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&control_socket)?;
        std::fs::set_permissions(&control_socket, std::fs::Permissions::from_mode(0o600))?;
        let record = claim.publish(record)?;
        Ok(Self {
            listener: Some(listener),
            socket: control_socket,
            _record: record,
        })
    }

    pub(crate) fn spawn(
        &mut self,
        instance_id: String,
        phase: watch::Receiver<RunnerPhase>,
        requests: ControlRequests,
    ) -> tokio::task::JoinHandle<()> {
        let listener = self.listener.take().expect("host control starts once");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let instance_id = instance_id.clone();
                let phase = phase.clone();
                let requests = requests.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        RUNNER_CONTROL_TIMEOUT,
                        handle_connection(stream, instance_id, phase, requests),
                    )
                    .await;
                });
            }
        })
    }
}

impl Drop for HostControl {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

async fn handle_connection(
    stream: UnixStream,
    instance_id: String,
    phase: watch::Receiver<RunnerPhase>,
    requests: ControlRequests,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let request: RunnerRequestEnvelope = read_line(reader).await?;
    if request.version != RUNNER_CONTROL_VERSION {
        let current_phase = phase.borrow().clone();
        return write_reply(
            &mut writer,
            &instance_id,
            current_phase,
            RunnerReply::Error {
                message: format!("unsupported runner control version {}", request.version),
            },
        )
        .await;
    }
    let requested_id = match &request.request {
        RunnerRequest::Ping { instance_id }
        | RunnerRequest::Stop { instance_id }
        | RunnerRequest::Restart { instance_id } => instance_id,
    };
    if requested_id != &instance_id {
        let current_phase = phase.borrow().clone();
        return write_reply(
            &mut writer,
            &instance_id,
            current_phase,
            RunnerReply::Error {
                message: "runner instance does not match".to_owned(),
            },
        )
        .await;
    }
    match request.request {
        RunnerRequest::Ping { .. } => {
            let current_phase = phase.borrow().clone();
            let answer = PongAnswer {
                restart: requests.restart.is_some(),
            };
            write_reply(
                &mut writer,
                &instance_id,
                current_phase,
                RunnerReply::Pong(Some(answer)),
            )
            .await
        },
        RunnerRequest::Restart { .. } => {
            answer_restart(&mut writer, &instance_id, &phase, requests.restart).await
        },
        RunnerRequest::Stop { .. } => {
            let (outcome, outcome_rx) = oneshot::channel();
            let (flushed, flushed_rx) = oneshot::channel();
            requests
                .stop
                .send(StopRequest {
                    outcome,
                    flushed: flushed_rx,
                })
                .await
                .map_err(|_| anyhow::anyhow!("filesystem stop owner exited"))?;
            let outcome = outcome_rx
                .await
                .map_err(|_| anyhow::anyhow!("filesystem stop result was lost"))?;
            let current_phase = phase.borrow().clone();
            let result = write_reply(
                &mut writer,
                &instance_id,
                current_phase,
                RunnerReply::Stop(outcome),
            )
            .await;
            let _ = flushed.send(());
            result
        },
    }
}

/// Hands a `Restart` to the owner of the mount, and answers once its server
/// has stopped; a runner without a restart in place answers with an error.
async fn answer_restart(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    instance_id: &str,
    phase: &watch::Receiver<RunnerPhase>,
    restart: Option<mpsc::Sender<RestartRequest>>,
) -> anyhow::Result<()> {
    let Some(restart) = restart else {
        let current_phase = phase.borrow().clone();
        return write_reply(
            writer,
            instance_id,
            current_phase,
            RunnerReply::Error {
                message: "this filesystem does not restart in place".to_owned(),
            },
        )
        .await;
    };
    let (outcome, outcome_rx) = oneshot::channel();
    let (flushed, flushed_rx) = oneshot::channel();
    restart
        .send(RestartRequest {
            outcome,
            flushed: flushed_rx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("filesystem restart owner exited"))?;
    let outcome = outcome_rx
        .await
        .map_err(|_| anyhow::anyhow!("filesystem restart result was lost"))?;
    let current_phase = phase.borrow().clone();
    let reply = match outcome {
        Ok(()) => RunnerReply::Restart,
        Err(message) => RunnerReply::Error { message },
    };
    let result = write_reply(writer, instance_id, current_phase, reply).await;
    let _ = flushed.send(());
    result
}

async fn read_line(
    reader: tokio::net::unix::OwnedReadHalf,
) -> anyhow::Result<RunnerRequestEnvelope> {
    let reader = BufReader::new(reader);
    let mut line = Vec::new();
    let read = reader
        .take(u64::try_from(RUNNER_CONTROL_MAX_LINE_BYTES + 1).unwrap())
        .read_until(b'\n', &mut line)
        .await?;
    anyhow::ensure!(read > 0, "runner control request closed before a line");
    anyhow::ensure!(
        line.len() <= RUNNER_CONTROL_MAX_LINE_BYTES,
        "runner control request exceeds the maximum size"
    );
    anyhow::ensure!(
        line.ends_with(b"\n"),
        "runner control request is incomplete"
    );
    Ok(serde_json::from_slice(&line)?)
}

async fn write_reply(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    instance_id: &str,
    phase: RunnerPhase,
    reply: RunnerReply,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(&RunnerReplyEnvelope {
        version: RUNNER_CONTROL_VERSION,
        instance_id: instance_id.to_owned(),
        phase,
        reply,
    })?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

pub struct RunnerControlClient {
    instance_id: String,
    socket: PathBuf,
}

impl RunnerControlClient {
    #[must_use]
    pub fn new(record: &RunnerRecord) -> Self {
        Self {
            instance_id: record.instance_id.clone(),
            socket: record.control_socket.clone(),
        }
    }

    pub async fn ping(&self) -> anyhow::Result<RunnerState> {
        let reply = self
            .request(RunnerRequest::Ping {
                instance_id: self.instance_id.clone(),
            })
            .await?;
        match reply.reply {
            RunnerReply::Pong(answer) => Ok(RunnerState {
                instance_id: reply.instance_id,
                phase: reply.phase,
                restart: answer.is_some_and(|answer| answer.restart),
            }),
            RunnerReply::Error { message } => Err(anyhow::anyhow!(message)),
            RunnerReply::Stop(_) | RunnerReply::Restart => {
                anyhow::bail!("unexpected lifecycle reply to ping")
            },
        }
    }

    /// Asks the runner to write its filehandle table, stop its server and end
    /// without an unmount; it answers once its server has stopped.
    pub async fn restart(&self) -> anyhow::Result<()> {
        let reply = self
            .request(RunnerRequest::Restart {
                instance_id: self.instance_id.clone(),
            })
            .await?;
        match reply.reply {
            RunnerReply::Restart => Ok(()),
            RunnerReply::Error { message } => Err(anyhow::anyhow!(message)),
            RunnerReply::Pong(_) | RunnerReply::Stop(_) => {
                anyhow::bail!("unexpected reply to restart")
            },
        }
    }

    pub async fn stop(&self) -> anyhow::Result<StopOutcome> {
        let reply = self
            .request(RunnerRequest::Stop {
                instance_id: self.instance_id.clone(),
            })
            .await?;
        match reply.reply {
            RunnerReply::Stop(outcome) => Ok(outcome),
            RunnerReply::Error { message } => Err(anyhow::anyhow!(message)),
            RunnerReply::Pong(_) | RunnerReply::Restart => {
                anyhow::bail!("unexpected reply to stop")
            },
        }
    }

    async fn request(&self, request: RunnerRequest) -> anyhow::Result<RunnerReplyEnvelope> {
        tokio::time::timeout(RUNNER_CONTROL_TIMEOUT, async {
            let mut stream = UnixStream::connect(&self.socket).await?;
            let mut line = serde_json::to_vec(&RunnerRequestEnvelope {
                version: RUNNER_CONTROL_VERSION,
                request,
            })?;
            line.push(b'\n');
            stream.write_all(&line).await?;
            stream.flush().await?;
            let (reader, _) = stream.into_split();
            let reply: RunnerReplyEnvelope = read_reply_line(reader).await?;
            anyhow::ensure!(
                reply.version == RUNNER_CONTROL_VERSION,
                "unsupported runner control version {}",
                reply.version
            );
            anyhow::ensure!(
                reply.instance_id == self.instance_id,
                "runner instance does not match"
            );
            Ok(reply)
        })
        .await
        .map_err(|_| anyhow::anyhow!("runner control request timed out"))?
    }
}

async fn read_reply_line(
    reader: tokio::net::unix::OwnedReadHalf,
) -> anyhow::Result<RunnerReplyEnvelope> {
    let reader = BufReader::new(reader);
    let mut line = Vec::new();
    let read = reader
        .take(u64::try_from(RUNNER_CONTROL_MAX_LINE_BYTES + 1).unwrap())
        .read_until(b'\n', &mut line)
        .await?;
    anyhow::ensure!(read > 0, "runner control reply closed before a line");
    anyhow::ensure!(
        line.len() <= RUNNER_CONTROL_MAX_LINE_BYTES,
        "runner control reply exceeds the maximum size"
    );
    anyhow::ensure!(line.ends_with(b"\n"), "runner control reply is incomplete");
    Ok(serde_json::from_slice(&line)?)
}

pub fn control_socket_for(state_dir: &Path, instance_id: &str) -> PathBuf {
    state_dir.join(format!("control-{}.sock", &instance_id[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::fs;

    #[tokio::test]
    async fn ping_and_stop_require_and_echo_the_exact_instance() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("control.sock");
        let instance = "0123456789abcdef0123456789abcdef";
        let listener = UnixListener::bind(&socket).unwrap();
        let (phase_tx, phase_rx) = watch::channel(RunnerPhase::Mounted);
        let (stop_tx, mut stop_rx) = mpsc::channel(1);
        let server = tokio::spawn({
            let instance = instance.to_owned();
            async move {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    handle_connection(
                        stream,
                        instance.clone(),
                        phase_rx.clone(),
                        ControlRequests {
                            stop: stop_tx.clone(),
                            restart: None,
                        },
                    )
                    .await
                    .unwrap();
                }
            }
        });
        let record = RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: instance.to_owned(),
            pid: 42,
            process_group: 42,
            spec: fs::Spec::new(
                fs::Id::new("main").unwrap(),
                fs::Protocol::Nfs,
                fs::Runtime::Host,
                PathBuf::from("/mnt/omnifs"),
            )
            .unwrap(),
            control_socket: socket,
            program: None,
        };
        let client = RunnerControlClient::new(&record);
        assert_eq!(client.ping().await.unwrap().phase, RunnerPhase::Mounted);

        let completion = tokio::spawn(async move {
            stop_rx
                .recv()
                .await
                .unwrap()
                .complete(StopOutcome::Stopped)
                .await;
        });
        assert_eq!(client.stop().await.unwrap(), StopOutcome::Stopped);
        completion.await.unwrap();
        server.await.unwrap();
        drop(phase_tx);
    }

    fn record(socket: PathBuf, instance: &str) -> RunnerRecord {
        RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: instance.to_owned(),
            pid: 42,
            process_group: 42,
            spec: fs::Spec::new(
                fs::Id::new("main").unwrap(),
                fs::Protocol::Nfs,
                fs::Runtime::Host,
                PathBuf::from("/mnt/omnifs"),
            )
            .unwrap(),
            control_socket: socket,
            program: None,
        }
    }

    /// A runner of an NFS mount answers `Ping` with its `Restart`, and answers
    /// `Restart` once the owner of its mount has released it.
    #[tokio::test]
    async fn a_runner_answers_ping_with_its_restart_and_restarts() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("control.sock");
        let instance = "0123456789abcdef0123456789abcdef";
        let listener = UnixListener::bind(&socket).unwrap();
        let (_phase_tx, phase_rx) = watch::channel(RunnerPhase::Mounted);
        let (stop_tx, _stop_rx) = mpsc::channel(1);
        let (restart_tx, mut restart_rx) = mpsc::channel(1);
        let server = tokio::spawn({
            let instance = instance.to_owned();
            async move {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    handle_connection(
                        stream,
                        instance.clone(),
                        phase_rx.clone(),
                        ControlRequests {
                            stop: stop_tx.clone(),
                            restart: Some(restart_tx.clone()),
                        },
                    )
                    .await
                    .unwrap();
                }
            }
        });
        let client = RunnerControlClient::new(&record(socket, instance));
        assert!(client.ping().await.unwrap().restart);

        let completion = tokio::spawn(async move {
            restart_rx.recv().await.unwrap().complete(Ok(())).await;
        });
        client.restart().await.unwrap();
        completion.await.unwrap();
        server.await.unwrap();
    }

    /// The `Pong` of a runner of a build before `Restart` reads without it.
    #[tokio::test]
    async fn the_pong_of_a_runner_before_restart_reads_without_it() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("control.sock");
        let instance = "0123456789abcdef0123456789abcdef";
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn({
            let instance = instance.to_owned();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                read_line(reader).await.unwrap();
                writer
                    .write_all(
                        format!(
                            "{{\"version\":1,\"instance_id\":\"{instance}\",\"phase\":\"mounted\",\"reply\":{{\"kind\":\"pong\"}}}}\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let state = RunnerControlClient::new(&record(socket, instance))
            .ping()
            .await
            .unwrap();
        assert_eq!(state.phase, RunnerPhase::Mounted);
        assert!(!state.restart);
        server.await.unwrap();
    }

    #[test]
    fn control_socket_fits_the_macos_socket_path_limit() {
        let state_dir =
            Path::new("/Users/abcdefghijklmnopq/.omnifs/client/filesystems/state/sources");
        let socket = control_socket_for(state_dir, "0123456789abcdef0123456789abcdef");
        assert_eq!(socket.file_name().unwrap(), "control-01234567.sock");
        assert!(socket.as_os_str().len() < 104, "{}", socket.display());
    }
}
