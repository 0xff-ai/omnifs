//! Event sources: replay file, live typed control-plane subscriber.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Lines, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use omnifs_api::{CONTROL_REQUEST_TIMEOUT_SECS, events::InspectorLine};
use tokio::net::UnixStream;

const SOURCE_QUEUE_CAPACITY: usize = 1024;
const MESSAGES_PER_FRAME: usize = 256;
const INSPECTOR_SETUP_TIMEOUT: Duration = Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplaySpeed {
    Quarter,
    Half,
    #[default]
    Normal,
    Double,
    Quadruple,
}

impl ReplaySpeed {
    const ALL: [Self; 5] = [
        Self::Quarter,
        Self::Half,
        Self::Normal,
        Self::Double,
        Self::Quadruple,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Quarter => "0.25×",
            Self::Half => "0.5×",
            Self::Normal => "1×",
            Self::Double => "2×",
            Self::Quadruple => "4×",
        }
    }

    pub fn faster(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|speed| *speed == self)
            .unwrap_or(2);
        Self::ALL[(index + 1).min(Self::ALL.len() - 1)]
    }

    pub fn slower(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|speed| *speed == self)
            .unwrap_or(2);
        Self::ALL[index.saturating_sub(1)]
    }

    const fn encoded(self) -> u8 {
        match self {
            Self::Quarter => 0,
            Self::Half => 1,
            Self::Normal => 2,
            Self::Double => 3,
            Self::Quadruple => 4,
        }
    }

    fn decode(value: u8) -> Self {
        Self::ALL
            .get(usize::from(value))
            .copied()
            .unwrap_or(Self::Normal)
    }

    const fn ratio(self) -> (u128, u128) {
        match self {
            Self::Quarter => (1, 4),
            Self::Half => (1, 2),
            Self::Normal => (1, 1),
            Self::Double => (2, 1),
            Self::Quadruple => (4, 1),
        }
    }
}

/// Outcome of one [`EventsClient::attach`] call.
pub enum AttachOutcome {
    /// Could not connect or the daemon refused the stream; retry later.
    Unreachable,
    /// Connected and streamed until the daemon closed the response.
    Ended,
    /// The connected stream produced an invalid or unreadable line.
    Failed(String),
}

fn inspector_line(item: omnifs_api::grpc::wire::InspectorStreamItem) -> Result<InspectorLine> {
    match item.value {
        Some(omnifs_api::grpc::wire::inspector_stream_item::Value::JsonLine(bytes)) => {
            serde_json::from_slice(&bytes).context("invalid inspector JSON line")
        },
        Some(omnifs_api::grpc::wire::inspector_stream_item::Value::Dropped(count)) => {
            Ok(InspectorLine::Dropped { count })
        },
        Some(omnifs_api::grpc::wire::inspector_stream_item::Value::Ready(_)) => {
            anyhow::bail!("invalid inspector stream item")
        },
        None => anyhow::bail!("invalid inspector stream item"),
    }
}

/// Blocking line-oriented client for the daemon's inspector subscription.
/// Owns a single-thread tokio runtime so callers can drive the stream from
/// plain threads over the host-native Unix socket.
pub struct EventsClient {
    rt: tokio::runtime::Runtime,
    socket: PathBuf,
}

