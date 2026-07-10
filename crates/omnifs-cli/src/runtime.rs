//! The Docker client for the optional Docker-hosted FUSE frontend
//! (`omnifs frontend up|down|status`). The daemon itself always runs
//! host-native and has no Docker surface here.

use std::collections::HashMap;
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::net::Ipv4Addr;

use anyhow::{Context, Result, anyhow};
use bollard::Docker;
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, RemoveContainerOptions,
    StartContainerOptions, StopContainerOptions,
};
use futures_util::TryStreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::error::WithHint;
use crate::launch_backend::{
    BUILD_CHANNEL, BuildChannel, ContainerName, DockerTarget, ImageRef, names_registry,
};

/// Outcome of a Docker daemon reachability probe.
pub(crate) enum DockerProbeOutcome {
    /// Daemon responded to ping; the connected `Runtime` is returned for reuse.
    Reachable(Runtime),
    /// Could not connect to the daemon socket.
    ConnectFailed(bollard::errors::Error),
    /// Connected but the ping RPC failed.
    PingFailed(bollard::errors::Error),
}

pub(crate) struct Runtime {
    docker: Docker,
    target: DockerTarget,
}

impl Runtime {
    pub(crate) fn connect_for(target: &DockerTarget) -> Result<Self> {
        Ok(Self {
            docker: connect_docker_client()?,
            target: target.clone(),
        })
    }

    fn container_name(&self) -> &ContainerName {
        self.target.container_name()
    }

    fn image(&self) -> &ImageRef {
        self.target.image()
    }

