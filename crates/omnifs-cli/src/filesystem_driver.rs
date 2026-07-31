#![allow(
    dead_code,
    reason = "Plan 006 removes client runtime launch code while retaining its path scanner"
)]

//! CLI adapters for the UI-free filesystem runtime crate.

use std::collections::HashMap;

use anyhow::Result;
use omnifs_core::fs;
use omnifs_fs_runtime::{
    Artifact, ContainerState, ImageState, RuntimeAdvice, RuntimeAssets, RuntimeDriver,
    RuntimeError, RuntimeEvent, RuntimeEventReceiver, RuntimeEventSink, RuntimePaths, RuntimeStage,
    RuntimeState,
};

use crate::client_fs_state::ClientFilesystemState;
use crate::error::WithHint as _;
use crate::ui::output::Output;

const EVENT_CAPACITY: usize = 128;

pub(crate) fn runtime_paths(state: &ClientFilesystemState) -> Result<RuntimePaths> {
    state.runtime_paths()
}

pub(crate) fn runtime_assets(
    state: &ClientFilesystemState,
    runtime: fs::Runtime,
) -> Result<RuntimeAssets> {
    if runtime == fs::Runtime::Host {
        return Ok(RuntimeAssets::default());
    }
    let config = state.config()?;
    Ok(RuntimeAssets {
        docker_image: config.filesystem.docker_image,
        guest_image: config.filesystem.guest_image,
    })
}

pub(crate) fn runtime_driver(
    state: &ClientFilesystemState,
    spec: &fs::Spec,
    events: RuntimeEventSink,
) -> Result<RuntimeDriver> {
    RuntimeDriver::new(
        &runtime_paths(state)?,
        spec.clone(),
        runtime_assets(state, spec.runtime())?,
        events,
    )
}

pub(crate) fn into_cli_error(error: RuntimeError) -> anyhow::Error {
    let advice = error.advice().to_vec();
    let mut result: anyhow::Result<()> = Err(error.into_source());
    for item in advice {
        result = result.with_hint(match item {
            RuntimeAdvice::Diagnose => "Run `omnifs doctor` to diagnose".to_owned(),
            RuntimeAdvice::DiagnoseAlternative => "Or run `omnifs doctor` to diagnose".to_owned(),
            RuntimeAdvice::HostLog(path) => format!("See {}", path.display()),
            RuntimeAdvice::StartDocker => "Open Docker Desktop (or start the Docker daemon), then \
                 re-run `omnifs fs attach`"
                .to_owned(),
            RuntimeAdvice::BuildFilesystemImage => {
                "build it with `just filesystem-image`".to_owned()
            },
            RuntimeAdvice::ConfigureFilesystemImage => "or set a specific image via the \
                 OMNIFS_FILESYSTEM_IMAGE env var or the `[filesystem].docker_image` config key"
                .to_owned(),
            RuntimeAdvice::BuildGuestImage => "Build it with `just guest-image` (see \
                 docs/contracts/60-build-validation.md)"
                .to_owned(),
        });
    }
    result.expect_err("runtime error adapter must remain an error")
}

/// Owns the bounded runtime event receiver and renders facts through the
/// invocation's existing output mode.
pub(crate) struct RuntimeEventRenderer {
    task: tokio::task::JoinHandle<()>,
}

impl RuntimeEventRenderer {
    pub(crate) fn start(output: Output) -> (RuntimeEventSink, Self) {
        let (events, receiver) = RuntimeEventSink::bounded(EVENT_CAPACITY);
        let task = tokio::spawn(render_events(output, receiver));
        (events, Self { task })
    }

    pub(crate) async fn finish(self) {
        let _ = self.task.await;
    }
}

async fn render_events(output: Output, mut receiver: RuntimeEventReceiver) {
    let mut renderer = EventRenderState {
        output,
        progress: HashMap::new(),
        vm_progress: None,
    };
    while let Some(event) = receiver.recv().await {
        renderer.render(event);
    }
}