impl EventsClient {
    pub fn new(socket: PathBuf) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build events client runtime")?;
        Ok(Self { rt, socket })
    }

    /// Try to connect once. On success, call `on_connect`, then `on_line`
    /// for every typed event until the stream ends or fails.
    pub fn attach<E>(
        &self,
        on_connect: impl FnOnce(String),
        mut on_line: impl FnMut(&InspectorLine) -> std::result::Result<(), E>,
    ) -> std::result::Result<AttachOutcome, E> {
        self.rt.block_on(async {
            let endpoint = tonic::transport::Endpoint::from_static("http://[::]:50051");
            let socket = self.socket.clone();
            let Ok(Ok(channel)) = tokio::time::timeout(
                INSPECTOR_SETUP_TIMEOUT,
                endpoint.connect_with_connector(tower::service_fn(move |_| {
                    let socket = socket.clone();
                    async move {
                        UnixStream::connect(socket)
                            .await
                            .map(hyper_util::rt::TokioIo::new)
                    }
                })),
            )
            .await
            else {
                return Ok(AttachOutcome::Unreachable);
            };
            let Ok(Ok(response)) = tokio::time::timeout(
                INSPECTOR_SETUP_TIMEOUT,
                omnifs_api::grpc::wire::control_client::ControlClient::new(channel)
                    .subscribe_inspector(tonic::Request::new(
                        omnifs_api::grpc::wire::InspectorRequest {},
                    )),
            )
            .await
            else {
                return Ok(AttachOutcome::Unreachable);
            };
            let mut stream = response.into_inner();
            let first = match tokio::time::timeout(INSPECTOR_SETUP_TIMEOUT, stream.message()).await
            {
                Ok(Ok(Some(item))) => item,
                Ok(Ok(None)) | Err(_) => return Ok(AttachOutcome::Unreachable),
                Ok(Err(error)) => return Ok(AttachOutcome::Failed(error.to_string())),
            };
            let Some(omnifs_api::grpc::wire::inspector_stream_item::Value::Ready(ready)) =
                first.value
            else {
                return Ok(AttachOutcome::Failed(
                    "inspector stream missing Ready".into(),
                ));
            };
            let instance_id = ready.instance_id;
            on_connect(instance_id);
            while let Some(item) = match stream.message().await {
                Ok(item) => item,
                Err(error) => return Ok(AttachOutcome::Failed(error.to_string())),
            } {
                let line = match inspector_line(item) {
                    Ok(line) => line,
                    Err(error) => return Ok(AttachOutcome::Failed(error.to_string())),
                };
                on_line(&line)?;
            }
            Ok(AttachOutcome::Ended)
        })
    }
}

pub enum SourceKind {
    Replay(PathBuf),
    /// Subscribe to the daemon's event stream. Optional `record` also
    /// appends every typed line read to a host-side file.
    Socket {
        endpoint: PathBuf,
        record: Option<PathBuf>,
    },
}

impl SourceKind {
    /// Whether this source replays a finite captured file rather than
    /// attaching to a live daemon. The single source of truth for the
    /// live/replay distinction: nothing else needs to track it separately,
    /// so it cannot drift out of sync with the source it describes.
    pub const fn is_replay(&self) -> bool {
        matches!(self, Self::Replay(_))
    }
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
    replay_speed: Option<Arc<AtomicU8>>,
    replay_cancelled: Option<Arc<AtomicBool>>,
}

