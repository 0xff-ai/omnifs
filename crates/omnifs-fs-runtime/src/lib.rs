//! Out-of-process filesystem runtime mechanisms.
//!
//! This crate owns exact host-process, Docker-container, and libkrun-helper
//! identity, launch, probe, stop, and stale-cleanup operations. Callers own
//! desired state, retry policy, daemon RPC, profile resolution, and terminal
//! output. Every path and configured image enters through [`RuntimePaths`] or
//! [`RuntimeAssets`].

mod docker;
mod driver;
mod events;
mod guest_image;
mod host;
mod image;
mod libkrun;
mod process;

use std::path::PathBuf;

pub use docker::{
    ContainerName, DockerClient, DockerContainerIdentity, DockerTarget, ImageInspection,
    OwnedFilesystemContainer, resolve_filesystem_image,
};
pub use driver::{
    AttachEndpoints, AttachmentRuntimePaths, Candidate, ConfirmedRuntime, LaunchRequest,
    RuntimeAssets, RuntimeDriver, RuntimePaths, err_after_rollback, owned_filesystems,
};
pub use events::{
    Artifact, ContainerState, ImageState, RuntimeEvent, RuntimeEventReceiver, RuntimeEventSink,
    RuntimeStage, RuntimeState,
};
pub use host::HostDriver;
pub use image::{BUILD_CHANNEL, BuildChannel, ImageRef};
pub use libkrun::{LibkrunRunner, default_guest_image_for, ensure_socat_available};

/// An operation failure classified by its stable runtime stage.
#[derive(Debug, thiserror::Error)]
#[error("{source:#}")]
pub struct RuntimeError {
    stage: RuntimeStage,
    advice: Vec<RuntimeAdvice>,
    #[source]
    source: anyhow::Error,
}

/// Machine-readable remediation facts. The caller owns their wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAdvice {
    Diagnose,
    DiagnoseAlternative,
    HostLog(PathBuf),
    StartDocker,
    BuildFilesystemImage,
    ConfigureFilesystemImage,
    BuildGuestImage,
}

#[derive(Debug, thiserror::Error)]
#[error("{source}")]
struct AdvisedError {
    advice: Vec<RuntimeAdvice>,
    #[source]
    source: anyhow::Error,
}

pub(crate) fn advise(source: anyhow::Error, advice: RuntimeAdvice) -> anyhow::Error {
    match source.downcast::<AdvisedError>() {
        Ok(mut advised) => {
            advised.advice.insert(0, advice);
            anyhow::Error::new(advised)
        },
        Err(source) => anyhow::Error::new(AdvisedError {
            advice: vec![advice],
            source,
        }),
    }
}

impl RuntimeError {
    #[must_use]
    pub fn new(stage: RuntimeStage, source: anyhow::Error) -> Self {
        match source.downcast::<AdvisedError>() {
            Ok(advised) => Self {
                stage,
                advice: advised.advice,
                source: advised.source,
            },
            Err(source) => Self {
                stage,
                advice: Vec::new(),
                source,
            },
        }
    }

    #[must_use]
    pub const fn stage(&self) -> RuntimeStage {
        self.stage
    }

    #[must_use]
    pub fn source_error(&self) -> &anyhow::Error {
        &self.source
    }

    #[must_use]
    pub fn advice(&self) -> &[RuntimeAdvice] {
        &self.advice
    }

    #[must_use]
    pub fn into_source(self) -> anyhow::Error {
        self.source
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_error_keeps_structured_advice_in_emission_order() {
        let source = advise(
            advise(
                anyhow::anyhow!("Docker is unavailable"),
                RuntimeAdvice::DiagnoseAlternative,
            ),
            RuntimeAdvice::StartDocker,
        );
        let error = RuntimeError::new(RuntimeStage::StartContainer, source);

        assert_eq!(
            error.advice(),
            &[
                RuntimeAdvice::StartDocker,
                RuntimeAdvice::DiagnoseAlternative,
            ]
        );
        assert_eq!(error.to_string(), "Docker is unavailable");
    }
}
