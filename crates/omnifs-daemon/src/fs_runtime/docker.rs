//! The Docker client for optional Docker-hosted FUSE Filesystem reconciliation.
//! The daemon itself always runs host-native and has no Docker surface here.

mod container;

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bollard::Docker;
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, ListContainersOptions,
    RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use futures_util::TryStreamExt;
use omnifs_core::{FILESYSTEM_GUEST_LOCATION, FilesystemRuntime, FilesystemSpec, ResourceName};

pub use self::container::resolve_filesystem_image;
use self::container::{
    FILESYSTEM_HOME_LABEL, FILESYSTEM_ID_LABEL, assert_locked_down, filesystem_command,
};
use crate::fs_runtime::driver::{LaunchRequest, err_after_rollback};
use crate::fs_runtime::identity::ensure_identity_unchanged;
use crate::fs_runtime::{
    BUILD_CHANNEL, BuildChannel, Candidate, ImageRef, RuntimeEvent, RuntimeEventSink, RuntimeStage,
    RuntimeState,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ContainerName(String);

impl ContainerName {
    pub fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        validate_container_name(&name)?;
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn validate_container_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("container name must not be empty");
    }
    if name.len() > 64 {
        anyhow::bail!("container name must be at most 64 characters");
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("container name must not be empty");
    };
    if !first.is_ascii_alphanumeric() {
        anyhow::bail!("container name must start with an ASCII letter or digit");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        anyhow::bail!("container name may only contain ASCII letters, digits, _, ., and -");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerTarget {
    container_name: ContainerName,
    image: ImageRef,
}

impl DockerTarget {
    pub fn new(container_name: String, image: String) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: ContainerName::new(container_name)?,
            image: ImageRef::new(image)?,
        })
    }

    pub(crate) fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub fn image(&self) -> &ImageRef {
        &self.image
    }

    /// Resolve the filesystem container's target for `id`: the image
    /// through the flag > env > config > default precedence chain, and the
    /// profile's stable container name. The one recipe every Docker
    /// construction path shares.
    pub fn for_filesystem(
        profile_root: &std::path::Path,
        is_default_profile: bool,
        filesystem: &ResourceName,
        configured_image: Option<&str>,
    ) -> Result<Self> {
        let image = resolve_filesystem_image(None, configured_image)?;
        let name = Self::filesystem_container_name(profile_root, filesystem, is_default_profile)?;
        Self::new(name.as_str().to_owned(), image.as_str().to_owned())
    }
}

#[derive(Debug, Default)]
struct LayerProgress {
    current: u64,
    total: u64,
}

pub struct DockerClient {
    docker: Docker,
    target: DockerTarget,
    events: RuntimeEventSink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockerContainerIdentity {
    pub id: String,
    pub runtime_instance: String,
}

/// One container [`DockerClient::owned`] proved carries both an immutable ID
/// and a filesystem identity label. Validated at construction so a caller
/// never re-checks either field's presence downstream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedFilesystemContainer {
    pub identity: DockerContainerIdentity,
    pub filesystem_id: String,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageInspection {
    Present,
    Missing,
}

/// How long [`DockerClient::launch`] waits for the FUSE mount to appear
/// inside the freshly started filesystem container before rolling it back.
const MOUNT_READY_TIMEOUT: Duration = Duration::from_secs(5);
const MOUNT_READY_POLL_INTERVAL: Duration = Duration::from_millis(200);

impl DockerClient {
    /// Ping Docker, verify the daemon's TCP attach listener is reachable
    /// from wherever the Docker runtime lands, then launch the filesystem
    /// container and block until ready, matching the host and libkrun
    /// drivers' `launch` contract.
    pub(crate) async fn launch(&self, request: &LaunchRequest<'_>) -> Result<()> {
        self.events.emit(RuntimeEvent::Stage {
            stage: RuntimeStage::StartContainer,
            runtime: FilesystemRuntime::Docker,
            filesystem: request.filesystem.clone(),
            state: RuntimeState::Active,
        });
        self.ping()
            .await
            .context("Docker daemon did not respond (is Docker running?)")?;
        #[cfg(target_os = "linux")]
        let expected_ip = self.filesystem_attach_bind_ip().await?;
        #[cfg(not(target_os = "linux"))]
        let expected_ip = Ipv4Addr::LOCALHOST;
        let addr = request.endpoints.attach_tcp()?;
        let expected_ip = IpAddr::V4(expected_ip);
        anyhow::ensure!(
            addr.ip() == expected_ip,
            "daemon attach listener is bound to {addr}, but the Docker runtime reaches \
             {expected_ip}; restart the daemon"
        );
        self.launch_container(
            request.paths.profile_root(),
            request.filesystem,
            request.runtime_instance,
            request.spec,
            addr.port(),
        )
        .await
    }

