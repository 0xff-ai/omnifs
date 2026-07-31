use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use omnifs_core::{
    ATTACHMENT_GUEST_LOCATION, AttachmentProtocol, AttachmentRuntime, AttachmentSpec, ResourceName,
    attachment_pair_supported_on_current_host,
};

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
    guest_image_cache: PathBuf,
    executable: PathBuf,
}

fn short_attachment_hash(name: &ResourceName) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(name.as_str().as_bytes());
    hex::encode(&digest[..8])
}

impl RuntimePaths {
    /// Construct daemon-owned attachment paths. The caller supplies daemon
    /// state roots, so this crate never resolves a profile or creates a
    /// fallback client layout.
    #[must_use]
    pub fn daemon_owned(
        profile_root: PathBuf,
        is_default_profile: bool,
        attachments_root: PathBuf,
        attachment_logs_root: PathBuf,
        guest_image_cache: PathBuf,
        executable: PathBuf,
    ) -> Self {
        Self {
            profile_root,
            is_default_profile,
            state_root: attachments_root.clone(),
            host_log_root: attachment_logs_root,
            guest_image_cache,
            executable,
        }
    }

    #[must_use]
    pub fn attachment(&self, name: &ResourceName) -> AttachmentRuntimePaths {
        let state_dir = self.state_root.join(name.as_str());
        AttachmentRuntimePaths {
            profile_root: self.profile_root.clone(),
            state_dir: state_dir.clone(),
            host_log: self.host_log_root.join(format!("{name}.log")),
            host_control_socket: self
                .profile_root
                .join(".r")
                .join(format!("{}.sock", short_attachment_hash(name))),
            libkrun_root: self.state_root.join(name.as_str()).join("libkrun"),
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
}

/// Exact paths for one configured filesystem runtime.
#[derive(Debug, Clone)]
pub struct AttachmentRuntimePaths {
    profile_root: PathBuf,
    state_dir: PathBuf,
    host_log: PathBuf,
    host_control_socket: PathBuf,
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
    pub fn host_control_socket(&self) -> &Path {
        &self.host_control_socket
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
    pub attachment: &'a ResourceName,
    pub spec: &'a AttachmentSpec,
    pub runtime_instance: &'a str,
    pub paths: &'a AttachmentRuntimePaths,
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
    attachment: ResourceName,
    spec: AttachmentSpec,
    paths: AttachmentRuntimePaths,
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
    Libkrun(omnifs_libkrun::HelperRecord, bool),
}

impl ConfirmedRuntime {
    #[must_use]
    pub fn runtime_instance(&self) -> &str {
        match self {
            Self::Host(record, _) => &record.instance_id,
            Self::Docker(identity, _) => &identity.runtime_instance,
            Self::Libkrun(record, _) => &record.instance_id,
        }
    }

    /// Whether the proved runtime can still establish a VFS session.
    ///
    /// Host and libkrun confirmation includes a live control or process check.
    /// Docker retains stopped containers, so its exact identity and liveness
    /// remain separate facts.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        match self {
            Self::Host(_, _) => true,
            Self::Docker(_, running) | Self::Libkrun(_, running) => *running,
        }
    }
}

impl RuntimeDriver {
    /// The only match on the persisted runtime enum.
    pub fn new(
        paths: &RuntimePaths,
        attachment: ResourceName,
        spec: AttachmentSpec,
        events: RuntimeEventSink,
    ) -> Result<Self> {
        ensure!(
            attachment_pair_supported_on_current_host(spec.protocol(), spec.runtime()),
            "{}/{} is not supported on this daemon host",
            spec.protocol(),
            spec.runtime()
        );
        let runtime_paths = paths.attachment(&attachment);
        let backend = match spec.runtime() {
            AttachmentRuntime::Host => Backend::Host(HostDriver::new_with_control_socket(
                runtime_paths.state_dir().to_path_buf(),
                runtime_paths.host_log().to_path_buf(),
                runtime_paths.host_control_socket().to_path_buf(),
                runtime_paths.executable().to_path_buf(),
                events.clone(),
            )),
            AttachmentRuntime::Docker => {
                ensure!(
                    spec.protocol() == AttachmentProtocol::Fuse,
                    "Docker runtime requires the fuse protocol"
                );
                ensure!(
                    spec.location() == Path::new(ATTACHMENT_GUEST_LOCATION),
                    "Docker runtime requires location {ATTACHMENT_GUEST_LOCATION}"
                );
                Backend::Docker(DockerClient::for_filesystem(
                    paths.profile_root(),
                    paths.is_default_profile(),
                    &attachment,
                    spec.docker_image(),
                    events.clone(),
                )?)
            },
            AttachmentRuntime::Libkrun => {
                ensure!(
                    spec.protocol() == AttachmentProtocol::Fuse,
                    "libkrun runtime requires the fuse protocol"
                );
                ensure!(
                    spec.location() == Path::new(ATTACHMENT_GUEST_LOCATION),
                    "libkrun runtime requires location {ATTACHMENT_GUEST_LOCATION}"
                );
                Backend::Libkrun(LibkrunRunner::new(
                    runtime_paths.libkrun_root().to_path_buf(),
                ))
            },
        };
        Ok(Self {
            spec,
            attachment,
            paths: runtime_paths,
            events,
            backend,
        })
    }

