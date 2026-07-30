use std::path::PathBuf;

use omnifs_core::fs;

/// Runtime artifact whose bytes or verification state changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Artifact {
    FilesystemImage,
    GuestImage,
}

/// Stable operation stage used by progress and failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeStage {
    Probe,
    MaterializeImage,
    StartProcess,
    StartContainer,
    StartVm,
    WaitForOsMount,
    WaitForVfsSession,
    Stop,
}

/// Closed lifecycle state for stage events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Pending,
    Active,
    Ready,
    Stopping,
    Stopped,
}

/// Closed image-state facts used by the CLI renderer and daemon progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageState {
    Present { age: Option<String> },
    Missing,
}

/// Closed container actions with distinct user-visible meanings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Absent,
    RemovingExisting,
    Creating,
    Starting,
    StoppingConfirmed,
}

/// Facts emitted by runtime work. These variants contain no terminal policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Stage {
        stage: RuntimeStage,
        runtime: fs::Runtime,
        id: fs::Id,
        state: RuntimeState,
    },
    Image {
        artifact: Artifact,
        reference: String,
        state: ImageState,
    },
    Download {
        artifact: Artifact,
        completed_bytes: u64,
        total_bytes: Option<u64>,
        source: String,
    },
    /// The artifact reached its ready state. Guest images emit this only
    /// after digest and byte-count verification.
    DownloadFinished {
        artifact: Artifact,
        reference: String,
        completed_bytes: Option<u64>,
    },
    DownloadFailed {
        artifact: Artifact,
        reference: Option<String>,
    },
    ImageRetry {
        artifact: Artifact,
        path: PathBuf,
        reason: String,
    },
    Container {
        name: String,
        image: Option<String>,
        state: ContainerState,
    },
    MountReady {
        runtime: fs::Runtime,
        id: fs::Id,
        location: PathBuf,
        container: Option<String>,
    },
    Failed {
        stage: RuntimeStage,
        message: String,
    },
}

/// Bounded, non-blocking runtime event producer.
///
/// [`Self::emit`] uses `try_send`, so runtime work never waits for rendering
/// or status consumers. A full or closed channel drops the fact and returns
/// `false`; the owner can obtain current truth from its normal status source.
#[derive(Clone, Debug)]
pub struct RuntimeEventSink {
    sender: Option<tokio::sync::mpsc::Sender<RuntimeEvent>>,
}

pub type RuntimeEventReceiver = tokio::sync::mpsc::Receiver<RuntimeEvent>;

impl RuntimeEventSink {
    #[must_use]
    pub fn bounded(capacity: usize) -> (Self, RuntimeEventReceiver) {
        assert!(capacity > 0, "runtime event capacity must be nonzero");
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        (
            Self {
                sender: Some(sender),
            },
            receiver,
        )
    }

    #[must_use]
    pub const fn discard() -> Self {
        Self { sender: None }
    }

    /// Emit one fact without waiting for channel capacity.
    pub fn emit(&self, event: RuntimeEvent) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(event).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(state: RuntimeState) -> RuntimeEvent {
        RuntimeEvent::Stage {
            stage: RuntimeStage::StartProcess,
            runtime: fs::Runtime::Host,
            id: fs::Id::new("main").unwrap(),
            state,
        }
    }

    #[tokio::test]
    async fn bounded_sink_never_waits_for_a_slow_consumer() {
        let (sink, mut receiver) = RuntimeEventSink::bounded(1);
        assert!(sink.emit(event(RuntimeState::Active)));
        assert!(!sink.emit(event(RuntimeState::Ready)));
        assert_eq!(receiver.recv().await, Some(event(RuntimeState::Active)));
    }

    #[test]
    fn dropped_receiver_does_not_fail_runtime_work() {
        let (sink, receiver) = RuntimeEventSink::bounded(1);
        drop(receiver);
        assert!(!sink.emit(event(RuntimeState::Active)));
    }

    #[tokio::test]
    async fn facts_keep_order_and_report_only_real_byte_totals() {
        let (sink, mut receiver) = RuntimeEventSink::bounded(2);
        let unknown_total = RuntimeEvent::Download {
            artifact: Artifact::FilesystemImage,
            completed_bytes: 41,
            total_bytes: None,
            source: "registry".to_owned(),
        };
        let known_total = RuntimeEvent::Download {
            artifact: Artifact::GuestImage,
            completed_bytes: 42,
            total_bytes: Some(100),
            source: "registry".to_owned(),
        };

        assert!(sink.emit(unknown_total.clone()));
        assert!(sink.emit(known_total.clone()));
        assert_eq!(receiver.recv().await, Some(unknown_total));
        assert_eq!(receiver.recv().await, Some(known_total));
    }
}