    /// Any failure after the container starts (the lockdown audit, the
    /// exact-identity confirmation, or the mount readiness wait) rolls the
    /// container back before returning the error.
    async fn launch_container(
        &self,
        home: &std::path::Path,
        filesystem: &ResourceName,
        runtime_instance: &str,
        spec: &FilesystemSpec,
        attach_port: u16,
    ) -> Result<()> {
        let body = self.target.build_filesystem_container_body(
            home,
            filesystem,
            spec,
            runtime_instance,
            attach_port,
            cfg!(target_os = "linux"),
        );
        self.launch_filesystem_container(body).await?;

        let (mounts, env) = self.inspect_mounts_and_env().await?;
        if let Err(violation) = assert_locked_down(&mounts, &env) {
            let _ = self.remove().await;
            anyhow::bail!("refusing to run the filesystem container: {violation}");
        }

        let identity = self
            .confirmed(home, filesystem, spec, runtime_instance)
            .await?
            .map(|(identity, _running)| identity)
            .context("the launched filesystem container did not retain its exact identity")?;

        if let Err(mount_error) = self.wait_for_mount_ready().await {
            let cleanup = self
                .stop_confirmed(&identity, home, filesystem, spec, runtime_instance)
                .await;
            return err_after_rollback(mount_error, cleanup, "the failed filesystem container");
        }
        self.events.emit(RuntimeEvent::MountReady {
            runtime: FilesystemRuntime::Docker,
            filesystem: filesystem.clone(),
            location: spec.location().to_path_buf(),
            container: Some(self.container_name().to_string()),
        });
        Ok(())
    }

    async fn wait_for_mount_ready(&self) -> Result<()> {
        crate::fs_runtime::process::poll_until(
            MOUNT_READY_TIMEOUT,
            MOUNT_READY_POLL_INTERVAL,
            || async {
                Ok(self
                    .mount_ready(FILESYSTEM_GUEST_LOCATION)
                    .await?
                    .then_some(()))
            },
        )
        .await?
        .with_context(|| {
            format!(
                "{} did not appear inside the filesystem container within {}s",
                FILESYSTEM_GUEST_LOCATION,
                MOUNT_READY_TIMEOUT.as_secs()
            )
        })
    }

    async fn mount_ready(&self, path: &str) -> Result<bool> {
        tokio::time::timeout(Duration::from_secs(2), self.exec_path_exists(path))
            .await
            .context("Docker filesystem mount probe timed out")?
    }

