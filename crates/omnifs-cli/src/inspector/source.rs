//! Event sources: replay file, live typed control-plane subscriber.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Lines, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use omnifs_api::events::InspectorLine;
use omnifs_api::{
    CONTROL_PROTOCOL_VERSION, ControlOperation, ControlOutcome, ControlReply, ControlRequest,
};
use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;

use crate::client::{EventEndpoint, read_control_line};

/// Outcome of one [`EventsClient::attach`] call.
pub enum AttachOutcome {
    /// Could not connect or the daemon refused the stream; retry later.
    Unreachable,
    /// Connected and streamed until the daemon closed the response.
    Ended,
    /// The connected stream produced an invalid or unreadable line.
    Failed(String),
}

/// Blocking line-oriented client for the daemon's inspector subscription.
/// Owns a single-thread tokio runtime so callers can drive the stream from
/// plain threads over the host-native Unix socket.
pub struct EventsClient {
    rt: tokio::runtime::Runtime,
    endpoint: EventEndpoint,
}

impl EventsClient {
    pub fn new(endpoint: EventEndpoint) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build events client runtime")?;
        Ok(Self { rt, endpoint })
    }

    /// Try to connect once. On success, call `on_connect`, then `on_line`
    /// for every typed newline-framed line until the stream ends or fails.
    pub fn attach<E>(
        &self,
        on_connect: impl FnOnce(String),
        mut on_line: impl FnMut(&InspectorLine) -> std::result::Result<(), E>,
    ) -> std::result::Result<AttachOutcome, E> {
        self.rt.block_on(async {
            match &self.endpoint {
                EventEndpoint::Unix { socket } => {
                    let Ok(mut stream) = UnixStream::connect(socket).await else {
                        return Ok(AttachOutcome::Unreachable);
                    };
                    let request = ControlRequest {
                        version: CONTROL_PROTOCOL_VERSION,
                        operation: ControlOperation::SubscribeInspector,
                    };
                    let Ok(mut request_line) = serde_json::to_vec(&request) else {
                        return Ok(AttachOutcome::Unreachable);
                    };
                    request_line.push(b'\n');
                    if stream.write_all(&request_line).await.is_err() {
                        return Ok(AttachOutcome::Unreachable);
                    }
                    let Ok(reply_line) = read_control_line(&mut stream).await else {
                        return Ok(AttachOutcome::Unreachable);
                    };
                    let Ok(reply): std::result::Result<ControlReply, _> =
                        serde_json::from_slice(&reply_line)
                    else {
                        return Ok(AttachOutcome::Unreachable);
                    };
                    if reply.version != CONTROL_PROTOCOL_VERSION {
                        return Ok(AttachOutcome::Unreachable);
                    }
                    let ControlOutcome::InspectorReady { instance_id } = reply.outcome else {
                        return Ok(AttachOutcome::Unreachable);
                    };
                    on_connect(instance_id);
                    loop {
                        let line = match read_control_line(&mut stream).await {
                            Ok(line) => line,
                            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                                return Ok(AttachOutcome::Ended);
                            },
                            Err(error) => {
                                return Ok(AttachOutcome::Failed(format!(
                                    "read inspector stream: {error}"
                                )));
                            },
                        };
                        let line = match std::str::from_utf8(&line) {
                            Ok(line) => match InspectorLine::parse_line(line) {
                                Ok(line) => line,
                                Err(error) => {
                                    return Ok(AttachOutcome::Failed(format!(
                                        "parse inspector stream: {error}"
                                    )));
                                },
                            },
                            Err(error) => {
                                return Ok(AttachOutcome::Failed(format!(
                                    "parse inspector stream: line is not UTF-8: {error}"
                                )));
                            },
                        };
                        on_line(&line)?;
                    }
                },
            }
        })
    }
}

pub enum SourceKind {
    Replay(PathBuf),
    /// Subscribe to the daemon's event stream. Optional `record` also
    /// appends every typed line read to a host-side file.
    Socket {
        endpoint: EventEndpoint,
        record: Option<PathBuf>,
    },
}

