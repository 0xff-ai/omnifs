//! Event sources: replay file, live typed control-plane subscriber.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
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
    /// for every newline-framed record until the stream ends or `on_line`
    /// returns an error (which propagates to the caller).
    pub fn attach<E>(
        &self,
        on_connect: impl FnOnce(),
        mut on_line: impl FnMut(&str) -> std::result::Result<(), E>,
    ) -> std::result::Result<AttachOutcome, E> {
        self.rt.block_on(async {
            match &self.endpoint {
                EventEndpoint::Unix { socket } => {
                    let mut stream = match UnixStream::connect(socket).await {
                        Ok(stream) => stream,
                        Err(_) => return Ok(AttachOutcome::Unreachable),
                    };
                    let request = ControlRequest {
                        version: CONTROL_PROTOCOL_VERSION,
                        operation: ControlOperation::SubscribeInspector,
                    };
                    let mut request_line = match serde_json::to_vec(&request) {
                        Ok(line) => line,
                        Err(_) => return Ok(AttachOutcome::Unreachable),
                    };
                    request_line.push(b'\n');
                    if stream.write_all(&request_line).await.is_err() {
                        return Ok(AttachOutcome::Unreachable);
                    }
                    let reply_line = match read_control_line(&mut stream).await {
                        Ok(line) => line,
                        Err(_) => return Ok(AttachOutcome::Unreachable),
                    };
                    let reply: ControlReply = match serde_json::from_slice(&reply_line) {
                        Ok(reply) => reply,
                        Err(_) => return Ok(AttachOutcome::Unreachable),
                    };
                    if reply.version != CONTROL_PROTOCOL_VERSION
                        || !matches!(reply.outcome, ControlOutcome::InspectorReady)
                    {
                        return Ok(AttachOutcome::Unreachable);
                    }
                    on_connect();
                    loop {
                        let line = match read_control_line(&mut stream).await {
                            Ok(line) => line,
                            Err(_) => return Ok(AttachOutcome::Ended),
                        };
                        let envelope = match serde_json::from_slice::<InspectorLine>(&line) {
                            Ok(envelope) => envelope,
                            Err(_) => return Ok(AttachOutcome::Ended),
                        };
                        match envelope {
                            InspectorLine::Record(record) => {
                                let line = match serde_json::to_string(&record) {
                                    Ok(line) => line,
                                    Err(_) => return Ok(AttachOutcome::Ended),
                                };
                                on_line(&line)?;
                            },
                            InspectorLine::Dropped { count } => {
                                let line = format!("# dropped {count} events");
                                on_line(&line)?;
                            },
                        }
                    }
                },
            }
        })
    }
}

pub enum SourceKind {
    Replay(PathBuf),
    /// Subscribe to the daemon's event stream. Optional `record` also
    /// appends every line read to a host-side file.
    Socket {
        endpoint: EventEndpoint,
        record: Option<PathBuf>,
    },
}

/// Out-of-band signal the source thread sends so the front-end can
/// surface honest connection state instead of silently looping on a
/// failed `connect_timeout`.
pub enum SourceMessage {
    Line(String),
    /// First successful socket connection, or a successful reconnect after a drop.
    Connected,
    /// Stream closed after a previously-connected session (daemon
    /// shutdown or transient drop). Reconnection attempts continue.
    Disconnected,
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

fn replay_path(path: &Path, tx: &Sender<SourceMessage>) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        if tx.send(SourceMessage::Line(line)).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(120));
    }
}

/// Subscribe to the daemon's event stream and forward every received
/// line into `tx`. Reconnects with a short backoff if the daemon is
/// not yet listening — useful for `omnifs inspect` racing
/// `just dev`. Connect/disconnect transitions are reported through
/// `SourceMessage` so the front-end never claims "connected" while the
/// stream is still failing.
fn socket_source(endpoint: EventEndpoint, record: Option<&Path>, tx: &Sender<SourceMessage>) {
    /// Receiver hung up; stop the source thread.
    struct Hangup;

    let mut record_file = record.and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });
    let Ok(client) = EventsClient::new(endpoint) else {
        return;
    };

    loop {
        let outcome = client.attach(
            || {
                let _ = tx.send(SourceMessage::Connected);
            },
            |line| {
                if let Some(file) = record_file.as_mut() {
                    let _ = writeln!(file, "{}", line.trim());
                    let _ = file.flush();
                }
                tx.send(SourceMessage::Line(line.to_string()))
                    .map_err(|_| Hangup)
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
            Err(Hangup) => return,
        }
    }
}

pub fn replay_file_blocking(path: &Path) -> Result<Vec<String>> {
    let file = File::open(path).with_context(|| format!("open `{}`", path.display()))?;
    Ok(BufReader::new(file).lines().map_while(Result::ok).collect())
}