    /// Prove a live, identity-matched filesystem container: the stable name,
    /// both ownership labels, the immutable container ID, and the full flat
    /// launch command, regardless of whether the container is running.
    /// Callers that only care about a running instance filter the returned
    /// flag themselves, matching the host and libkrun drivers' `confirmed`.
    pub async fn confirmed(
        &self,
        expected_home: &std::path::Path,
        filesystem: &ResourceName,
        expected_spec: &FilesystemSpec,
        runtime_instance: &str,
    ) -> Result<Option<(DockerContainerIdentity, bool)>> {
        let Some((identity, running)) = self
            .confirmed_name_and_labels(expected_home, filesystem)
            .await?
        else {
            return Ok(None);
        };
        // Re-inspect rather than trust the first inspect's command: this
        // proves the container has not been removed and replaced by a
        // same-named one between the label check above and this command check.
        let inspect = self
            .docker
            .inspect_container(
                self.container_name().as_str(),
                None::<InspectContainerOptions>,
            )
            .await
            .with_context(|| format!("reinspect container `{}`", self.container_name()))?;
        anyhow::ensure!(
            inspect.id.as_deref() == Some(identity.id.as_str()),
            "filesystem container changed during exact identity confirmation"
        );
        let actual_command = inspect
            .config
            .as_ref()
            .and_then(|config| config.cmd.as_ref());
        let expected_command = filesystem_command(filesystem, expected_spec, runtime_instance);
        anyhow::ensure!(
            actual_command == Some(&expected_command),
            "filesystem container command does not match configured spec `{filesystem}`"
        );
        Ok(Some((
            DockerContainerIdentity {
                id: identity.id,
                runtime_instance: runtime_instance.to_owned(),
            },
            running,
        )))
    }

    /// Prove the stable name and both ownership labels, regardless of whether
    /// the container is running. Doctor uses this for stopped containers that
    /// still reserve a filesystem ID; [`Self::confirmed`] builds on it with
    /// the full command check.
    async fn confirmed_name_and_labels(
        &self,
        expected_home: &std::path::Path,
        expected_id: &ResourceName,
    ) -> Result<Option<(DockerContainerIdentity, bool)>> {
        let inspect = match self
            .docker
            .inspect_container(
                self.container_name().as_str(),
                None::<InspectContainerOptions>,
            )
            .await
        {
            Ok(inspect) => inspect,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect container `{}`", self.container_name()));
            },
        };
        let running = inspect
            .state
            .as_ref()
            .and_then(|state| state.running)
            .unwrap_or(false);
        let label = inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .and_then(|labels| labels.get(FILESYSTEM_HOME_LABEL))
            .context("filesystem container has no profile ownership label")?;
        anyhow::ensure!(
            label == &expected_home.display().to_string(),
            "filesystem container profile label `{label}` does not match `{}`",
            expected_home.display()
        );
        let filesystem_id = inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .and_then(|labels| labels.get(FILESYSTEM_ID_LABEL))
            .context("filesystem container has no filesystem identity label")?;
        anyhow::ensure!(
            filesystem_id == expected_id.as_str(),
            "filesystem container identity label `{filesystem_id}` does not match `{expected_id}`"
        );
        let id = inspect
            .id
            .context("Docker inspect returned no immutable container ID")?;
        Ok(Some((
            DockerContainerIdentity {
                id,
                runtime_instance: String::new(),
            },
            running,
        )))
    }

