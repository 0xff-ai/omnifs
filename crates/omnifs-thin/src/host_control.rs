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
    Ping { instance_id: String },
    Stop { instance_id: String },
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
    Pong,
    Stop(StopOutcome),
    Error { message: String },
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
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
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
        stop: mpsc::Sender<StopRequest>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = self.listener.take().expect("host control starts once");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let instance_id = instance_id.clone();
                let phase = phase.clone();
                let stop = stop.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        RUNNER_CONTROL_TIMEOUT,
                        handle_connection(stream, instance_id, phase, stop),
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
    stop: mpsc::Sender<StopRequest>,
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
        RunnerRequest::Ping { instance_id } | RunnerRequest::Stop { instance_id } => instance_id,
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
            write_reply(&mut writer, &instance_id, current_phase, RunnerReply::Pong).await
        },
        RunnerRequest::Stop { .. } => {
            let (outcome, outcome_rx) = oneshot::channel();
            let (flushed, flushed_rx) = oneshot::channel();
            stop.send(StopRequest {
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
            RunnerReply::Pong => Ok(RunnerState {
                instance_id: reply.instance_id,
                phase: reply.phase,
            }),
            RunnerReply::Error { message } => Err(anyhow::anyhow!(message)),
            RunnerReply::Stop(_) => anyhow::bail!("unexpected stop reply to ping"),
        }
    }

    pub async fn stop(&self) -> anyhow::Result<(RunnerState, StopOutcome)> {
        let reply = self
            .request(RunnerRequest::Stop {
                instance_id: self.instance_id.clone(),
            })
            .await?;
        match reply.reply {
            RunnerReply::Stop(outcome) => Ok((
                RunnerState {
                    instance_id: reply.instance_id,
                    phase: reply.phase,
                },
                outcome,
            )),
            RunnerReply::Error { message } => Err(anyhow::anyhow!(message)),
            RunnerReply::Pong => anyhow::bail!("unexpected ping reply to stop"),
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

pub fn control_socket_for(state_dir: &Path, _instance_id: &str) -> PathBuf {
    // One runner claim owns this state directory at a time, while the control
    // protocol still fences every request with the exact runtime instance.
    // Keeping the filename fixed avoids exceeding the short Unix-domain socket
    // path limit for otherwise valid profile roots.
    state_dir.join("control.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::{FilesystemProtocol, FilesystemRuntime, FilesystemSpec, ResourceName};

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
                    handle_connection(stream, instance.clone(), phase_rx.clone(), stop_tx.clone())
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
            filesystem: ResourceName::new("main").unwrap(),
            spec: FilesystemSpec::new(
                FilesystemProtocol::Nfs,
                FilesystemRuntime::Host,
                PathBuf::from("/mnt/omnifs"),
                None,
                None,
            )
            .unwrap(),
            control_socket: socket,
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
        let (state, outcome) = client.stop().await.unwrap();
        assert_eq!(state.instance_id, instance);
        assert_eq!(outcome, StopOutcome::Stopped);
        completion.await.unwrap();
        server.await.unwrap();
        drop(phase_tx);
    }
}