impl EventSource {
    pub fn spawn(kind: SourceKind) -> Self {
        let (tx, rx) = mpsc::sync_channel(SOURCE_QUEUE_CAPACITY);
        let mut replay_speed = None;
        let mut replay_cancelled = None;
        let handle = match kind {
            SourceKind::Replay(path) => {
                let speed = Arc::new(AtomicU8::new(ReplaySpeed::Normal.encoded()));
                let cancelled = Arc::new(AtomicBool::new(false));
                replay_speed = Some(Arc::clone(&speed));
                replay_cancelled = Some(Arc::clone(&cancelled));
                Some(thread::spawn(move || {
                    replay_path(&path, &tx, &speed, &cancelled);
                }))
            },
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
        Self {
            rx,
            handle,
            replay_speed,
            replay_cancelled,
        }
    }

    /// Drain bounded work for one draw tick. A hot stream can neither grow
    /// memory without limit nor starve keyboard input and screen redraws.
    pub fn drain_frame(&self) -> Vec<SourceMessage> {
        let mut messages = Vec::with_capacity(MESSAGES_PER_FRAME);
        while messages.len() < MESSAGES_PER_FRAME
            && let Ok(message) = self.rx.try_recv()
        {
            messages.push(message);
        }
        messages
    }

    pub fn recv(&self) -> Option<SourceMessage> {
        self.rx.recv().ok()
    }

    pub fn set_replay_speed(&self, speed: ReplaySpeed) {
        if let Some(control) = &self.replay_speed {
            control.store(speed.encoded(), Ordering::Relaxed);
        }
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        // Close the receiver before joining finite replay workers so
        // they break out on their next send instead of finishing replay.
        let (_tx, rx) = mpsc::sync_channel(1);
        drop(std::mem::replace(&mut self.rx, rx));
        if let Some(handle) = self.handle.take() {
            if let Some(cancelled) = &self.replay_cancelled {
                cancelled.store(true, Ordering::Relaxed);
                handle.thread().unpark();
            }
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

fn replay_path(
    path: &Path,
    tx: &SyncSender<SourceMessage>,
    speed: &AtomicU8,
    cancelled: &AtomicBool,
) {
    let result: Result<bool> = (|| -> Result<bool> {
        let mut reader = ReplayReader::open(path)?;
        let mut previous_timestamp = None;
        while let Some(line) = reader.next_line()? {
            if let Some(timestamp) = line_timestamp(&line) {
                if let Some(previous) = previous_timestamp
                    && let Some(delay) = replay_delay(
                        previous,
                        timestamp,
                        ReplaySpeed::decode(speed.load(Ordering::Relaxed)),
                    )
                {
                    thread::park_timeout(delay);
                    if cancelled.load(Ordering::Relaxed) {
                        return Ok(false);
                    }
                }
                previous_timestamp = Some(timestamp);
            }
            if tx.send(SourceMessage::Line(line)).is_err() {
                return Ok(false);
            }
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

fn line_timestamp(line: &InspectorLine) -> Option<time::OffsetDateTime> {
    let InspectorLine::Record(record) = line else {
        return None;
    };
    time::OffsetDateTime::parse(&record.ts, &time::format_description::well_known::Rfc3339).ok()
}

fn replay_delay(
    previous: time::OffsetDateTime,
    current: time::OffsetDateTime,
    speed: ReplaySpeed,
) -> Option<Duration> {
    let micros = (current - previous).whole_microseconds();
    let micros = u128::try_from(micros).ok()?;
    let (numerator, denominator) = speed.ratio();
    let scaled = micros.saturating_mul(denominator) / numerator;
    Some(Duration::from_micros(
        u64::try_from(scaled).unwrap_or(u64::MAX),
    ))
}

/// Subscribe to the daemon's event stream and forward every received typed
/// line into `tx`. Reconnects with a short backoff if the daemon is not yet
/// listening, which is useful for `omnifs inspect` racing `just dev`.
fn socket_source(endpoint: PathBuf, record: Option<&Path>, tx: &SyncSender<SourceMessage>) {
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
    use time::format_description::well_known::Rfc3339;

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

    #[test]
    fn replay_delay_uses_recorded_wall_time_at_each_speed() {
        let start =
            time::OffsetDateTime::parse("2026-05-23T12:00:00Z", &Rfc3339).expect("parse start");
        let end = time::OffsetDateTime::parse("2026-05-23T12:00:02Z", &Rfc3339).expect("parse end");
        assert_eq!(
            replay_delay(start, end, ReplaySpeed::Quarter),
            Some(Duration::from_secs(8))
        );
        assert_eq!(
            replay_delay(start, end, ReplaySpeed::Normal),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            replay_delay(start, end, ReplaySpeed::Quadruple),
            Some(Duration::from_millis(500))
        );
        assert_eq!(replay_delay(end, start, ReplaySpeed::Normal), None);
    }

    #[test]
    fn dropping_replay_interrupts_a_recorded_delay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("replay.jsonl");
        let line = |ts, seq| {
            InspectorLine::Record(
                InspectorRecord::new(
                    ts,
                    seq,
                    seq,
                    InspectorEvent::FuseStart {
                        op: "lookup".into(),
                        mount: "github".into(),
                        path: format!("/{seq}"),
                    },
                )
                .with_seq(seq),
            )
            .to_json_line()
            .expect("serialize")
        };
        std::fs::write(
            &path,
            format!(
                "{}{}",
                line("2026-05-23T00:00:00Z", 1),
                line("2026-05-24T00:00:00Z", 2)
            ),
        )
        .expect("write replay");

        let source = EventSource::spawn(SourceKind::Replay(path));
        assert!(matches!(source.recv(), Some(SourceMessage::Line(_))));
        let started = std::time::Instant::now();
        drop(source);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn inspector_stream_rejects_repeated_ready_and_empty_items() {
        let repeated_ready = omnifs_api::grpc::wire::InspectorStreamItem {
            value: Some(omnifs_api::grpc::wire::inspector_stream_item::Value::Ready(
                omnifs_api::grpc::wire::InspectorReady {
                    instance_id: "same".into(),
                },
            )),
        };
        let empty = omnifs_api::grpc::wire::InspectorStreamItem { value: None };
        assert!(inspector_line(repeated_ready).is_err());
        assert!(inspector_line(empty).is_err());
    }

    #[test]
    fn inspector_stream_maps_dropped_items() {
        let item = omnifs_api::grpc::wire::InspectorStreamItem {
            value: Some(omnifs_api::grpc::wire::inspector_stream_item::Value::Dropped(7)),
        };
        assert!(matches!(
            inspector_line(item).expect("dropped item"),
            InspectorLine::Dropped { count: 7 }
        ));
    }
}