    /// List every container carrying this profile's ownership label,
    /// validating each one's immutable ID and filesystem identity label at
    /// construction. Canonical names are still validated by doctor, since
    /// that check needs the resolved `DockerTarget` for the label's claimed
    /// filesystem id, not just the raw listing.
    pub async fn owned(&self, expected_home: &std::path::Path) -> Result<Vec<Candidate>> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_owned(),
            vec![format!(
                "{FILESYSTEM_HOME_LABEL}={}",
                expected_home.display()
            )],
        );
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .context("list profile filesystem containers")?;
        Ok(containers
            .into_iter()
            .map(|container| {
                let identity = container.id.map(|id| DockerContainerIdentity {
                    id,
                    runtime_instance: String::new(),
                });
                let filesystem_id = container
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(FILESYSTEM_ID_LABEL).cloned());
                let names = container.names.unwrap_or_default();
                let target = filesystem_id
                    .clone()
                    .or_else(|| identity.as_ref().map(|identity| identity.id.clone()));
                match (identity, filesystem_id) {
                    (Some(identity), Some(filesystem_id)) => {
                        Candidate::Docker(OwnedFilesystemContainer {
                            identity,
                            filesystem_id,
                            names,
                        })
                    },
                    (None, _) => Candidate::Invalid {
                        backend: "docker",
                        target,
                        error: "Docker did not return an immutable container ID".to_owned(),
                    },
                    (Some(_), None) => Candidate::Invalid {
                        backend: "docker",
                        target,
                        error: "Filesystem container has no filesystem identity label".to_owned(),
                    },
                }
            })
            .collect())
    }

    /// The one teardown entry point for a proven identity: reconfirm the
    /// container still matches `expected`, then stop and remove it.
    pub async fn stop_confirmed(
        &self,
        expected: &DockerContainerIdentity,
        expected_home: &std::path::Path,
        filesystem: &ResourceName,
        expected_spec: &FilesystemSpec,
        runtime_instance: &str,
    ) -> Result<()> {
        let Some((current, _)) = self
            .confirmed(expected_home, filesystem, expected_spec, runtime_instance)
            .await?
        else {
            return Ok(());
        };
        ensure_identity_unchanged(Some(&current), expected, "filesystem container")?;
        self.stop_and_remove_id(&expected.id).await
    }

    pub fn shell_command(
        &self,
        interactive: bool,
        shell_override: Option<&str>,
        trailing: &[String],
    ) -> Command {
        let mut command = Command::new("docker");
        command.arg("exec").arg("-i");
        if interactive {
            command.arg("-t");
        }
        command
            .arg("-w")
            .arg(FILESYSTEM_GUEST_LOCATION)
            .arg(self.container_name().as_str());
        if trailing.is_empty() {
            command.arg(shell_override.unwrap_or("/bin/sh"));
        } else {
            command.args(trailing);
        }
        command
    }
}

