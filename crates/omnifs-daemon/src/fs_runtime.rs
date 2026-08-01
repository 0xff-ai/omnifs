//! Out-of-process filesystem runtime mechanisms.
//!
//! This module owns exact host-process, Docker-container, and libkrun-helper
//! identity, launch, probe, stop, and stale-cleanup operations. Callers own
//! desired state, retry policy, daemon RPC, profile resolution, and terminal
//! output. Every path enters through [`RuntimePaths`], while exact runtime
//! configuration enters through `omnifs_core::FilesystemSpec`.

pub(crate) mod docker;
mod driver;
mod events;
mod guest_image;
mod host;
mod identity;
mod image;
mod libkrun;
mod process;

pub use docker::{DockerClient, DockerTarget, ImageInspection, OwnedFilesystemContainer};
pub use driver::{Candidate, RuntimePaths, owned_filesystems};
pub use events::RuntimeEventSink;
pub use host::HostDriver;
pub use image::ImageRef;
pub use libkrun::LibkrunRunner;

pub(crate) use driver::{AttachEndpoints, ConfirmedRuntime, RuntimeDriver};
pub(crate) use events::{
    Artifact, ContainerState, ImageState, RuntimeEvent, RuntimeEventReceiver, RuntimeStage,
    RuntimeState,
};
pub(crate) use image::{BUILD_CHANNEL, BuildChannel};
pub(crate) use libkrun::ensure_socat_available;

/// An operation failure classified by its stable runtime stage.
#[derive(Debug, thiserror::Error)]
#[error("{source:#}")]
pub struct RuntimeError {
    stage: RuntimeStage,
    #[source]
    source: anyhow::Error,
}

impl RuntimeError {
    #[must_use]
    pub fn new(stage: RuntimeStage, source: anyhow::Error) -> Self {
        Self { stage, source }
    }

    #[must_use]
    pub const fn stage(&self) -> RuntimeStage {
        self.stage
    }
}
