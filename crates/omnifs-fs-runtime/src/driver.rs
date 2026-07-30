use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use omnifs_core::{ClientOwnerId, fs};

use crate::docker::{DockerClient, DockerContainerIdentity, OwnedFilesystemContainer};
use crate::host::HostDriver;
use crate::libkrun::LibkrunRunner;
use crate::{RuntimeError, RuntimeEvent, RuntimeEventSink, RuntimeStage, RuntimeState};

/// Caller-supplied roots and executable identity used by all runtime drivers.
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    profile_root: PathBuf,
    is_default_profile: bool,
    state_root: PathBuf,
    host_log_root: PathBuf,
    runtime_root: PathBuf,
    guest_image_cache: PathBuf,
    executable: PathBuf,
}

impl RuntimePaths {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        profile_root: PathBuf,
        is_default_profile: bool,
        state_root: PathBuf,
        host_log_root: PathBuf,
        runtime_root: PathBuf,
        guest_image_cache: PathBuf,
        executable: PathBuf,
    ) -> Self {
        Self {
            profile_root,
            is_default_profile,
            state_root,
            host_log_root,
            runtime_root,
            guest_image_cache,
            executable,
        }
    }

    #[must_use]
    pub fn attachment(&self, id: &fs::Id) -> AttachmentRuntimePaths {
        AttachmentRuntimePaths {
            profile_root: self.profile_root.clone(),
            state_dir: self.state_root.join(id.as_str()),
            host_log: self.host_log_root.join(format!("filesystem-{id}.log")),
            libkrun_root: self.runtime_root.join(id.as_str()).join("libkrun"),
            guest_image_cache: self.guest_image_cache.clone(),
            executable: self.executable.clone(),
        }
    }

    #[must_use]
    pub fn profile_root(&self) -> &Path {
        &self.profile_root
    }

    #[must_use]
    pub const fn is_default_profile(&self) -> bool {
        self.is_default_profile
    }

    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    #[must_use]
    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }
}

/// Exact paths for one configured filesystem runtime.
#[derive(Debug, Clone)]
pub struct AttachmentRuntimePaths {
    profile_root: PathBuf,
    state_dir: PathBuf,
    host_log: PathBuf,
    libkrun_root: PathBuf,
    guest_image_cache: PathBuf,
    executable: PathBuf,
}

impl AttachmentRuntimePaths {
    #[must_use]
    pub fn profile_root(&self) -> &Path {
        &self.profile_root
    }

    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    #[must_use]
    pub fn host_log(&self) -> &Path {
        &self.host_log
    }

    #[must_use]
    pub fn libkrun_root(&self) -> &Path {
        &self.libkrun_root
    }

    #[must_use]
    pub fn guest_image_cache(&self) -> &Path {
        &self.guest_image_cache
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }
}

/// Image choices already read from caller-owned config.
#[derive(Debug, Clone, Default)]
pub struct RuntimeAssets {
    pub docker_image: Option<String>,
    pub guest_image: Option<String>,
}

/// Exact daemon attach endpoints. Each driver consumes only its transport.
#[derive(Debug, Clone, Default)]
pub struct AttachEndpoints {
    unix: Option<PathBuf>,
    tcp: Option<SocketAddr>,
}

impl AttachEndpoints {
    #[must_use]
    pub const fn new(unix: Option<PathBuf>, tcp: Option<SocketAddr>) -> Self {
        Self { unix, tcp }
    }

    pub fn attach_unix(&self) -> Result<&Path> {
        self.unix
            .as_deref()
            .context("daemon has no Unix filesystem attach listener")
    }

    pub fn attach_tcp(&self) -> Result<SocketAddr> {
        self.tcp
            .context("daemon has no TCP filesystem attach listener")
    }
}

/// One launch, with all config, paths, identity, endpoints, and event delivery
/// supplied by the caller.
pub struct LaunchRequest<'a> {
    pub spec: &'a fs::Spec,
    pub client_owner: ClientOwnerId,
    pub paths: &'a AttachmentRuntimePaths,
    pub assets: &'a RuntimeAssets,
    pub endpoints: &'a AttachEndpoints,
    pub events: &'a RuntimeEventSink,
}

enum Backend {
    Host(HostDriver),
    Docker(DockerClient),
    Libkrun(LibkrunRunner),
}

/// One exact configured runtime with closed host, Docker, and libkrun
/// dispatch.
pub struct RuntimeDriver {
    spec: fs::Spec,
    paths: AttachmentRuntimePaths,
    assets: RuntimeAssets,
    events: RuntimeEventSink,
    backend: Backend,
}

