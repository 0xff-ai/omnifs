//! The Docker client for the optional Docker-hosted FUSE filesystem
//! (`omnifs fs attach|detach`). The daemon itself always runs
//! host-native and has no Docker surface here.

use std::collections::HashMap;
use std::fmt;
#[cfg(target_os = "linux")]
use std::net::Ipv4Addr;
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
use omnifs_core::fs;

use crate::error::WithHint;
use crate::fs_container::{
    FILESYSTEM_HOME_LABEL, FILESYSTEM_ID_LABEL, assert_locked_down,
    build_filesystem_container_body, filesystem_command,
};
use crate::image::{BUILD_CHANNEL, BuildChannel, ImageRef};
use crate::ui::output::Output;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ContainerName(String);

impl ContainerName {
    pub(crate) fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        validate_container_name(&name)?;
        Ok(Self(name))
    }

    pub(crate) fn as_str(&self) -> &str {
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
pub(crate) struct DockerTarget {
    container_name: ContainerName,
    image: ImageRef,
}

impl DockerTarget {
    pub(crate) fn new(container_name: String, image: String) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: ContainerName::new(container_name)?,
            image: ImageRef::new(image)?,
        })
    }

    pub(crate) fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub(crate) fn image(&self) -> &ImageRef {
        &self.image
    }
}

#[derive(Debug, Default)]
struct LayerProgress {
    current: u64,
    total: u64,
}

