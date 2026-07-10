//! Event sources: replay file, live `GET /v1/events` subscriber.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use hyperlocal::{UnixConnector, Uri as UnixUri};

use crate::client::EventEndpoint;

/// Outcome of one [`EventsClient::attach`] call.
pub enum AttachOutcome {
    /// Could not connect or the daemon refused the stream; retry later.
    Unreachable,
    /// Connected and streamed until the daemon closed the response.
    Ended,
}

/// Blocking line-oriented client for the daemon's `GET /v1/events`
/// stream. Owns a single-thread tokio runtime so callers can drive the
/// HTTP stream from plain threads. Speaks TCP (reqwest) or the host-native
/// Unix socket (hyper), depending on the resolved endpoint.
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
        use omnifs_api::events::split_complete_lines;

        self.rt.block_on(async {
            match &self.endpoint {
                EventEndpoint::Tcp { base, token } => {
                    use futures_util::StreamExt as _;
                    // Built lazily, only for this TCP override path: this
                    // initializes the TLS backend (system CA probe), which
                    // must not block the unix-socket production path (the
                    // `EventEndpoint::Unix` arm below never touches it). A
                    // CA-less build failure here degrades the same as an
                    // unreachable daemon: the caller's reconnect loop
                    // retries.
                    let Ok(http) = reqwest::Client::builder()
                        .connect_timeout(Duration::from_millis(500))
                        .build()
                    else {
                        return Ok(AttachOutcome::Unreachable);
                    };
                    let mut request = http.get(format!("{base}/v1/events"));
                    if let Some(token) = token {
                        request = request.bearer_auth(token);
                    }
                    let response = match request.send().await {
                        Ok(response) if response.status().is_success() => response,
                        _ => return Ok(AttachOutcome::Unreachable),
                    };
                    on_connect();
                    let mut stream = response.bytes_stream();
                    let mut buf = String::new();
                    while let Some(chunk) = stream.next().await {
                        let Ok(chunk) = chunk else {
                            return Ok(AttachOutcome::Ended);
                        };
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        let (lines, rest) = split_complete_lines(&buf);
                        for line in &lines {
                            on_line(line)?;
                        }
                        buf = rest.to_string();
                    }
                    Ok(AttachOutcome::Ended)
                },
                EventEndpoint::Unix { socket } => {
                    let client: HyperClient<UnixConnector, Full<Bytes>> =
                        HyperClient::builder(TokioExecutor::new()).build(UnixConnector);
                    let uri: hyper::Uri = UnixUri::new(socket, "/v1/events").into();
                    let Ok(request) = hyper::Request::builder()
                        .uri(uri)
                        .body(Full::new(Bytes::new()))
                    else {
                        return Ok(AttachOutcome::Unreachable);
                    };
                    let mut response = match client.request(request).await {
                        Ok(response) if response.status().is_success() => response,
                        _ => return Ok(AttachOutcome::Unreachable),
                    };
                    on_connect();
                    let mut buf = String::new();
                    while let Some(frame) = response.body_mut().frame().await {
                        let Ok(frame) = frame else {
                            return Ok(AttachOutcome::Ended);
                        };
                        if let Some(chunk) = frame.data_ref() {
                            buf.push_str(&String::from_utf8_lossy(chunk));
                            let (lines, rest) = split_complete_lines(&buf);
                            for line in &lines {
                                on_line(line)?;
                            }
                            buf = rest.to_string();
                        }
                    }
                    Ok(AttachOutcome::Ended)
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
    /// First successful TCP connect, or a successful reconnect after a drop.
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