/// A live instance whose exact runtime identity was proved.
pub enum ConfirmedRuntime {
    Host(
        omnifs_mtab::RunnerRecord,
        omnifs_thin::host_control::RunnerPhase,
    ),
    Docker(DockerContainerIdentity, bool),
    Libkrun(omnifs_libkrun::HelperRecord),
}

impl RuntimeDriver {
    /// The only match on the persisted runtime enum.
    pub fn new(
        paths: &RuntimePaths,
        spec: fs::Spec,
        assets: RuntimeAssets,
        events: RuntimeEventSink,
    ) -> Result<Self> {
        let attachment = paths.attachment(spec.id());
        let backend = match spec.runtime() {
            fs::Runtime::Host => Backend::Host(HostDriver::new(
                attachment.state_dir().to_path_buf(),
                attachment.host_log().to_path_buf(),
                attachment.executable().to_path_buf(),
                events.clone(),
            )),
            fs::Runtime::Docker => {
                ensure!(
                    spec.protocol() == fs::Protocol::Fuse,
                    "Docker runtime requires the fuse protocol"
                );
                ensure!(
                    spec.location() == Path::new(fs::GUEST_LOCATION),
                    "Docker runtime requires location {}",
                    fs::GUEST_LOCATION
                );
                Backend::Docker(DockerClient::for_filesystem(
                    paths.profile_root(),
                    paths.is_default_profile(),
                    spec.id(),
                    assets.docker_image.as_deref(),
                    events.clone(),
                )?)
            },
            fs::Runtime::Libkrun => {
                ensure!(
                    spec.protocol() == fs::Protocol::Fuse,
                    "libkrun runtime requires the fuse protocol"
                );
                ensure!(
                    spec.location() == Path::new(fs::GUEST_LOCATION),
                    "libkrun runtime requires location {}",
                    fs::GUEST_LOCATION
                );
                Backend::Libkrun(LibkrunRunner::new(attachment.libkrun_root().to_path_buf()))
            },
        };
        Ok(Self {
            spec,
            paths: attachment,
            assets,
            events,
            backend,
        })
    }

    #[must_use]
    pub fn spec(&self) -> &fs::Spec {
        &self.spec
    }

    pub async fn confirmed(
        &self,
        client_owner: ClientOwnerId,
    ) -> std::result::Result<Option<ConfirmedRuntime>, RuntimeError> {
        let result = match &self.backend {
            Backend::Host(runner) => runner
                .confirmed(&self.spec)
                .await
                .map(|value| value.map(|(record, phase)| ConfirmedRuntime::Host(record, phase))),
            Backend::Docker(client) => client
                .confirmed(self.paths.profile_root(), client_owner, &self.spec)
                .await
                .map(|value| {
                    value.map(|(identity, running)| ConfirmedRuntime::Docker(identity, running))
                }),
            Backend::Libkrun(runner) => runner
                .confirmed()
                .map(|record| record.map(ConfirmedRuntime::Libkrun)),
        };
        result.map_err(|source| {
            let error = RuntimeError::new(RuntimeStage::Probe, source);
            self.events.emit(RuntimeEvent::Failed {
                stage: RuntimeStage::Probe,
                message: error.to_string(),
            });
            error
        })
    }

    pub async fn stop_confirmed(
        &self,
        client_owner: ClientOwnerId,
        confirmed: ConfirmedRuntime,
    ) -> std::result::Result<(), RuntimeError> {
        self.events.emit(RuntimeEvent::Stage {
            stage: RuntimeStage::Stop,
            runtime: self.spec.runtime(),
            id: self.spec.id().clone(),
            state: RuntimeState::Stopping,
        });
        let result: anyhow::Result<()> = match (&self.backend, confirmed) {
            (Backend::Host(runner), ConfirmedRuntime::Host(record, _)) => {
                runner.stop_confirmed(&record).await
            },
            (Backend::Docker(client), ConfirmedRuntime::Docker(identity, _)) => {
                client
                    .stop_confirmed(
                        &identity,
                        self.paths.profile_root(),
                        client_owner,
                        &self.spec,
                    )
                    .await
            },
            (Backend::Libkrun(runner), ConfirmedRuntime::Libkrun(record)) => {
                runner.stop_confirmed(record).await
            },
            _ => Err(anyhow::anyhow!(
                "confirmed identity belongs to a different filesystem driver than `{}`",
                self.spec.id()
            )),
        };
        result
            .map(|()| {
                self.events.emit(RuntimeEvent::Stage {
                    stage: RuntimeStage::Stop,
                    runtime: self.spec.runtime(),
                    id: self.spec.id().clone(),
                    state: RuntimeState::Stopped,
                });
            })
            .map_err(|source| {
                let error = RuntimeError::new(RuntimeStage::Stop, source);
                self.events.emit(RuntimeEvent::Failed {
                    stage: RuntimeStage::Stop,
                    message: error.to_string(),
                });
                error
            })
    }

