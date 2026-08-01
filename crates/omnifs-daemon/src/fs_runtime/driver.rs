use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use omnifs_core::{
    FILESYSTEM_GUEST_LOCATION, FilesystemProtocol, FilesystemRuntime, FilesystemSpec, ResourceName,
    filesystem_pair_supported_on_current_host,
};

use crate::fs_runtime::docker::{DockerClient, DockerContainerIdentity, OwnedFilesystemContainer};
use crate::fs_runtime::host::HostDriver;
use crate::fs_runtime::libkrun::LibkrunRunner;
use crate::fs_runtime::{RuntimeError, RuntimeEvent, RuntimeEventSink, RuntimeStage, RuntimeState};

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

fn short_filesystem_hash(name: &ResourceName) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(name.as_str().as_bytes());
    hex::encode(&digest[..8])
}

impl RuntimePaths {
    /// Construct daemon-owned filesystem paths. The caller supplies daemon
    /// state roots, so this crate never resolves a profile or creates
    /// client-owned state.
    #[must_use]
    pub fn daemon_owned(
        profile_root: PathBuf,
        is_default_profile: bool,
        filesystems_root: PathBuf,
        filesystem_logs_root: PathBuf,
        guest_image_cache: PathBuf,
        executable: PathBuf,
    ) -> Self {
        Self {
            profile_root,
            is_default_profile,
            state_root: filesystems_root,
            host_log_root: filesystem_logs_root,
            guest_image_cache,
            executable,
        }
    }

    /// Construct daemon-owned filesystem paths directly from the daemon's own
    /// state roots. `fs_runtime` must never read `OMNIFS_HOME` itself, so the
    /// caller resolves `is_default_profile` and supplies every path.
    #[must_use]
    pub fn from_daemon_state(
        profile_root: PathBuf,
        is_default_profile: bool,
        state: &omnifs_state::DaemonStatePaths,
        executable: PathBuf,
    ) -> Self {
        Self::daemon_owned(
            profile_root,
            is_default_profile,
            state.filesystems_runtime(),
            state.filesystem_logs(),
            state.guest_images_cache(),
            executable,
        )
    }