pub(crate) struct DockerClient {
    docker: Docker,
    target: DockerTarget,
    output: Output,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DockerContainerIdentity {
    pub(crate) id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OwnedFilesystemContainer {
    pub(crate) identity: Option<DockerContainerIdentity>,
    pub(crate) filesystem_id: Option<String>,
    pub(crate) names: Vec<String>,
}

impl DockerClient {
    pub(crate) async fn launch(
        &self,
        home: &std::path::Path,
        spec: &fs::Spec,
        attach_port: u16,
    ) -> Result<()> {
        let body = build_filesystem_container_body(
            self.image(),
            home,
            spec,
            attach_port,
            cfg!(target_os = "linux"),
        );
        self.launch_filesystem_container(body).await?;

        let (mounts, env) = self.inspect_mounts_and_env().await?;
        if let Err(violation) = assert_locked_down(&mounts, &env) {
            let _ = self.remove().await;
            anyhow::bail!("refusing to run the filesystem container: {violation}");
        }
        Ok(())
    }

    pub(crate) async fn mount_ready(&self, path: &str) -> Result<bool> {
        tokio::time::timeout(Duration::from_secs(2), self.exec_path_exists(path))
            .await
            .context("Docker filesystem mount probe timed out")?
    }

    pub(crate) async fn is_running(&self) -> Result<Option<bool>> {
        self.container_running(self.container_name()).await
    }

    /// Prove that the stable workspace name still resolves to one running
    /// container with the expected workspace label, and return Docker's
    /// immutable container ID for later revalidation.
    pub(crate) async fn confirmed_filesystem(
        &self,
        expected_home: &std::path::Path,
        expected_spec: &fs::Spec,
    ) -> Result<Option<DockerContainerIdentity>> {
        Ok(self
            .confirmed_container_for_spec(expected_home, expected_spec)
            .await?
            .and_then(|(identity, running)| running.then_some(identity)))
    }

    /// Prove the workspace labels and full flat filesystem command, regardless
    /// of whether the container is running.
    pub(crate) async fn confirmed_container_for_spec(
        &self,
        expected_home: &std::path::Path,
        expected_spec: &fs::Spec,
    ) -> Result<Option<(DockerContainerIdentity, bool)>> {
        let Some((identity, running)) = self
            .confirmed_container(expected_home, expected_spec.id())
            .await?
        else {
            return Ok(None);
        };
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
        let expected_command = filesystem_command(expected_spec);
        anyhow::ensure!(
            actual_command == Some(&expected_command),
            "filesystem container command does not match configured spec `{}`",
            expected_spec.id()
        );
        Ok(Some((identity, running)))
    }

    /// Prove the stable name and both ownership labels, regardless of whether
    /// the container is running. Doctor uses this for stopped containers that
    /// still reserve a filesystem ID.
    pub(crate) async fn confirmed_container(
        &self,
        expected_home: &std::path::Path,
        expected_id: &fs::Id,
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
            .context("filesystem container has no workspace ownership label")?;
        anyhow::ensure!(
            label == &expected_home.display().to_string(),
            "filesystem container workspace label `{label}` does not match `{}`",
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
        Ok(Some((DockerContainerIdentity { id }, running)))
    }

    /// List every container carrying this workspace's ownership label.
    /// Individual filesystem IDs and canonical names are validated by doctor
    /// before it offers any destructive action.
    pub(crate) async fn owned_filesystem_containers(
        &self,
        expected_home: &std::path::Path,
    ) -> Result<Vec<OwnedFilesystemContainer>> {
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
            .context("list workspace filesystem containers")?;
        Ok(containers
            .into_iter()
            .map(|container| OwnedFilesystemContainer {
                identity: container.id.map(|id| DockerContainerIdentity { id }),
                filesystem_id: container
                    .labels
                    .and_then(|labels| labels.get(FILESYSTEM_ID_LABEL).cloned()),
                names: container.names.unwrap_or_default(),
            })
            .collect())
    }

    pub(crate) async fn remove_confirmed(
        &self,
        expected: &DockerContainerIdentity,
        expected_home: &std::path::Path,
        expected_spec: &fs::Spec,
    ) -> Result<()> {
        let current = self
            .confirmed_container_for_spec(expected_home, expected_spec)
            .await?
            .context("the confirmed filesystem container no longer exists")?
            .0;
        anyhow::ensure!(
            current == *expected,
            "filesystem container identity changed; refusing to remove its replacement"
        );
        self.stop_and_remove_id(&expected.id).await
    }

    pub(crate) fn shell_command(
        &self,
        shell_override: Option<&str>,
        trailing: &[String],
    ) -> Command {
        use std::io::IsTerminal as _;

        let mut command = Command::new("docker");
        command.arg("exec").arg("-i");
        if std::io::stdin().is_terminal() {
            command.arg("-t");
        }
        command
            .arg("-w")
            .arg(fs::GUEST_LOCATION)
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
    pub(crate) fn connect_for(target: &DockerTarget, output: Output) -> Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_local_defaults()
                .context("connect to Docker daemon (is it running?)")?,
            target: target.clone(),
            output,
        })
    }

    /// This runtime's own container identity, so lifecycle operations do not
    /// thread the name back in from each caller.
    pub(crate) fn container_name(&self) -> &ContainerName {
        self.target.container_name()
    }

    /// This runtime's own image, so
    /// the Docker runner can embed it in the container body without
    /// duplicating it in the caller.
    pub(crate) fn image(&self) -> &ImageRef {
        self.target.image()
    }

    pub(crate) async fn connect_ready(
        target: &DockerTarget,
        command: &'static str,
        output: Output,
    ) -> Result<Self> {
        output.narrate("Connecting to Docker");
        let runtime = Self::connect_for(target, output)?;
        runtime
            .ping()
            .await
            .context("Docker daemon did not respond (is Docker running?)")
            .with_hint(format!(
                "Open Docker Desktop (or start the Docker daemon), then re-run `{command}`"
            ))
            .with_hint("Or run `omnifs doctor` to diagnose")?;
        Ok(runtime)
    }

    pub(crate) async fn ping(&self) -> Result<()> {
        self.docker.ping().await.map(|_| ()).map_err(Into::into)
    }

    /// Address on which the host daemon must accept the filesystem container's
    /// attach connection. Docker Desktop forwards `host.docker.internal` to
    /// host loopback. Native Linux maps that name to the default bridge
    /// gateway instead, so the daemon must bind that gateway explicitly.
    #[cfg(target_os = "linux")]
    pub(crate) async fn filesystem_attach_bind_ip(&self) -> Result<Ipv4Addr> {
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
    pub(crate) async fn inspect_image(
        &self,
        image: &str,
    ) -> std::result::Result<bollard::models::ImageInspect, bollard::errors::Error> {
        self.docker.inspect_image(image).await
    }

    pub(crate) async fn pull_image_with_progress(&self, image: &str) -> Result<()> {
        let (from_image, tag) = image
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("image `{image}` has no tag"))?;
        let opts = CreateImageOptions {
            from_image: Some(from_image.to_string()),
            tag: Some(tag.to_string()),
            ..Default::default()
        };
        let source = image.split('/').next().unwrap_or(image);
        // Its own standalone single-row block: no sibling ledger row prints
        // alongside a filesystem image pull, so the width is just its own key.
        let key_width = Output::ledger_block_width(&["filesystem image"]);
        let mut row = self.output.progress("filesystem image", key_width);
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
                    if total > 0 {
                        row.update_bytes_with(current, total, format_args!("from {source}"));
                    } else if let Some(status) = info.status.as_deref() {
                        row.update(status);
                    }
                } else if let Some(status) = info.status {
                    row.update(&status);
                }
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                row.settle_ok(format!("{image} ready"));
                Ok(())
            },
            Err(error) => {
                row.settle_fail(format!("{image} pull failed"));
                Err(error)
            },
        }
    }

    pub(crate) async fn remove(&self) -> Result<()> {
        self.remove_existing(self.container_name()).await
    }

    /// `Some(running)` when a container named `name` exists, `None` when it
    /// does not. Generic over `name` (unlike a self-targeted check) so a
    /// caller can probe the filesystem container through any connected `DockerClient`.
    pub(crate) async fn container_running(&self, name: &ContainerName) -> Result<Option<bool>> {
        match self
            .docker
            .inspect_container(name.as_str(), None::<InspectContainerOptions>)
            .await
        {
            Ok(container) => Ok(Some(
                container
                    .state
                    .as_ref()
                    .and_then(|state| state.running)
                    .unwrap_or(false),
            )),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("inspect container `{name}`")),
        }
    }

    pub(crate) async fn remove_existing(&self, name: &ContainerName) -> Result<()> {
        match self
            .docker
            .inspect_container(name.as_str(), None::<InspectContainerOptions>)
            .await
        {
            Ok(_) => {
                self.output
                    .narrate(format!("Found existing container `{name}`"));
                // Best-effort stop, then remove. Bollard returns errors for
                // already-stopped containers; we don't care about that case.
                self.output
                    .narrate(format!("Stopping existing container `{name}` (1s timeout)"));
                let _ = self
                    .docker
                    .stop_container(
                        name.as_str(),
                        Some(StopContainerOptions {
                            signal: None,
                            t: Some(1),
                        }),
                    )
                    .await;
                self.output
                    .narrate(format!("Removing existing container `{name}`"));
                self.docker
                    .remove_container(
                        name.as_str(),
                        Some(RemoveContainerOptions {
                            force: true,
                            v: true,
                            ..Default::default()
                        }),
                    )
                    .await
                    .with_context(|| format!("remove container `{name}`"))?;
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => self
                .output
                .narrate(format!("No existing container `{name}`")),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect container `{name}`"));
            },
        }
        Ok(())
    }

    async fn stop_and_remove_id(&self, id: &str) -> Result<()> {
        self.output
            .narrate(format!("Stopping confirmed filesystem container `{id}`"));
        let _ = self
            .docker
            .stop_container(
                id,
                Some(StopContainerOptions {
                    signal: None,
                    t: Some(1),
                }),
            )
            .await;
        self.docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("remove confirmed filesystem container `{id}`"))
    }

    /// Launch the filesystem container from `body`, replacing any existing
    /// container of the same name first (one filesystem container per
    /// workspace). Reuses [`Self::ensure_image`]'s dev/release pull gating.
    pub(crate) async fn launch_filesystem_container(
        &self,
        body: ContainerCreateBody,
    ) -> Result<()> {
        self.ensure_image().await?;
        self.remove().await?;

        self.output.narrate(format!(
            "Creating filesystem container `{}` from image `{}`",
            self.container_name(),
            self.image()
        ));
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
        self.output.narrate(format!(
            "Starting filesystem container `{}`",
            self.container_name()
        ));
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
    pub(crate) async fn inspect_mounts_and_env(
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
    pub(crate) async fn exec_path_exists(&self, path: &str) -> Result<bool> {
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
                match (BUILD_CHANNEL, image_age_words(inspect.created.as_deref())) {
                    (BuildChannel::Dev, Some(age)) => {
                        self.output.narrate(format!(
                            "Image `{}` present (built {age} ago)",
                            self.image()
                        ));
                    },
                    _ => self
                        .output
                        .narrate(format!("Image `{}` present", self.image())),
                }
                Ok(())
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) if !self.image().has_registry() => {
                // A registry-less reference is a local build product. Never
                // reach for a registry: refuse and point at the dev build.
                self.output
                    .narrate(format!("Image `{}` missing", self.image()));
                let image = self.image();
                Err(anyhow!(BUILD_CHANNEL.pull_refusal_reason()))
                    .context(format!("image `{image}` is not present locally"))
                    .with_hint("build it with `just filesystem-image`")
                    .with_hint("or set a specific image via the OMNIFS_FILESYSTEM_IMAGE env var or the `[filesystem].docker_image` config key")
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                self.output
                    .narrate(format!("Image `{}` missing", self.image()));
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
/// `pub(crate)` so doctor's daemon-uptime row reuses the same
/// wording instead of a second bucket table.
pub(crate) fn duration_words(secs: i64) -> String {
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