    pub async fn launch(
        &self,
        client_owner: ClientOwnerId,
        endpoints: &AttachEndpoints,
        attached: impl Future<Output = Result<()>>,
    ) -> std::result::Result<(), RuntimeError> {
        let request = LaunchRequest {
            spec: &self.spec,
            client_owner,
            paths: &self.paths,
            assets: &self.assets,
            endpoints,
            events: &self.events,
        };
        let stage = match self.spec.runtime() {
            fs::Runtime::Host => RuntimeStage::StartProcess,
            fs::Runtime::Docker => RuntimeStage::StartContainer,
            fs::Runtime::Libkrun => RuntimeStage::StartVm,
        };
        self.events.emit(RuntimeEvent::Stage {
            stage,
            runtime: self.spec.runtime(),
            id: self.spec.id().clone(),
            state: RuntimeState::Pending,
        });
        let result = match &self.backend {
            Backend::Host(runner) => runner.launch(&request).await,
            Backend::Docker(client) => client.launch(&request).await,
            Backend::Libkrun(runner) => runner.launch(&request, attached).await,
        };
        result.map_err(|source| {
            let error = RuntimeError::new(stage, source);
            self.events.emit(RuntimeEvent::Failed {
                stage,
                message: error.to_string(),
            });
            error
        })
    }

    #[must_use]
    pub fn shell_command(
        &self,
        interactive: bool,
        shell_override: Option<&str>,
        trailing: &[String],
    ) -> Option<Command> {
        match &self.backend {
            Backend::Host(_) => None,
            Backend::Docker(client) => {
                Some(client.shell_command(interactive, shell_override, trailing))
            },
            Backend::Libkrun(runner) => Some(runner.shell_command(shell_override, trailing)),
        }
    }

    #[must_use]
    pub fn container_name(&self) -> Option<&str> {
        match &self.backend {
            Backend::Docker(client) => Some(client.container_name().as_str()),
            Backend::Host(_) | Backend::Libkrun(_) => None,
        }
    }
}

/// One runtime instance found by a combined ownership scan.
pub enum Candidate {
    Host {
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
        confirmed: std::result::Result<omnifs_thin::host_control::RunnerPhase, String>,
    },
    Docker(OwnedFilesystemContainer),
    Libkrun {
        id: fs::Id,
        state_dir: PathBuf,
        confirmed: std::result::Result<Option<omnifs_libkrun::HelperRecord>, String>,
    },
    Invalid {
        backend: &'static str,
        target: Option<String>,
        error: String,
    },
    ListingFailed {
        backend: &'static str,
        error: String,
    },
}

/// Scan all runtime backends without letting one listing failure hide another.
pub async fn owned_filesystems(
    paths: &RuntimePaths,
    docker: Option<&DockerClient>,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    match crate::host::owned(paths.state_root()).await {
        Ok(mut owned) => candidates.append(&mut owned),
        Err(error) => candidates.push(Candidate::ListingFailed {
            backend: "host",
            error: format!("{error:#}"),
        }),
    }
    if let Some(docker) = docker {
        match docker.owned(paths.profile_root()).await {
            Ok(mut owned) => candidates.append(&mut owned),
            Err(error) => candidates.push(Candidate::ListingFailed {
                backend: "docker",
                error: format!("{error:#}"),
            }),
        }
    }
    candidates.append(&mut LibkrunRunner::owned(paths.runtime_root()));
    candidates
}

pub(crate) fn ensure_record_matches(record_spec: &fs::Spec, expected: &fs::Spec) -> Result<()> {
    ensure!(
        record_spec == expected,
        "runner record does not match configured filesystem `{}`",
        expected.id()
    );
    Ok(())
}

pub(crate) fn ensure_identity_unchanged<T: PartialEq>(
    current: Option<&T>,
    expected: &T,
    noun: &str,
) -> Result<()> {
    ensure!(
        current == Some(expected),
        "{noun} identity changed; refusing to touch its replacement"
    );
    Ok(())
}