impl DockerClient {
    pub fn connect_for(target: &DockerTarget, events: RuntimeEventSink) -> Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_local_defaults()
                .context("connect to Docker daemon (is it running?)")?,
            target: target.clone(),
            events,
        })
    }

    /// Resolve `id`'s target and connect in one step, for a caller that
    /// wants the connected client directly rather than resolving the
    /// target and connecting as two separate steps.
    pub(crate) fn for_filesystem(
        profile_root: &std::path::Path,
        is_default_profile: bool,
        filesystem: &ResourceName,
        configured_image: Option<&str>,
        events: RuntimeEventSink,
    ) -> Result<Self> {
        let target = DockerTarget::for_filesystem(
            profile_root,
            is_default_profile,
            filesystem,
            configured_image,
        )?;
        Self::connect_for(&target, events)
    }

    /// This runtime's own container identity, so lifecycle operations do not
    /// thread the name back in from each caller.
    pub(crate) fn container_name(&self) -> &ContainerName {
        self.target.container_name()
    }

    /// This runtime's own image, so
    /// the Docker runner can embed it in the container body without
    /// duplicating it in the caller.
    pub fn image(&self) -> &ImageRef {
        self.target.image()
    }

    pub async fn ping(&self) -> Result<()> {
        self.docker.ping().await.map(|_| ()).map_err(Into::into)
    }

    /// Address on which the host daemon must accept the filesystem container's
    /// attach connection. Docker Desktop forwards `host.docker.internal` to
    /// host loopback. Native Linux maps that name to the default bridge
    /// gateway instead, so the daemon must bind that gateway explicitly.
    #[cfg(target_os = "linux")]
    async fn filesystem_attach_bind_ip(&self) -> Result<Ipv4Addr> {
        let network = self
            .docker
            .inspect_network("bridge", None)
            .await
            .context("inspect Docker's default bridge network")?;
        let gateway = network
            .ipam
            .and_then(|ipam| ipam.config)
            .into_iter()
            .flatten()
            .find_map(|config| config.gateway)
            .context("Docker's default bridge network has no gateway")?;
        gateway
            .parse()
            .with_context(|| format!("Docker bridge gateway `{gateway}` is not IPv4"))
    }

    /// Inspect an image by name. Returns the bollard result directly so callers
    /// can match on 404 vs other errors.
    pub async fn inspect_image(&self, image: &str) -> Result<ImageInspection> {
        match self.docker.inspect_image(image).await {
            Ok(_) => Ok(ImageInspection::Present),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(ImageInspection::Missing),
            Err(error) => Err(error.into()),
        }
    }

    async fn pull_image_with_progress(&self, image: &str) -> Result<()> {
        let (from_image, tag) = image
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("image `{image}` has no tag"))?;
        let opts = CreateImageOptions {
            from_image: Some(from_image.to_string()),
            tag: Some(tag.to_string()),
            ..Default::default()
        };
        let source = image.split('/').next().unwrap_or(image);
        let mut layers: HashMap<String, LayerProgress> = HashMap::new();
        let mut stream = self.docker.create_image(Some(opts), None, None);
        let result: Result<()> = async {
            while let Some(info) = stream
                .try_next()
                .await
                .with_context(|| format!("pull {image}"))?
            {
                if let Some(id) = info.id.as_deref() {
                    let layer = layers.entry(id.to_string()).or_default();
                    if let Some(progress) = info.progress_detail.as_ref() {
                        if let Some(total) = progress.total
                            && let Ok(total) = u64::try_from(total)
                            && total > 0
                        {
                            layer.total = total;
                        }
                        if let Some(current) = progress.current
                            && let Ok(current) = u64::try_from(current)
                        {
                            layer.current = current;
                        }
                    }
                    let current = layers
                        .values()
                        .fold(0_u64, |sum, layer| sum.saturating_add(layer.current));
                    let total = layers
                        .values()
                        .fold(0_u64, |sum, layer| sum.saturating_add(layer.total));
                    self.events.emit(RuntimeEvent::Download {
                        artifact: crate::fs_runtime::Artifact::FilesystemImage,
                        completed_bytes: current,
                        total_bytes: (total > 0).then_some(total),
                        source: source.to_owned(),
                    });
                }
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                self.events.emit(RuntimeEvent::DownloadFinished {
                    artifact: crate::fs_runtime::Artifact::FilesystemImage,
                    reference: image.to_owned(),
                    completed_bytes: None,
                });
                Ok(())
            },
            Err(error) => {
                self.events.emit(RuntimeEvent::DownloadFailed {
                    artifact: crate::fs_runtime::Artifact::FilesystemImage,
                    reference: Some(image.to_owned()),
                });
                Err(error)
            },
        }
    }

    pub(crate) async fn remove(&self) -> Result<()> {
        self.remove_existing(self.container_name()).await
    }

    async fn remove_existing(&self, name: &ContainerName) -> Result<()> {
        match self
            .docker
            .inspect_container(name.as_str(), None::<InspectContainerOptions>)
            .await
        {
            Ok(_) => {
                self.events.emit(RuntimeEvent::Container {
                    name: name.to_string(),
                    image: None,
                    state: crate::fs_runtime::ContainerState::RemovingExisting,
                });
                self.stop_and_remove(name.as_str()).await?;
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                self.events.emit(RuntimeEvent::Container {
                    name: name.to_string(),
                    image: None,
                    state: crate::fs_runtime::ContainerState::Absent,
                });
            },
            Err(error) => {
                return Err(error).with_context(|| format!("inspect container `{name}`"));
            },
        }
        Ok(())
    }

    async fn stop_and_remove_id(&self, id: &str) -> Result<()> {
        self.events.emit(RuntimeEvent::Container {
            name: id.to_owned(),
            image: None,
            state: crate::fs_runtime::ContainerState::StoppingConfirmed,
        });
        self.stop_and_remove(id).await
    }

    /// Best-effort stop (1s timeout; Bollard returns an error for an
    /// already-stopped container, which is not interesting here) followed by
    /// a forced remove. Shared by [`Self::remove_existing`] (a stable
    /// container name) and [`Self::stop_and_remove_id`] (an already-confirmed
    /// immutable container ID).
    async fn stop_and_remove(&self, id_or_name: &str) -> Result<()> {
        let _ = self
            .docker
            .stop_container(
                id_or_name,
                Some(StopContainerOptions {
                    signal: None,
                    t: Some(1),
                }),
            )
            .await;
        self.docker
            .remove_container(
                id_or_name,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("remove container `{id_or_name}`"))
    }

    /// Launch the filesystem container from `body`, replacing any existing
    /// container of the same name first (one filesystem container per
    /// profile). Reuses [`Self::ensure_image`]'s dev/release pull gating.
    async fn launch_filesystem_container(&self, body: ContainerCreateBody) -> Result<()> {
        self.ensure_image().await?;
        self.remove().await?;

        self.events.emit(RuntimeEvent::Container {
            name: self.container_name().to_string(),
            image: Some(self.image().to_string()),
            state: crate::fs_runtime::ContainerState::Creating,
        });
        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(self.container_name().as_str().to_string()),
                    ..Default::default()
                }),
                body,
            )
            .await
            .with_context(|| format!("create filesystem container `{}`", self.container_name()))?;
        self.events.emit(RuntimeEvent::Container {
            name: self.container_name().to_string(),
            image: Some(self.image().to_string()),
            state: crate::fs_runtime::ContainerState::Starting,
        });
        self.docker
            .start_container(
                self.container_name().as_str(),
                None::<StartContainerOptions>,
            )
            .await
            .with_context(|| format!("start filesystem container `{}`", self.container_name()))?;
        Ok(())
    }

    /// Mounts and env of the running container, for the fail-closed lockdown
    /// check run immediately after a filesystem container starts.
    async fn inspect_mounts_and_env(
        &self,
    ) -> Result<(Vec<bollard::models::MountPoint>, Vec<String>)> {
        let inspect = self
            .docker
            .inspect_container(
                self.container_name().as_str(),
                None::<InspectContainerOptions>,
            )
            .await
            .with_context(|| format!("inspect container `{}`", self.container_name()))?;
        let mounts = inspect.mounts.unwrap_or_default();
        let env = inspect
            .config
            .and_then(|config| config.env)
            .unwrap_or_default();
        Ok((mounts, env))
    }

    /// True when `path` exists inside the running container, probed with
    /// `docker exec test -e <path>`. Used to wait for the FUSE mount to come
    /// up inside the filesystem container after start.
    async fn exec_path_exists(&self, path: &str) -> Result<bool> {
        use bollard::exec::{CreateExecOptions, StartExecResults};

        let exec = self
            .docker
            .create_exec(
                self.container_name().as_str(),
                CreateExecOptions::<&str> {
                    cmd: Some(vec!["test", "-e", path]),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("create exec probe in `{}`", self.container_name()))?;
        // Drain the attached stream to completion before inspecting: dockerd
        // does not reliably finalize an exec whose attach client disconnects
        // early, so dropping the stream leaves the exit code unobservable.
        match self
            .docker
            .start_exec(&exec.id, None)
            .await
            .with_context(|| format!("start exec probe in `{}`", self.container_name()))?
        {
            StartExecResults::Attached { mut output, .. } => {
                while output.try_next().await.unwrap_or(None).is_some() {}
            },
            StartExecResults::Detached => {},
        }
        let inspect = self
            .docker
            .inspect_exec(&exec.id)
            .await
            .with_context(|| format!("inspect exec probe in `{}`", self.container_name()))?;
        Ok(inspect.exit_code == Some(0))
    }

    async fn ensure_image(&self) -> Result<()> {
        match self.docker.inspect_image(self.image().as_str()).await {
            Ok(inspect) => {
                // Surface the dev image's age so a stale local build is
                // visible; release channel keeps the terse `present`.
                let age = match (BUILD_CHANNEL, image_age_words(inspect.created.as_deref())) {
                    (BuildChannel::Dev, age) => age,
                    (BuildChannel::Release, _) => None,
                };
                self.events.emit(RuntimeEvent::Image {
                    artifact: crate::fs_runtime::Artifact::FilesystemImage,
                    reference: self.image().to_string(),
                    state: crate::fs_runtime::ImageState::Present { age },
                });
                Ok(())
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) if !self.image().has_registry() => {
                // A registry-less reference is a local build product. Never
                // reach for a registry: refuse and point at the dev build.
                self.events.emit(RuntimeEvent::Image {
                    artifact: crate::fs_runtime::Artifact::FilesystemImage,
                    reference: self.image().to_string(),
                    state: crate::fs_runtime::ImageState::Missing,
                });
                let image = self.image();
                Err(
                    anyhow!(BUILD_CHANNEL.pull_refusal_reason()).context(format!(
                        "image `{image}` is not present locally; run `just filesystem-image` to \
                     build it, or set `OMNIFS_FILESYSTEM_IMAGE` (or the profile's configured \
                     filesystem image)"
                    )),
                )
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                self.events.emit(RuntimeEvent::Image {
                    artifact: crate::fs_runtime::Artifact::FilesystemImage,
                    reference: self.image().to_string(),
                    state: crate::fs_runtime::ImageState::Missing,
                });
                self.pull_image_with_progress(self.image().as_str())
                    .await
                    .map_err(|pull_err| {
                        // When the pull itself hits a 404 the tag is likely absent
                        // from the registry. Surface an actionable message naming
                        // the tag and pointing at the remediation options instead of
                        // exposing a raw registry 404.
                        let image_str = self.image().as_str();
                        if pull_err.to_string().contains("404")
                            || pull_err.to_string().to_lowercase().contains("not found")
                        {
                            anyhow::anyhow!(
                                "image `{image_str}` was not found in the registry\n\n\
                                 This tag may not be published yet. Options:\n\
                                 - Configure a specific filesystem image in `config.toml` (for example \
                                   a release tag or a channel tag)\n\
                                 - Run `just dev` to build and launch the local sandbox\n\
                                 - Check https://ghcr.io/0xff-ai/omnifs-filesystem for available tags"
                            )
                        } else {
                            pull_err
                        }
                    })
            },
            Err(error) => Err(error).with_context(|| format!("inspect image `{}`", self.image())),
        }
    }
}