struct EventRenderState {
    output: Output,
    progress: HashMap<Artifact, crate::ui::live::Spinner>,
    vm_progress: Option<crate::ui::live::Spinner>,
}

impl EventRenderState {
    fn render(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Stage {
                stage,
                runtime,
                id,
                state,
            } => self.render_stage(stage, runtime, &id, state),
            RuntimeEvent::Image {
                artifact: _,
                reference,
                state,
            } => self.render_image(&reference, state),
            RuntimeEvent::Download {
                artifact,
                completed_bytes,
                total_bytes,
                source,
            } => self.render_download(artifact, completed_bytes, total_bytes, &source),
            RuntimeEvent::DownloadFinished {
                artifact,
                reference,
                completed_bytes,
            } => self.render_download_finished(artifact, &reference, completed_bytes),
            RuntimeEvent::DownloadFailed {
                artifact,
                reference,
            } => self.render_download_failed(artifact, reference.as_deref()),
            RuntimeEvent::ImageRetry {
                artifact,
                path,
                reason,
            } => self.render_image_retry(artifact, &path, &reason),
            RuntimeEvent::Container { name, image, state } => {
                self.render_container(&name, image.as_deref(), state);
            },
            RuntimeEvent::MountReady {
                runtime,
                id,
                location,
                container,
            } => self.render_mount_ready(runtime, &id, &location, container.as_deref()),
            RuntimeEvent::Failed { message, .. } => self.render_failed(message),
        }
    }

    fn render_stage(
        &mut self,
        stage: RuntimeStage,
        runtime: fs::Runtime,
        id: &fs::Id,
        lifecycle_state: RuntimeState,
    ) {
        match (stage, lifecycle_state) {
            (
                RuntimeStage::StartProcess | RuntimeStage::StartContainer | RuntimeStage::StartVm,
                RuntimeState::Pending,
            ) => self
                .output
                .narrate(format!("Starting {runtime} filesystem `{id}`")),
            (RuntimeStage::StartContainer, RuntimeState::Active) => {
                self.output.narrate("Connecting to Docker");
            },
            (RuntimeStage::StartVm, RuntimeState::Active) => {
                self.vm_progress.get_or_insert_with(|| {
                    self.output
                        .progress("filesystem", Output::ledger_block_width(&["filesystem"]))
                });
            },
            (RuntimeStage::MaterializeImage, RuntimeState::Active) => {
                self.update_vm_progress("materializing guest image");
            },
            (RuntimeStage::WaitForOsMount, RuntimeState::Active) => {
                self.update_vm_progress("booting guest");
            },
            (RuntimeStage::WaitForVfsSession, RuntimeState::Active) => {
                self.update_vm_progress("attaching to daemon");
            },
            (RuntimeStage::WaitForVfsSession, RuntimeState::Ready) => {
                if let Some(row) = self.vm_progress.take() {
                    row.settle_ok("guest ready");
                }
            },
            (RuntimeStage::Stop, RuntimeState::Stopping) if runtime == fs::Runtime::Libkrun => {
                self.update_vm_progress("cleaning up failed launch");
            },
            _ => {},
        }
    }

    fn update_vm_progress(&mut self, message: &str) {
        if let Some(row) = self.vm_progress.as_mut() {
            row.update(message);
        }
    }

    fn render_image(&self, reference: &str, state: ImageState) {
        match state {
            ImageState::Present { age: Some(age) } => self
                .output
                .narrate(format!("Image `{reference}` present (built {age} ago)")),
            ImageState::Present { age: None } => {
                self.output.narrate(format!("Image `{reference}` present"));
            },
            ImageState::Missing => self.output.narrate(format!("Image `{reference}` missing")),
        }
    }

    fn render_download(
        &mut self,
        artifact: Artifact,
        completed_bytes: u64,
        total_bytes: Option<u64>,
        source: &str,
    ) {
        let label = artifact_label(artifact);
        let row = self.progress.entry(artifact).or_insert_with(|| {
            self.output
                .progress(label, Output::ledger_block_width(&[label]))
        });
        if let Some(total) = total_bytes {
            row.update_bytes_with(completed_bytes, total, format_args!("from {source}"));
        } else {
            row.update(&format!(
                "{} from {source}",
                crate::ui::live::human_bytes(completed_bytes)
            ));
        }
    }

    fn render_download_finished(
        &mut self,
        artifact: Artifact,
        reference: &str,
        completed_bytes: Option<u64>,
    ) {
        let row = self.progress.remove(&artifact).unwrap_or_else(|| {
            let label = artifact_label(artifact);
            self.output
                .progress(label, Output::ledger_block_width(&[label]))
        });
        match (artifact, completed_bytes) {
            (Artifact::GuestImage, Some(bytes)) => row.settle_ok(format!(
                "{}, verified (cached for next time)",
                crate::ui::live::human_bytes(bytes)
            )),
            _ => row.settle_ok(format!("{reference} ready")),
        }
    }

    fn render_image_retry(&self, artifact: Artifact, path: &std::path::Path, reason: &str) {
        let label = artifact_label(artifact);
        let mut row = self
            .output
            .progress(label, Output::ledger_block_width(&[label]));
        row.update("retrying");
        row.settle_warn(format!(
            "cached image at {} is corrupt ({reason}); retrying",
            path.display()
        ));
    }

    fn render_download_failed(&mut self, artifact: Artifact, reference: Option<&str>) {
        let row = self.progress.remove(&artifact).unwrap_or_else(|| {
            let label = artifact_label(artifact);
            self.output
                .progress(label, Output::ledger_block_width(&[label]))
        });
        match reference {
            Some(reference) => row.settle_fail(format!("{reference} pull failed")),
            None => row.settle_fail("download failed"),
        }
    }

    fn render_container(&self, name: &str, image: Option<&str>, state: ContainerState) {
        match state {
            ContainerState::Absent => self
                .output
                .narrate(format!("No existing container `{name}`")),
            ContainerState::RemovingExisting => self.output.narrate(format!(
                "Removing existing container `{name}` (1s stop timeout)"
            )),
            ContainerState::Creating => self.output.narrate(format!(
                "Creating filesystem container `{name}` from image `{}`",
                image.unwrap_or_default()
            )),
            ContainerState::Starting => self
                .output
                .narrate(format!("Starting filesystem container `{name}`")),
            ContainerState::StoppingConfirmed => self
                .output
                .narrate(format!("Stopping confirmed filesystem container `{name}`")),
        }
    }

    fn render_mount_ready(
        &self,
        runtime: fs::Runtime,
        id: &fs::Id,
        location: &std::path::Path,
        container: Option<&str>,
    ) {
        if runtime == fs::Runtime::Libkrun {
            return;
        }
        let detail = container.map_or_else(
            || format!("{id} mounted at {}", location.display()),
            |container| format!("{id} running in `{container}`"),
        );
        self.output.ledger_row(
            &crate::ui::render::LedgerRow::new(crate::ui::style::Glyph::Done, "filesystem", detail),
            Output::ledger_block_width(&["filesystem"]),
        );
    }

    fn render_failed(&mut self, message: String) {
        for (_, row) in self.progress.drain() {
            row.settle_fail("download failed");
        }
        if let Some(row) = self.vm_progress.take() {
            row.settle_fail(message);
        }
    }
}

const fn artifact_label(artifact: Artifact) -> &'static str {
    match artifact {
        Artifact::FilesystemImage => "filesystem image",
        Artifact::GuestImage => "guest image",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_runtime_does_not_read_filesystem_image_config() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        let state = ClientFilesystemState::under_root(&profile.join("client"));
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("config.toml"), "not valid toml = [").unwrap();

        assert!(runtime_assets(&state, fs::Runtime::Host).is_ok());
        assert!(runtime_assets(&state, fs::Runtime::Docker).is_err());
    }
}