pub fn err_after_rollback<T>(primary: anyhow::Error, cleanup: Result<()>, what: &str) -> Result<T> {
    Err(match cleanup {
        Ok(()) => primary,
        Err(cleanup_error) => primary.context(format!(
            "{what} also could not be cleaned up: {cleanup_error:#}"
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> RuntimePaths {
        RuntimePaths::new(
            root.to_path_buf(),
            false,
            root.join("state"),
            root.join("logs"),
            root.join("runtime"),
            root.join("guest-images"),
            root.join("omnifs"),
        )
    }

    fn spec(runtime: fs::Runtime, protocol: fs::Protocol, location: &str) -> fs::Spec {
        fs::Spec::new(
            fs::Id::new("main").unwrap(),
            protocol,
            runtime,
            PathBuf::from(location),
        )
        .unwrap()
    }

    #[test]
    fn dispatches_each_closed_runtime_variant() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let events = RuntimeEventSink::discard();
        assert!(matches!(
            RuntimeDriver::new(
                &paths,
                spec(fs::Runtime::Host, fs::Protocol::Nfs, "/tmp/main"),
                RuntimeAssets::default(),
                events.clone(),
            )
            .unwrap()
            .backend,
            Backend::Host(_)
        ));
        assert!(matches!(
            RuntimeDriver::new(
                &paths,
                spec(fs::Runtime::Docker, fs::Protocol::Fuse, fs::GUEST_LOCATION),
                RuntimeAssets::default(),
                events.clone(),
            )
            .unwrap()
            .backend,
            Backend::Docker(_)
        ));
        assert!(matches!(
            RuntimeDriver::new(
                &paths,
                spec(fs::Runtime::Libkrun, fs::Protocol::Fuse, fs::GUEST_LOCATION),
                RuntimeAssets::default(),
                events,
            )
            .unwrap()
            .backend,
            Backend::Libkrun(_)
        ));
    }

    #[test]
    fn strict_specs_reject_invalid_guest_runtime_inputs_before_dispatch() {
        for runtime in [fs::Runtime::Docker, fs::Runtime::Libkrun] {
            let result = fs::Spec::new(
                fs::Id::new("main").unwrap(),
                fs::Protocol::Nfs,
                runtime,
                PathBuf::from("/tmp/not-guest"),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn docker_dispatch_rejects_an_invalid_image_reference() {
        let temp = tempfile::tempdir().unwrap();
        let error = RuntimeDriver::new(
            &paths(temp.path()),
            spec(fs::Runtime::Docker, fs::Protocol::Fuse, fs::GUEST_LOCATION),
            RuntimeAssets {
                docker_image: Some("   ".to_owned()),
                guest_image: None,
            },
            RuntimeEventSink::discard(),
        )
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("image reference must not be empty")
        );
    }

    #[test]
    fn uses_only_caller_supplied_paths() {
        let root = Path::new("/caller/runtime");
        let paths = paths(root);
        let id = fs::Id::new("work").unwrap();
        let attachment = paths.attachment(&id);
        assert_eq!(attachment.state_dir(), root.join("state/work"));
        assert_eq!(attachment.host_log(), root.join("logs/filesystem-work.log"));
        assert_eq!(attachment.libkrun_root(), root.join("runtime/work/libkrun"));
        assert_eq!(attachment.guest_image_cache(), root.join("guest-images"));
        assert_eq!(attachment.executable(), root.join("omnifs"));
    }

    #[test]
    fn record_and_runtime_identity_rechecks_fail_closed() {
        let recorded = spec(fs::Runtime::Host, fs::Protocol::Nfs, "/tmp/recorded");
        let configured = spec(fs::Runtime::Host, fs::Protocol::Nfs, "/tmp/configured");
        assert!(
            ensure_record_matches(&recorded, &configured)
                .unwrap_err()
                .to_string()
                .contains("runner record does not match")
        );
        assert!(
            ensure_identity_unchanged(Some(&2_u8), &1_u8, "runner")
                .unwrap_err()
                .to_string()
                .contains("refusing to touch its replacement")
        );
    }

    #[test]
    fn rollback_keeps_the_primary_failure_and_reports_cleanup_failure() {
        let error = err_after_rollback::<()>(
            anyhow::anyhow!("mount failed"),
            Err(anyhow::anyhow!("stop failed")),
            "the failed runtime",
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.starts_with("the failed runtime also could not be cleaned up"));
        assert!(message.contains("stop failed"));
        assert!(message.ends_with("mount failed"));
    }
}