/// Render a docker image's RFC3339 `created` timestamp as a coarse relative age
/// like `3d`, `5h`, or `2m`. Returns `None` when the field is absent, unparsable,
/// or in the future so the caller falls back to a bare `present`.
fn image_age_words(created: Option<&str>) -> Option<String> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let created = OffsetDateTime::parse(created?, &Rfc3339).ok()?;
    let secs = (OffsetDateTime::now_utc() - created).whole_seconds();
    if secs < 0 {
        return None;
    }
    Some(duration_words(secs))
}

/// Coarse duration-to-words for image age: seconds, minutes, hours, or days.
fn duration_words(secs: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    if secs < MINUTE {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m", secs / MINUTE)
    } else if secs < DAY {
        format!("{}h", secs / HOUR)
    } else {
        format!("{}d", secs / DAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_refusal_reason_names_dev_build_only_on_the_dev_channel() {
        assert!(
            BuildChannel::Dev
                .pull_refusal_reason()
                .contains("dev build")
        );
        assert!(
            !BuildChannel::Release
                .pull_refusal_reason()
                .contains("dev build")
        );
        assert!(
            BuildChannel::Release
                .pull_refusal_reason()
                .contains("never pulls")
        );
    }

    #[test]
    fn duration_words_buckets() {
        assert_eq!(duration_words(5), "5s");
        assert_eq!(duration_words(120), "2m");
        assert_eq!(duration_words(3 * 3600), "3h");
        assert_eq!(duration_words(3 * 86400 + 5), "3d");
    }

    #[test]
    fn image_age_words_handles_missing_and_future() {
        assert_eq!(image_age_words(None), None);
        assert_eq!(image_age_words(Some("not-a-timestamp")), None);
        // A far-future timestamp is not a sensible age.
        assert_eq!(image_age_words(Some("2999-01-01T00:00:00Z")), None);
    }
}