/// Source messages retain typed lines and make finite-source termination
/// explicit, so parse and I/O failures cannot become indistinguishable from EOF.
pub enum SourceMessage {
    Line(InspectorLine),
    /// First successful socket connection, or a successful reconnect after a drop.
    Connected {
        epoch: String,
    },
    /// Stream closed after a previously-connected session (daemon
    /// shutdown or transient drop). Reconnection attempts continue.
    Disconnected,
    /// A finite source reached its end successfully.
    Finished,
    /// A source reached a terminal error and will not produce more lines.
    Failed(String),
}

#[derive(Debug, Default)]
struct InspectorSession {
    epoch: Option<String>,
    high_water_seq: u64,
}

impl InspectorSession {
    fn begin(&mut self, epoch: String) {
        if self.epoch.as_deref() != Some(epoch.as_str()) {
            self.epoch = Some(epoch);
            self.high_water_seq = 0;
        }
    }

    fn accept(&mut self, line: &InspectorLine) -> bool {
        let InspectorLine::Record(record) = line else {
            return true;
        };
        if record.seq == 0 {
            return true;
        }
        if record.seq <= self.high_water_seq {
            return false;
        }
        self.high_water_seq = record.seq;
        true
    }
}

pub struct EventSource {
    rx: Receiver<SourceMessage>,
    handle: Option<JoinHandle<()>>,
}

impl EventSource {
    pub fn spawn(kind: SourceKind) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = match kind {
            SourceKind::Replay(path) => Some(thread::spawn(move || replay_path(&path, &tx))),
            SourceKind::Socket { endpoint, record } => {
                // The live socket source reconnects forever; detach it
                // so quitting the TUI never waits on the reconnect loop.
                let handle = thread::spawn(move || {
                    socket_source(endpoint, record.as_deref(), &tx);
                });
                drop(handle);
                None
            },
        };
        Self { rx, handle }
    }

    pub fn drain(&self) -> Vec<SourceMessage> {
        let mut messages = Vec::new();
        while let Ok(message) = self.rx.try_recv() {
            messages.push(message);
        }
        messages
    }

    pub fn recv(&self) -> Option<SourceMessage> {
        self.rx.recv().ok()
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        // Close the receiver before joining finite replay workers so
        // they break out on their next send instead of finishing replay.
        let (_tx, rx) = mpsc::channel();
        drop(std::mem::replace(&mut self.rx, rx));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct ReplayReader {
    path: PathBuf,
    lines: Lines<BufReader<File>>,
    line_number: usize,
}

impl ReplayReader {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open replay `{}`", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            lines: BufReader::new(file).lines(),
            line_number: 0,
        })
    }

    fn next_line(&mut self) -> Result<Option<InspectorLine>> {
        let Some(line) = self.lines.next() else {
            return Ok(None);
        };
        self.line_number += 1;
        let line = line.with_context(|| {
            format!(
                "read replay `{}` line {}",
                self.path.display(),
                self.line_number
            )
        })?;
        InspectorLine::parse_line(&line)
            .with_context(|| format!("replay `{}` line {}", self.path.display(), self.line_number))
            .map(Some)
    }
}

fn replay_path(path: &Path, tx: &Sender<SourceMessage>) {
    let result: Result<bool> = (|| -> Result<bool> {
        let mut reader = ReplayReader::open(path)?;
        while let Some(line) = reader.next_line()? {
            if tx.send(SourceMessage::Line(line)).is_err() {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(120));
        }
        Ok(true)
    })();
    match result {
        Ok(true) => {
            let _ = tx.send(SourceMessage::Finished);
        },
        Ok(false) => {},
        Err(error) => {
            let _ = tx.send(SourceMessage::Failed(format!("{error:#}")));
        },
    }
}