    pub(crate) async fn connect_ready(
        target: &DockerTarget,
        command: &'static str,
    ) -> Result<Self> {
        anstream::eprintln!("Connecting to Docker");
        let runtime = Self::connect_for(target)?;
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

    /// The Docker server version string, if the daemon reports one. Used only
    /// for the informational reachability row in `omnifs setup`.
    pub(crate) async fn server_version(&self) -> Option<String> {
        self.docker.version().await.ok()?.version
    }

    /// Address on which the host daemon must accept the frontend container's
    /// attach connection. Docker Desktop forwards `host.docker.internal` to
    /// host loopback. Native Linux maps that name to the default bridge
    /// gateway instead, so the daemon must bind that gateway explicitly.
    #[cfg(target_os = "linux")]
    pub(crate) async fn frontend_attach_bind_ip(&self) -> Result<Ipv4Addr> {
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

    /// Probe Docker daemon reachability without requiring a pre-connected client.
    /// Used by `omnifs doctor` so the probe result carries a typed outcome and
    /// the resulting `Runtime` can be reused for image inspection.
    pub(crate) async fn probe_docker(target: &DockerTarget) -> DockerProbeOutcome {
        let runtime = match Self::connect_for(target) {
            Ok(r) => r,
            Err(e) => {
                // connect_for wraps bollard errors in anyhow; downcast to the
                // underlying bollard error or synthesise an IOError.
                let bollard_err = e.downcast::<bollard::errors::Error>().unwrap_or_else(|e| {
                    bollard::errors::Error::IOError {
                        err: std::io::Error::other(e.to_string()),
                    }
                });
                return DockerProbeOutcome::ConnectFailed(bollard_err);
            },
        };
        match runtime.docker.ping().await {
            Ok(_) => DockerProbeOutcome::Reachable(runtime),
            Err(e) => DockerProbeOutcome::PingFailed(e),
        }
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
        anstream::eprintln!("Pulling {image}");
        let (from_image, tag) = image
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("image `{image}` has no tag"))?;
        let opts = CreateImageOptions {
            from_image: Some(from_image.to_string()),
            tag: Some(tag.to_string()),
            ..Default::default()
        };
        let multi = MultiProgress::new();
        let style = ProgressStyle::with_template(
            "  {prefix:<14.bold} {wide_bar} {bytes:>10}/{total_bytes}",
        )
        .map_err(|e| anyhow!("progress template: {e}"))?
        .progress_chars("##-");
        let mut bars: HashMap<String, ProgressBar> = HashMap::new();
        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(info) = stream
            .try_next()
            .await
            .with_context(|| format!("pull {image}"))?
        {
            if let Some(id) = info.id.as_deref() {
                let bar = bars.entry(id.to_string()).or_insert_with(|| {
                    let bar = multi.add(ProgressBar::new(0));
                    bar.set_style(style.clone());
                    bar.set_prefix(id.to_string());
                    bar
                });
                if let Some(progress) = info.progress_detail.as_ref() {
                    if let Some(total) = progress.total
                        && total > 0
                    {
                        bar.set_length(u64::try_from(total).unwrap_or(0));
                    }
                    if let Some(current) = progress.current {
                        bar.set_position(u64::try_from(current).unwrap_or(0));
                    }
                }
                if let Some(status) = info.status.as_deref() {
                    bar.set_message(status.to_string());
                    if matches!(
                        status,
                        "Pull complete" | "Already exists" | "Download complete"
                    ) {
                        bar.finish_with_message(status.to_string());
                    }
                }
            } else if let Some(status) = info.status {
                anstream::eprintln!("  {status}");
            }
        }
        multi.clear().ok();
        anstream::eprintln!("✓ Image ready");
        Ok(())
    }

    pub(crate) async fn remove(&self) -> Result<()> {
        self.remove_existing(self.container_name()).await
    }

    /// `Some(running)` when a container named `name` exists, `None` when it
    /// does not. Generic over `name` (unlike a self-targeted check) so
    /// `omnifs frontend status` can probe the frontend container through any
    /// connected `Runtime`.
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
        anstream::eprint!("Checking for existing container `{name}` ");
        std::io::stderr().flush().ok();
        match self
            .docker
            .inspect_container(name.as_str(), None::<InspectContainerOptions>)
            .await
        {
            Ok(_) => {
                anstream::eprintln!("found");
                // Best-effort stop, then remove. Bollard returns errors for
                // already-stopped containers; we don't care about that case.
                anstream::eprintln!("Stopping existing container `{name}` (1s timeout)");
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
                anstream::eprintln!("Removing existing container `{name}`");
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
            }) => anstream::eprintln!("none"),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect container `{name}`"));
            },
        }
        Ok(())
    }

    /// Launch the frontend container from `body`, replacing any existing
    /// container of the same name first (one frontend container per
    /// workspace). Reuses [`Self::ensure_image`]'s dev/release pull gating.
    pub(crate) async fn launch_frontend_container(&self, body: ContainerCreateBody) -> Result<()> {
        self.ensure_image().await?;
        self.remove().await?;

        anstream::eprintln!(
            "Creating frontend container `{}` from image `{}`",
            self.container_name(),
            self.image()
        );
        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(self.container_name().as_str().to_string()),
                    ..Default::default()
                }),
                body,
            )
            .await
            .with_context(|| format!("create frontend container `{}`", self.container_name()))?;
        anstream::eprintln!("Starting frontend container `{}`", self.container_name());
        self.docker
            .start_container(
                self.container_name().as_str(),
                None::<StartContainerOptions>,
            )
            .await
            .with_context(|| format!("start frontend container `{}`", self.container_name()))?;
        Ok(())
    }

    /// Mounts and env of the running container, for the fail-closed lockdown
    /// check run immediately after a frontend container starts.
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
    /// up inside the frontend container after start.
    pub(crate) async fn exec_path_exists(&self, path: &str) -> Result<bool> {
        use bollard::exec::CreateExecOptions;

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
        self.docker
            .start_exec(&exec.id, None)
            .await
            .with_context(|| format!("start exec probe in `{}`", self.container_name()))?;
        let inspect = self
            .docker
            .inspect_exec(&exec.id)
            .await
            .with_context(|| format!("inspect exec probe in `{}`", self.container_name()))?;
        Ok(inspect.exit_code == Some(0))
    }

    async fn ensure_image(&self) -> Result<()> {
        anstream::eprint!("Checking image `{}` ", self.image());
        std::io::stderr().flush().ok();
        match self.docker.inspect_image(self.image().as_str()).await {
            Ok(inspect) => {
                // Surface the dev image's age so a stale local build is
                // visible; release channel keeps the terse `present`.
                match (BUILD_CHANNEL, image_age_words(inspect.created.as_deref())) {
                    (BuildChannel::Dev, Some(age)) => {
                        anstream::eprintln!("present (built {age} ago)");
                    },
                    _ => anstream::eprintln!("present"),
                }
                Ok(())
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) if !names_registry(self.image().as_str()) => {
                // A registry-less reference is a local build product. Never
                // reach for a registry: refuse and point at the dev build.
                anstream::eprintln!("missing");
                let image = self.image();
                Err(anyhow!(pull_refusal_reason(BUILD_CHANNEL)))
                    .context(format!("image `{image}` is not present locally"))
                    .with_hint("build it with `just frontend-image`")
                    .with_hint("or set a specific image via the OMNIFS_FRONTEND_IMAGE env var or the `[system].frontend_image` config key")
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                anstream::eprintln!("missing");
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
                                 - Configure a specific frontend image in `config.toml` (for example \
                                   a release tag or a channel tag)\n\
                                 - Run `just dev` to build and launch the local sandbox\n\
                                 - Check https://ghcr.io/0xff-ai/omnifs-frontend for available tags"
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

fn connect_docker_client() -> Result<Docker> {
    Docker::connect_with_local_defaults().context("connect to Docker daemon (is it running?)")
}

/// Why a registry-less image absent locally is not pulled, worded per build
/// channel: only a dev binary defaults to `omnifs-frontend:dev`, so a release
/// binary hitting this path chose a local tag explicitly and must not be told
/// it is a dev build.
const fn pull_refusal_reason(channel: BuildChannel) -> &'static str {
    match channel {
        BuildChannel::Dev => {
            "this omnifs binary is a dev build; it uses the locally built frontend image \
             and never pulls from a registry"
        },
        BuildChannel::Release => {
            "registry-less image references are local build products; omnifs never pulls \
             them from a registry"
        },
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
        assert!(pull_refusal_reason(BuildChannel::Dev).contains("dev build"));
        assert!(!pull_refusal_reason(BuildChannel::Release).contains("dev build"));
        assert!(pull_refusal_reason(BuildChannel::Release).contains("never pulls"));
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