    #[must_use]
    pub fn filesystem(&self, name: &ResourceName) -> FilesystemRuntimePaths {
        let state_dir = self.state_root.join(name.as_str());
        FilesystemRuntimePaths {
            profile_root: self.profile_root.clone(),
            state_dir: state_dir.clone(),
            host_log: self.host_log_root.join(format!("{name}.log")),
            host_control_socket: self
                .profile_root
                .join(".r")
                .join(format!("{}.sock", short_filesystem_hash(name))),
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
pub struct FilesystemRuntimePaths {
    profile_root: PathBuf,
    state_dir: PathBuf,
    host_log: PathBuf,
    host_control_socket: PathBuf,
    libkrun_root: PathBuf,
    guest_image_cache: PathBuf,
    executable: PathBuf,
}

impl FilesystemRuntimePaths {
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
    pub filesystem: &'a ResourceName,
    pub spec: &'a FilesystemSpec,
    pub runtime_instance: &'a str,
    pub paths: &'a FilesystemRuntimePaths,
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
    filesystem: ResourceName,
    spec: FilesystemSpec,
    paths: FilesystemRuntimePaths,
    events: RuntimeEventSink,
    backend: Backend,
}

/// A live instance whose exact runtime identity was proved.
pub enum ConfirmedRuntime {
    Host(omnifs_mtab::RunnerRecord),
    Docker(DockerContainerIdentity, bool),
    Libkrun(omnifs_libkrun::HelperRecord, bool),
}

impl ConfirmedRuntime {
    #[must_use]
    pub fn runtime_instance(&self) -> &str {
        match self {
            Self::Host(record) => &record.instance_id,
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
            Self::Host(_) => true,
            Self::Docker(_, running) | Self::Libkrun(_, running) => *running,
        }
    }
}

impl RuntimeDriver {
    /// The only match on the persisted runtime enum.
    pub fn new(
        paths: &RuntimePaths,
        filesystem: ResourceName,
        spec: FilesystemSpec,
        events: RuntimeEventSink,
    ) -> Result<Self> {
        ensure!(
            filesystem_pair_supported_on_current_host(spec.protocol(), spec.runtime()),
            "{}/{} is not supported on this daemon host",
            spec.protocol(),
            spec.runtime()
        );
        let runtime_paths = paths.filesystem(&filesystem);
        let backend = match spec.runtime() {
            FilesystemRuntime::Host => Backend::Host(HostDriver::new_with_control_socket(
                runtime_paths.state_dir().to_path_buf(),
                runtime_paths.host_log().to_path_buf(),
                runtime_paths.host_control_socket().to_path_buf(),
                runtime_paths.executable().to_path_buf(),
                events.clone(),
            )),
            FilesystemRuntime::Docker => {
                ensure!(
                    spec.protocol() == FilesystemProtocol::Fuse,
                    "Docker runtime requires the fuse protocol"
                );
                ensure!(
                    spec.location() == Path::new(FILESYSTEM_GUEST_LOCATION),
                    "Docker runtime requires location {FILESYSTEM_GUEST_LOCATION}"
                );
                Backend::Docker(DockerClient::for_filesystem(
                    paths.profile_root(),
                    paths.is_default_profile(),
                    &filesystem,
                    spec.docker_image(),
                    events.clone(),
                )?)
            },
            FilesystemRuntime::Libkrun => {
                ensure!(
                    spec.protocol() == FilesystemProtocol::Fuse,
                    "libkrun runtime requires the fuse protocol"
                );
                ensure!(
                    spec.location() == Path::new(FILESYSTEM_GUEST_LOCATION),
                    "libkrun runtime requires location {FILESYSTEM_GUEST_LOCATION}"
                );
                Backend::Libkrun(LibkrunRunner::new(
                    runtime_paths.libkrun_root().to_path_buf(),
                ))
            },
        };
        Ok(Self {
            spec,
            filesystem,
            paths: runtime_paths,
            events,
            backend,
        })
    }

    pub async fn confirmed(
        &self,
        runtime_instance: &str,
    ) -> std::result::Result<Option<ConfirmedRuntime>, RuntimeError> {
        let result = match &self.backend {
            Backend::Host(runner) => runner
                .confirmed(&self.filesystem, &self.spec)
                .await
                .and_then(|value| {
                    value
                        .map(|(record, _phase)| {
                            ensure!(
                                record.instance_id == runtime_instance,
                                "host runtime instance changed before exact confirmation"
                            );
                            Ok(ConfirmedRuntime::Host(record))
                        })
                        .transpose()
                }),
            Backend::Docker(client) => client
                .confirmed(
                    self.paths.profile_root(),
                    &self.filesystem,
                    &self.spec,
                    runtime_instance,
                )
                .await
                .map(|value| {
                    value.map(|(identity, running)| ConfirmedRuntime::Docker(identity, running))
                }),
            Backend::Libkrun(runner) => runner
                .confirmed(&self.filesystem, &self.spec)
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
            filesystem: self.filesystem.clone(),
            state: RuntimeState::Stopping,
        });
        let result: anyhow::Result<()> = match (&self.backend, confirmed) {
            (Backend::Host(runner), ConfirmedRuntime::Host(record)) => {
                runner.stop_confirmed(&record).await
            },
            (Backend::Docker(client), ConfirmedRuntime::Docker(identity, _)) => {
                client
                    .stop_confirmed(
                        &identity,
                        self.paths.profile_root(),
                        &self.filesystem,
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
                self.filesystem
            )),
        };
        result
            .map(|()| {
                self.events.emit(RuntimeEvent::Stage {
                    stage: RuntimeStage::Stop,
                    runtime: self.spec.runtime(),
                    filesystem: self.filesystem.clone(),
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

    pub async fn launch<Fut>(
        &self,
        runtime_instance: &str,
        endpoints: &AttachEndpoints,
        attached: impl FnOnce() -> Fut,
    ) -> std::result::Result<(), RuntimeError>
    where
        Fut: Future<Output = Result<()>>,
    {
        let request = LaunchRequest {
            filesystem: &self.filesystem,
            spec: &self.spec,
            runtime_instance,
            paths: &self.paths,
            endpoints,
            events: &self.events,
        };
        let stage = match self.spec.runtime() {
            FilesystemRuntime::Host => RuntimeStage::StartProcess,
            FilesystemRuntime::Docker => RuntimeStage::StartContainer,
            FilesystemRuntime::Libkrun => RuntimeStage::StartVm,
        };
        self.events.emit(RuntimeEvent::Stage {
            stage,
            runtime: self.spec.runtime(),
            filesystem: self.filesystem.clone(),
            state: RuntimeState::Pending,
        });
        let result = match &self.backend {
            Backend::Host(runner) => runner.launch(&request).await,
            Backend::Docker(client) => client.launch(&request).await,
            Backend::Libkrun(runner) => runner.launch(&request, attached()).await,
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
        filesystem: ResourceName,
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
    match crate::fs_runtime::host::owned(paths.state_root()).await {
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
        runtime: FilesystemRuntime,
        protocol: FilesystemProtocol,
        location: &str,
    ) -> FilesystemSpec {
        FilesystemSpec::new(
            protocol,
            runtime,
            PathBuf::from(location),
            (runtime == FilesystemRuntime::Docker).then(|| "omnifs-filesystem:dev".into()),
            (runtime == FilesystemRuntime::Libkrun).then(|| "guest.raw".into()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn ownership_scan_keeps_host_errors_and_later_backend_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        for name in ["host-dead", "host-later"] {
            let state_dir = paths.state_root().join(name);
            std::fs::create_dir_all(&state_dir).unwrap();
            let record = omnifs_mtab::RunnerRecord {
                version: omnifs_mtab::RunnerRecord::VERSION,
                instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
                pid: 42,
                process_group: 42,
                filesystem: ResourceName::new(name).unwrap(),
                spec: spec(
                    FilesystemRuntime::Host,
                    FilesystemProtocol::Nfs,
                    &format!("/mnt/{name}"),
                ),
                control_socket: state_dir.join("missing-control.sock"),
            };
            std::fs::write(
                state_dir.join("runner.json"),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
        }
        std::fs::create_dir_all(paths.state_root().join("libkrun-later/libkrun")).unwrap();

        let candidates = owned_filesystems(&paths, None).await;
        assert!(candidates.iter().any(|candidate| matches!(
            candidate,
            Candidate::Host {
                record,
                confirmed: Err(_),
                ..
            } if record.filesystem.as_str() == "host-dead"
        )));
        assert!(candidates.iter().any(|candidate| matches!(
            candidate,
            Candidate::Host {
                record,
                confirmed: Err(_),
                ..
            } if record.filesystem.as_str() == "host-later"
        )));
        assert!(candidates.iter().any(|candidate| matches!(
            candidate,
            Candidate::Libkrun {
                filesystem,
                confirmed: Ok(None),
                ..
            } if filesystem.as_str() == "libkrun-later"
        )));
    }

    #[test]
    fn daemon_owned_paths_stay_under_filesystem_state() {
        let root = Path::new("/tmp/omnifs-daemon");
        let paths = RuntimePaths::daemon_owned(
            root.to_path_buf(),
            false,
            root.join("runtime/filesystems"),
            root.join("logs/filesystems"),
            root.join("cache/guest-images"),
            root.join("omnifs"),
        );
        let filesystem = paths.filesystem(&ResourceName::new("work").unwrap());
        assert_eq!(
            filesystem.state_dir(),
            root.join("runtime/filesystems/work")
        );
        assert_eq!(
            filesystem.host_log(),
            root.join("logs/filesystems/work.log")
        );
        assert_eq!(
            filesystem.host_control_socket(),
            root.join(".r/00e13ed7af55b276.sock")
        );
        assert_eq!(
            filesystem.libkrun_root(),
            root.join("runtime/filesystems/work/libkrun")
        );
        assert_eq!(
            filesystem.guest_image_cache(),
            root.join("cache/guest-images")
        );
    }

    #[test]
    fn dispatches_each_closed_runtime_variant() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let events = RuntimeEventSink::discard();
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(matches!(
            RuntimeDriver::new(
                &paths,
                name(),
                spec(
                    FilesystemRuntime::Host,
                    FilesystemProtocol::Nfs,
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
                    FilesystemRuntime::Docker,
                    FilesystemProtocol::Fuse,
                    FILESYSTEM_GUEST_LOCATION,
                ),
                events.clone(),
            )
            .unwrap()
            .backend,
            Backend::Docker(_)
        ));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(matches!(
            RuntimeDriver::new(
                &paths,
                name(),
                spec(
                    FilesystemRuntime::Libkrun,
                    FilesystemProtocol::Fuse,
                    FILESYSTEM_GUEST_LOCATION,
                ),
                events,
            )
            .unwrap()
            .backend,
            Backend::Libkrun(_)
        ));
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        assert!(
            RuntimeDriver::new(
                &paths,
                name(),
                spec(
                    FilesystemRuntime::Libkrun,
                    FilesystemProtocol::Fuse,
                    FILESYSTEM_GUEST_LOCATION,
                ),
                events,
            )
            .is_err()
        );
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
        for runtime in [FilesystemRuntime::Docker, FilesystemRuntime::Libkrun] {
            let result = FilesystemSpec::new(
                FilesystemProtocol::Nfs,
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
            FilesystemSpec::new(
                FilesystemProtocol::Fuse,
                FilesystemRuntime::Docker,
                FILESYSTEM_GUEST_LOCATION.into(),
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
        let filesystem = paths.filesystem(&name);
        assert_eq!(filesystem.state_dir(), root.join("state/work"));
        assert_eq!(filesystem.host_log(), root.join("logs/work.log"));
        assert_eq!(filesystem.libkrun_root(), root.join("state/work/libkrun"));
        assert_eq!(filesystem.guest_image_cache(), root.join("guest-images"));
        assert_eq!(filesystem.executable(), root.join("omnifs"));
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