/// Subscribe to the daemon's event stream and forward every received typed
/// line into `tx`. Reconnects with a short backoff if the daemon is not yet
/// listening, which is useful for `omnifs inspect` racing `just dev`.
fn socket_source(endpoint: EventEndpoint, record: Option<&Path>, tx: &Sender<SourceMessage>) {
    let mut record_file = match record {
        Some(path) => match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => Some(file),
            Err(error) => {
                let _ = tx.send(SourceMessage::Failed(format!(
                    "open record file `{}`: {error}",
                    path.display()
                )));
                return;
            },
        },
        None => None,
    };
    let Ok(client) = EventsClient::new(endpoint) else {
        let _ = tx.send(SourceMessage::Failed("build events client runtime".into()));
        return;
    };

    let session = RefCell::new(InspectorSession::default());

    loop {
        let outcome = client.attach(
            |instance_id| {
                session.borrow_mut().begin(instance_id.clone());
                let _ = tx.send(SourceMessage::Connected { epoch: instance_id });
            },
            |line| {
                if !session.borrow_mut().accept(line) {
                    return Ok(());
                }
                if let Some(file) = record_file.as_mut() {
                    let serialized = line
                        .to_json_line()
                        .map_err(|error| SourceForwardError::Failed(error.to_string()))?;
                    file.write_all(serialized.as_bytes())
                        .map_err(|error| SourceForwardError::Failed(error.to_string()))?;
                    file.flush()
                        .map_err(|error| SourceForwardError::Failed(error.to_string()))?;
                }
                tx.send(SourceMessage::Line(line.clone()))
                    .map_err(|_| SourceForwardError::Hangup)
            },
        );
        match outcome {
            Ok(AttachOutcome::Unreachable) => thread::sleep(Duration::from_millis(250)),
            // Stream closed (daemon shutdown or transient drop). Brief
            // backoff then reconnect; the daemon serves a fresh history
            // snapshot on the next attach.
            Ok(AttachOutcome::Ended) => {
                if tx.send(SourceMessage::Disconnected).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(500));
            },
            Ok(AttachOutcome::Failed(error)) => {
                let _ = tx.send(SourceMessage::Failed(error));
                return;
            },
            Err(SourceForwardError::Hangup) => return,
            Err(SourceForwardError::Failed(error)) => {
                let _ = tx.send(SourceMessage::Failed(format!(
                    "write inspector record: {error}"
                )));
                return;
            },
        }
    }
}

enum SourceForwardError {
    Hangup,
    Failed(String),
}

pub fn replay_file_blocking(path: &Path) -> Result<Vec<InspectorLine>> {
    let mut reader = ReplayReader::open(path)?;
    let mut lines = Vec::new();
    while let Some(line) = reader.next_line()? {
        lines.push(line);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::events::{InspectorEvent, InspectorRecord};

    #[test]
    fn replay_reports_malformed_line_as_failed_terminal_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("replay.jsonl");
        let line = InspectorLine::Record(InspectorRecord::new(
            "2026-05-23T00:00:00Z",
            1,
            7,
            InspectorEvent::FuseStart {
                op: "lookup".into(),
                mount: "github".into(),
                path: "/a".into(),
            },
        ))
        .to_json_line()
        .expect("serialize");
        std::fs::write(&path, format!("{line}not json\n")).expect("write replay");

        let source = EventSource::spawn(SourceKind::Replay(path.clone()));
        assert!(matches!(source.recv(), Some(SourceMessage::Line(_))));
        match source.recv() {
            Some(SourceMessage::Failed(error)) => {
                assert!(error.contains(&path.display().to_string()));
                assert!(error.contains("line 2"));
                assert!(error.contains("invalid json"));
            },
            Some(SourceMessage::Finished) | None => panic!("malformed replay became EOF"),
            Some(
                SourceMessage::Line(_)
                | SourceMessage::Connected { .. }
                | SourceMessage::Disconnected,
            ) => panic!("unexpected source message"),
        }
    }

    #[test]
    fn inspector_session_deduplicates_within_epoch_and_resets_between_epochs() {
        let event = InspectorEvent::FuseStart {
            op: "lookup".into(),
            mount: "github".into(),
            path: "/a".into(),
        };
        let record = |seq| {
            InspectorLine::Record(InspectorRecord::new("t", seq, 1, event.clone()).with_seq(seq))
        };
        let mut session = InspectorSession::default();
        session.begin("one".into());
        assert!(session.accept(&record(0)));
        assert_eq!(session.high_water_seq, 0);
        assert!(session.accept(&record(2)));
        assert!(!session.accept(&record(1)));
        assert!(!session.accept(&record(2)));
        assert!(session.accept(&record(3)));
        session.begin("two".into());
        assert_eq!(session.high_water_seq, 0);
        assert!(session.accept(&record(1)));
    }
}