    #[must_use]
    pub fn spec(&self) -> &AttachmentSpec {
        &self.spec
    }

    #[must_use]
    pub fn attachment(&self) -> &ResourceName {
        &self.attachment
    }

    pub async fn confirmed(
        &self,
        runtime_instance: &str,
    ) -> std::result::Result<Option<ConfirmedRuntime>, RuntimeError> {
        let result = match &self.backend {
            Backend::Host(runner) => runner
                .confirmed(&self.attachment, &self.spec)
                .await
                .and_then(|value| {
                    value
                        .map(|(record, phase)| {
                            ensure!(
                                record.instance_id == runtime_instance,
                                "host runtime instance changed before exact confirmation"
                            );
                            Ok(ConfirmedRuntime::Host(record, phase))
                        })
                        .transpose()
                }),
            Backend::Docker(client) => client
                .confirmed(
                    self.paths.profile_root(),
                    &self.attachment,
                    &self.spec,
                    runtime_instance,
                )
                .await
                .map(|value| {
                    value.map(|(identity, running)| ConfirmedRuntime::Docker(identity, running))
                }),
            Backend::Libkrun(runner) => runner
                .confirmed(&self.attachment, &self.spec)
                .await
                .and_then(|record| {
                    record
                        .map(|(record, running)| {
                            ensure!(
                                record.instance_id == runtime_instance,
                                "libkrun runtime instance changed before exact confirmation"
                            );
                            Ok(ConfirmedRuntime::Libkrun(record, running))
                        })
                        .transpose()
                }),
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
        runtime_instance: &str,
        confirmed: ConfirmedRuntime,
    ) -> std::result::Result<(), RuntimeError> {
        self.events.emit(RuntimeEvent::Stage {
            stage: RuntimeStage::Stop,
            runtime: self.spec.runtime(),
            attachment: self.attachment.clone(),
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
                        &self.attachment,
                        &self.spec,
                        runtime_instance,
                    )
                    .await
            },
            (Backend::Libkrun(runner), ConfirmedRuntime::Libkrun(record, _)) => {
                runner.stop_confirmed(record).await
            },
            _ => Err(anyhow::anyhow!(
                "confirmed identity belongs to a different filesystem driver than `{}`",
                self.attachment
            )),
        };
        result
            .map(|()| {
                self.events.emit(RuntimeEvent::Stage {
                    stage: RuntimeStage::Stop,
                    runtime: self.spec.runtime(),
                    attachment: self.attachment.clone(),
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
        runtime_instance: &str,
        endpoints: &AttachEndpoints,
        attached: impl Future<Output = Result<()>>,
    ) -> std::result::Result<(), RuntimeError> {
        let request = LaunchRequest {
            attachment: &self.attachment,
            spec: &self.spec,
            runtime_instance,
            paths: &self.paths,
            endpoints,
            events: &self.events,
        };
        let stage = match self.spec.runtime() {
            AttachmentRuntime::Host => RuntimeStage::StartProcess,
            AttachmentRuntime::Docker => RuntimeStage::StartContainer,
            AttachmentRuntime::Libkrun => RuntimeStage::StartVm,
        };
        self.events.emit(RuntimeEvent::Stage {
            stage,
            runtime: self.spec.runtime(),
            attachment: self.attachment.clone(),
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
        attachment: ResourceName,
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
    candidates.append(&mut LibkrunRunner::owned(paths.state_root()));
    candidates
}

pub(crate) fn ensure_record_matches(
    record_attachment: &ResourceName,
    record_spec: &AttachmentSpec,
    expected_attachment: &ResourceName,
    expected_spec: &AttachmentSpec,
) -> Result<()> {
    ensure!(
        record_attachment == expected_attachment && record_spec == expected_spec,
        "runner record does not match configured Attachment `{expected_attachment}`",
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
        RuntimePaths::daemon_owned(
            root.to_path_buf(),
            false,
            root.join("state"),
            root.join("logs"),
            root.join("guest-images"),
            root.join("omnifs"),
        )
    }

    fn name() -> ResourceName {
        ResourceName::new("main").unwrap()
    }

    fn spec(
        runtime: AttachmentRuntime,
        protocol: AttachmentProtocol,
        location: &str,
    ) -> AttachmentSpec {
        AttachmentSpec::new(
            protocol,
            runtime,
            PathBuf::from(location),
            (runtime == AttachmentRuntime::Docker).then(|| "omnifs-filesystem:dev".into()),
            (runtime == AttachmentRuntime::Libkrun).then(|| "guest.raw".into()),
        )
        .unwrap()
    }

    #[test]
    fn daemon_owned_paths_stay_under_attachment_state() {
        let root = Path::new("/tmp/omnifs-daemon");
        let paths = RuntimePaths::daemon_owned(
            root.to_path_buf(),
            false,
            root.join("runtime/attachments"),
            root.join("logs/attachments"),
            root.join("cache/guest-images"),
            root.join("omnifs"),
        );
        let attachment = paths.attachment(&ResourceName::new("work").unwrap());
        assert_eq!(
            attachment.state_dir(),
            root.join("runtime/attachments/work")
        );
        assert_eq!(
            attachment.host_log(),
            root.join("logs/attachments/work.log")
        );
        assert_eq!(
            attachment.host_control_socket(),
            root.join(".r/00e13ed7af55b276.sock")
        );
        assert_eq!(
            attachment.libkrun_root(),
            root.join("runtime/attachments/work/libkrun")
        );
        assert_eq!(
            attachment.guest_image_cache(),
            root.join("cache/guest-images")
        );
    }

    #[test]
    fn dispatches_each_closed_runtime_variant() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let events = RuntimeEventSink::discard();
        assert!(matches!(
            RuntimeDriver::new(
                &paths,
                name(),
                spec(
                    AttachmentRuntime::Host,
                    AttachmentProtocol::Nfs,
                    "/tmp/main",
                ),
                events.clone(),
            )
            .unwrap()
            .backend,
            Backend::Host(_)
        ));
        assert!(matches!(
            RuntimeDriver::new(
                &paths,
                name(),
                spec(
                    AttachmentRuntime::Docker,
                    AttachmentProtocol::Fuse,
                    ATTACHMENT_GUEST_LOCATION,
                ),
                events.clone(),
            )
            .unwrap()
            .backend,
            Backend::Docker(_)
        ));
        assert!(matches!(
            RuntimeDriver::new(
                &paths,
                name(),
                spec(
                    AttachmentRuntime::Libkrun,
                    AttachmentProtocol::Fuse,
                    ATTACHMENT_GUEST_LOCATION,
                ),
                events,
            )
            .unwrap()
            .backend,
            Backend::Libkrun(_)
        ));
    }

    #[test]
    fn stopped_docker_identity_is_not_a_running_runtime() {
        let identity = DockerContainerIdentity {
            id: "container".to_owned(),
            runtime_instance: "instance".to_owned(),
        };
        assert!(!ConfirmedRuntime::Docker(identity.clone(), false).is_running());
        assert!(ConfirmedRuntime::Docker(identity, true).is_running());
    }

    #[test]
    fn strict_specs_reject_invalid_guest_runtime_inputs_before_dispatch() {
        for runtime in [AttachmentRuntime::Docker, AttachmentRuntime::Libkrun] {
            let result = AttachmentSpec::new(
                AttachmentProtocol::Nfs,
                runtime,
                PathBuf::from("/tmp/not-guest"),
                None,
                None,
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn docker_dispatch_rejects_an_invalid_image_reference() {
        let temp = tempfile::tempdir().unwrap();
        let error = RuntimeDriver::new(
            &paths(temp.path()),
            name(),
            AttachmentSpec::new(
                AttachmentProtocol::Fuse,
                AttachmentRuntime::Docker,
                ATTACHMENT_GUEST_LOCATION.into(),
                Some("   ".to_owned()),
                None,
            )
            .unwrap(),
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
        let name = ResourceName::new("work").unwrap();
        let attachment = paths.attachment(&name);
        assert_eq!(attachment.state_dir(), root.join("state/work"));
        assert_eq!(attachment.host_log(), root.join("logs/work.log"));
        assert_eq!(attachment.libkrun_root(), root.join("state/work/libkrun"));
        assert_eq!(attachment.guest_image_cache(), root.join("guest-images"));
        assert_eq!(attachment.executable(), root.join("omnifs"));
    }

    #[test]
    fn record_and_runtime_identity_rechecks_fail_closed() {
        let recorded = spec(
            AttachmentRuntime::Host,
            AttachmentProtocol::Nfs,
            "/tmp/recorded",
        );
        let configured = spec(
            AttachmentRuntime::Host,
            AttachmentProtocol::Nfs,
            "/tmp/configured",
        );
        assert!(
            ensure_record_matches(&name(), &recorded, &name(), &configured)
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
