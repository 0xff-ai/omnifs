use std::collections::HashMap;
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, DeviceMapping, HostConfig, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, RemoveContainerOptions,
    StartContainerOptions, StopContainerOptions,
};
use futures_util::{StreamExt, TryStreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::error::WithHint;
use crate::launch_backend::{
    BUILD_CHANNEL, BuildChannel, ContainerName, DockerTarget, ENV_CONTAINER_NAME, ENV_IMAGE,
    names_registry,
};
use crate::launch_backend::{GUEST_HOME, GUEST_MOUNT, ImageRef};
use omnifs_workspace::layout::OMNIFS_HOME_ENV;

/// Image label written by `Dockerfile` from the `OMNIFS_MIN_LAUNCHER_VERSION`
/// build arg. The launcher reads it before `docker create` to refuse running
/// an image baked from a newer source tree than the launcher itself.
const LAUNCHER_VERSION_LABEL: &str = "ai.0xff.omnifs.min-launcher-version";
const LAUNCH_PROTOCOL_LABEL: &str = "ai.0xff.omnifs.launch-protocol";

/// Derived from `omnifs_api::API_MAJOR` so the image-label check and the
/// control-API check are one fact in two places that cannot drift independently.
/// A unit test in this module verifies the string matches the numeric constant.
const EXPECTED_LAUNCH_PROTOCOL: &str = "daemon-control-v3";

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

    /// Stream the last (all, by default) lines of container stdout+stderr using
    /// bollard's logs API. Non-follow snapshot path for `omnifs logs`.
    pub(crate) async fn container_logs(
        &self,
        container_name: &ContainerName,
        tail: Option<&str>,
    ) -> Result<()> {
        use bollard::query_parameters::LogsOptions;

        let mut stream = self.docker.logs(
            container_name.as_str(),
            Some(LogsOptions {
                stdout: true,
                stderr: true,
                timestamps: false,
                tail: tail.unwrap_or("all").to_string(),
                ..Default::default()
            }),
        );
        while let Some(chunk) = stream.next().await {
            let line = chunk.with_context(|| format!("read logs from `{container_name}`"))?;
            anstream::print!("{line}");
        }
        Ok(())
    }

    /// Stream `/tmp/omnifs.log` from inside the container via `tail -F`.
    /// Blocks until the process exits (user Ctrl-C or container stops).
    ///
    /// Using exec+tail rather than bollard's follow-logs because the daemon
    /// writes through a `tee` pipe that buffers at EOL boundaries, so
    /// bollard's `logs` API only surfaces entrypoint stdout/stderr, not the
    /// runtime log file.
    pub(crate) async fn exec_follow_log(&self, container_name: &ContainerName) -> Result<()> {
        use bollard::exec::{CreateExecOptions, StartExecResults};

        let exec = self
            .docker
            .create_exec(
                container_name.as_str(),
                CreateExecOptions::<&str> {
                    cmd: Some(vec!["tail", "-F", "/tmp/omnifs.log"]),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("create exec in `{container_name}`"))?;

        let StartExecResults::Attached { mut output, .. } = self
            .docker
            .start_exec(&exec.id, None)
            .await
            .with_context(|| format!("start exec in `{container_name}`"))?
        else {
            anyhow::bail!("expected attached exec output");
        };

        while let Some(msg) = output.next().await {
            let chunk = msg.with_context(|| format!("read exec output from `{container_name}`"))?;
            anstream::print!("{chunk}");
        }
        Ok(())
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

    pub(crate) async fn launch_container(
        &self,
        runtime_home: &Path,
        extra_binds: Vec<String>,
        extra_env: Vec<String>,
        reuse_existing: bool,
    ) -> Result<()> {
        self.ensure_image().await?;
        self.verify_launcher_compat().await?;

        // Non-destructive callers can reuse a matching running container and
        // let reconcile handle config changes. Dev launches force recreation so
        // env vars and fixture binds always match the requested session.
        if reuse_existing && self.running_container_matches_image().await? {
            anstream::eprintln!(
                "Container `{}` is already running on image `{}`; skipping recreate",
                self.container_name(),
                self.image()
            );
            return Ok(());
        }

        self.remove().await?;

        let mut binds = vec![format!("{}:{GUEST_HOME}", runtime_home.display())];
        if let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") {
            let host_sock = PathBuf::from(&sock);
            if host_sock.exists() {
                binds.push(format!("{}:/ssh-agent", host_sock.display()));
            } else {
                anstream::eprintln!(
                    "SSH_AUTH_SOCK={} does not exist; git callouts will not work",
                    host_sock.display()
                );
            }
        }
        let docker_sock = PathBuf::from("/var/run/docker.sock");
        if docker_sock.exists() {
            binds.push("/var/run/docker.sock:/var/run/docker.sock:ro".to_string());
        }
        binds.extend(extra_binds);

        anstream::eprintln!(
            "Creating container `{}` from image `{}`",
            self.container_name(),
            self.image()
        );
        let create =
            Self::build_container_body(self.container_name(), self.image(), binds, extra_env);
        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(self.container_name().as_str().to_string()),
                    ..Default::default()
                }),
                create,
            )
            .await
            .with_context(|| format!("create container `{}`", self.container_name()))?;
        anstream::eprintln!("Starting container `{}`", self.container_name());
        self.docker
            .start_container(
                self.container_name().as_str(),
                None::<StartContainerOptions>,
            )
            .await
            .with_context(|| format!("start container `{}`", self.container_name()))?;
        Ok(())
    }

    /// Returns `true` when the container with our name is running and was
    /// created from the desired image. Used by [`Self::launch_container`] to
    /// skip remove+recreate when the healthy setup is already in place.
    async fn running_container_matches_image(&self) -> Result<bool> {
        match self
            .docker
            .inspect_container(
                self.container_name().as_str(),
                None::<InspectContainerOptions>,
            )
            .await
        {
            Ok(container) => {
                let running = container
                    .state
                    .as_ref()
                    .and_then(|s| s.running)
                    .unwrap_or(false);
                if !running {
                    return Ok(false);
                }
                // Check the image name recorded in Docker's container config.
                // It stores the tag the container was created from, which is
                // what we compare against.
                let container_image = container
                    .config
                    .as_ref()
                    .and_then(|c| c.image.as_deref())
                    .unwrap_or("");
                Ok(container_image == self.image().as_str())
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("inspect container `{}`", self.container_name()))
            },
        }
    }

    pub(crate) async fn remove(&self) -> Result<()> {
        self.remove_existing(self.container_name()).await
    }

    /// `Some(running)` when a container named `name` exists, `None` when it
    /// does not. Generic over `name` (unlike [`Self::running_container_matches_image`],
    /// which checks `self`'s own target) so `omnifs frontend status` can probe
    /// the frontend container through any connected `Runtime`.
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
    /// workspace, matching `launch_container`'s daemon-container semantics).
    /// Reuses [`Self::ensure_image`]'s dev/release pull gating: the frontend
    /// image is resolved and checked exactly like the daemon runtime image.
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

    pub(crate) async fn wait_for_daemon_ready(
        &self,
        client: &crate::client::DaemonClient,
    ) -> Result<()> {
        anstream::eprintln!(
            "Waiting for {GUEST_MOUNT} inside `{}`",
            self.container_name()
        );
        for attempt in 0..60 {
            if client.ready().await {
                anstream::eprintln!("✓ FUSE mount is ready");
                return Ok(());
            }
            if let Ok(container) = self
                .docker
                .inspect_container(
                    self.container_name().as_str(),
                    None::<InspectContainerOptions>,
                )
                .await
                && let Some(state) = container.state
                && state.running == Some(false)
            {
                let exit_code = state.exit_code.unwrap_or_default();
                let status = state
                    .status
                    .map_or_else(|| "exited".to_string(), |status| status.to_string());
                return Err(anyhow::anyhow!(
                    "container `{}` {status} before {GUEST_MOUNT} became available (exit {exit_code})",
                    self.container_name()
                ))
                .with_hint(format!(
                    "`docker logs {}` may show why the daemon failed to mount",
                    self.container_name()
                ));
            }
            if attempt > 0 && attempt % 5 == 0 {
                anstream::eprint!(".");
                std::io::stderr().flush().ok();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        anstream::eprintln!();
        Err(anyhow::anyhow!(
            "{GUEST_MOUNT} did not become available inside `{}` within 60s",
            self.container_name()
        ))
        .with_hint(format!(
            "`docker logs {}` may show why the daemon failed to mount",
            self.container_name()
        ))
        .with_hint("Run `omnifs doctor` to verify Docker, FUSE, and image cache")
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
                    .with_hint("build it with `just dev --build-only`")
                    .with_hint("or set a specific image via the OMNIFS_IMAGE env var or the `[system].image` config key")
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
                                 - Configure a specific runtime image in `config.toml` (for example \
                                   a release tag or a channel tag)\n\
                                 - Run `just dev` to build and launch the local sandbox\n\
                                 - Check https://ghcr.io/0xff-ai/omnifs for available tags"
                            )
                        } else {
                            pull_err
                        }
                    })
            },
            Err(error) => Err(error).with_context(|| format!("inspect image `{}`", self.image())),
        }
    }

    /// Pre-`docker create` check: read the image's
    /// `ai.0xff.omnifs.min-launcher-version` label and refuse to
    /// launch if this CLI is older than the value. Catches the
    /// footgun where a contributor's `omnifs` on PATH is an older
    /// release than the daemon baked into the image (new ports, env
    /// vars, or mounts get silently dropped because the launcher
    /// doesn't know to set them).
    async fn verify_launcher_compat(&self) -> Result<()> {
        let image = self
            .docker
            .inspect_image(self.image().as_str())
            .await
            .with_context(|| format!("inspect image `{}` for compatibility label", self.image()))?;
        let labels = image.config.as_ref().and_then(|c| c.labels.as_ref());
        let min_launcher = labels.and_then(|l| l.get(LAUNCHER_VERSION_LABEL));
        let launch_protocol = labels.and_then(|l| l.get(LAUNCH_PROTOCOL_LABEL));
        check_image_compat(
            env!("CARGO_PKG_VERSION"),
            min_launcher.map(String::as_str),
            launch_protocol.map(String::as_str),
            self.image().as_str(),
        )
    }

    fn build_container_body(
        container_name: &ContainerName,
        image: &ImageRef,
        binds: Vec<String>,
        extra_env: Vec<String>,
    ) -> ContainerCreateBody {
        let mut port_bindings = HashMap::new();
        let port = omnifs_api::DEFAULT_PORT;
        let port_key = format!("{port}/tcp");
        port_bindings.insert(
            port_key.clone(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(port.to_string()),
            }]),
        );

        let host_config = HostConfig {
            binds: Some(binds),
            port_bindings: Some(port_bindings),
            devices: Some(vec![DeviceMapping {
                path_on_host: Some("/dev/fuse".to_string()),
                path_in_container: Some("/dev/fuse".to_string()),
                cgroup_permissions: Some("rwm".to_string()),
            }]),
            cap_add: Some(vec!["SYS_ADMIN".to_string()]),
            security_opt: Some(vec!["apparmor:unconfined".to_string()]),
            ..Default::default()
        };

        let env = vec![
            format!("{OMNIFS_HOME_ENV}={GUEST_HOME}"),
            format!("{ENV_CONTAINER_NAME}={container_name}"),
            format!("{ENV_IMAGE}={image}"),
            "SSH_AUTH_SOCK=/ssh-agent".to_string(),
            "GIT_SSH_COMMAND=ssh -F /dev/null -o StrictHostKeyChecking=accept-new".to_string(),
        ]
        .into_iter()
        .chain(extra_env)
        .collect();

        ContainerCreateBody {
            image: Some(image.as_str().to_string()),
            env: Some(env),
            exposed_ports: Some(vec![port_key]),
            host_config: Some(host_config),
            ..Default::default()
        }
    }
}

fn connect_docker_client() -> Result<Docker> {
    Docker::connect_with_local_defaults().context("connect to Docker daemon (is it running?)")
}

fn check_image_compat(
    launcher_version: &str,
    min_launcher_label: Option<&str>,
    launch_protocol_label: Option<&str>,
    image: &str,
) -> Result<()> {
    check_launch_protocol(image, launch_protocol_label)?;
    check_launcher_compat(launcher_version, min_launcher_label)
}

fn check_launch_protocol(image: &str, label: Option<&str>) -> Result<()> {
    match label {
        Some(EXPECTED_LAUNCH_PROTOCOL) => Ok(()),
        Some(other) => anyhow::bail!(
            "runtime image `{image}` uses launch protocol `{other}`, but this CLI expects \
             `{EXPECTED_LAUNCH_PROTOCOL}`. Configure a matching runtime image in `config.toml`, \
             or run `just dev` to build and launch the local sandbox."
        ),
        None => anyhow::bail!(
            "runtime image `{image}` does not declare `{LAUNCH_PROTOCOL_LABEL}`. It was likely \
             built before the daemon control-API launcher. Configure a matching runtime image in \
             `config.toml`, or run `just dev` to build and launch the local sandbox."
        ),
    }
}

/// Compare the running launcher's version to the image's
/// min-launcher-version label. Pure function so the policy is
/// covered by unit tests without spinning up Docker.
///
/// Policy:
/// - Missing label (older image): warn, allow. Preserves
///   compatibility with images built before this handshake landed.
/// - Sentinel `"unknown"` (image built without the build arg): warn,
///   allow. Same reason.
/// - Unparseable label or launcher version: warn, allow. Do not break
///   launch on a parse failure; leave a breadcrumb instead.
/// - Launcher version `< label`: refuse.
fn check_launcher_compat(launcher_version: &str, label: Option<&str>) -> Result<()> {
    use semver::Version;

    let Some(label_value) = label else {
        anstream::eprintln!(
            "note: image has no `{LAUNCHER_VERSION_LABEL}` label; skipping launcher version check"
        );
        return Ok(());
    };
    if label_value == "unknown" {
        anstream::eprintln!(
            "note: image's `{LAUNCHER_VERSION_LABEL}` is `unknown` (build arg not set); \
             skipping launcher version check"
        );
        return Ok(());
    }
    let Ok(required) = Version::parse(label_value) else {
        anstream::eprintln!(
            "note: image's `{LAUNCHER_VERSION_LABEL}` label `{label_value}` is not valid semver; \
             skipping launcher version check"
        );
        return Ok(());
    };
    let Ok(running) = Version::parse(launcher_version) else {
        anstream::eprintln!(
            "note: launcher version `{launcher_version}` is not valid semver; \
             skipping launcher version check"
        );
        return Ok(());
    };
    if running < required {
        anyhow::bail!(
            "launcher version mismatch: this `omnifs` CLI is {running}, but the image expects \
             ≥ {required}. The image was built from a newer source tree and may declare ports, \
             env vars, or mounts this launcher doesn't know to set. Update your launcher: \
             `cargo install --path crates/omnifs-cli --force` from the worktree, or reinstall via npm."
        );
    }
    Ok(())
}

/// Why a registry-less image absent locally is not pulled, worded per build
/// channel: only a dev binary defaults to `omnifs:dev`, so a release binary
/// hitting this path chose a local tag explicitly and must not be told it is a
/// dev build.
const fn pull_refusal_reason(channel: BuildChannel) -> &'static str {
    match channel {
        BuildChannel::Dev => {
            "this omnifs binary is a dev build; it uses the locally built runtime image \
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
    use omnifs_api::API_MAJOR;

    /// Verify that `EXPECTED_LAUNCH_PROTOCOL` matches `API_MAJOR`. Both must be
    /// updated together when the API major bumps; this test enforces that.
    #[test]
    fn expected_launch_protocol_matches_api_major() {
        let expected = format!("daemon-control-v{API_MAJOR}");
        assert_eq!(
            EXPECTED_LAUNCH_PROTOCOL, expected,
            "EXPECTED_LAUNCH_PROTOCOL must equal daemon-control-v{{API_MAJOR}}; \
             bump EXPECTED_LAUNCH_PROTOCOL in runtime.rs and the Dockerfile when API_MAJOR changes"
        );
    }

    #[test]
    fn image_compat_requires_launch_protocol_label() {
        let err = check_image_compat("0.2.1", Some("0.2.1"), None, "ghcr.io/0xff-ai/omnifs:0.2.1")
            .expect_err("missing protocol must be refused");
        let msg = format!("{err}");
        assert!(
            msg.contains(LAUNCH_PROTOCOL_LABEL),
            "msg should name missing label: {msg}"
        );
        assert!(
            msg.contains("config.toml"),
            "msg should tell users how to recover: {msg}"
        );
    }

    #[test]
    fn image_compat_rejects_wrong_launch_protocol() {
        let err = check_image_compat(
            "0.2.1",
            Some("0.2.1"),
            Some("legacy-config-dir"),
            "ghcr.io/0xff-ai/omnifs:0.2.1",
        )
        .expect_err("wrong protocol must be refused");
        let msg = format!("{err}");
        assert!(msg.contains("legacy-config-dir"));
        assert!(msg.contains(EXPECTED_LAUNCH_PROTOCOL));
    }

    #[test]
    fn launch_image_compat() {
        check_image_compat(
            "0.2.1",
            Some("0.2.1"),
            Some(EXPECTED_LAUNCH_PROTOCOL),
            "omnifs:local-dev",
        )
        .expect("matching protocol and version should pass");
        check_launcher_compat("0.2.0-dev.1", None).expect("missing label should be permissive");
    }

    #[test]
    fn launcher_version_compat() {
        for (launcher, label) in [
            ("0.2.0-dev.1", Some("unknown")),
            ("0.2.0-dev.1", Some("0.2.0-dev.1")),
            ("0.3.0", Some("0.2.0-dev.1")),
            ("0.2.0-dev.1", Some("not-semver")),
        ] {
            check_launcher_compat(launcher, label)
                .unwrap_or_else(|error| panic!("launcher={launcher} label={label:?}: {error}"));
        }
    }

    #[test]
    fn launcher_older_than_label_fails() {
        let err = check_launcher_compat("0.2.0-dev.1", Some("0.2.0-dev.2"))
            .expect_err("older launcher must be refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("0.2.0-dev.1"),
            "msg should name running version: {msg}"
        );
        assert!(
            msg.contains("0.2.0-dev.2"),
            "msg should name required version: {msg}"
        );
        assert!(
            msg.contains("cargo install") || msg.contains("npm"),
            "msg should hint at remediation: {msg}"
        );
    }

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

    #[test]
    fn container_body_binds_and_env_ordering() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(tmp.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();

        let image = ImageRef::new("ghcr.io/0xff-ai/omnifs:test").unwrap();
        let container_name = ContainerName::new("omnifs-test").unwrap();
        let binds = vec![
            format!("{}:{GUEST_HOME}", paths.config_dir.display()),
            "/extra:/extra:ro".to_string(),
        ];
        let body = Runtime::build_container_body(
            &container_name,
            &image,
            binds,
            vec!["GITHUB_TOKEN=secret".to_string()],
        );
        let host_config = body.host_config.expect("host config");
        let binds = host_config.binds.expect("binds");

        assert_eq!(
            binds[0],
            format!("{}:{GUEST_HOME}", paths.config_dir.display())
        );
        assert_eq!(
            binds.last().map(String::as_str),
            Some("/extra:/extra:ro"),
            "extra bind should be last: {binds:?}"
        );

        let env = body.env.expect("env");
        let expected_home_env = format!("{OMNIFS_HOME_ENV}={GUEST_HOME}");
        assert!(
            env.iter().any(|e| e == &expected_home_env),
            "{OMNIFS_HOME_ENV} must be set"
        );
        assert!(
            env.iter()
                .any(|e| e == &format!("{ENV_CONTAINER_NAME}=omnifs-test")),
            "{ENV_CONTAINER_NAME} must be set"
        );
        assert!(
            env.iter()
                .any(|e| e == &format!("{ENV_IMAGE}=ghcr.io/0xff-ai/omnifs:test")),
            "{ENV_IMAGE} must be set"
        );
        assert!(
            env.iter().any(|e| e == "SSH_AUTH_SOCK=/ssh-agent"),
            "SSH_AUTH_SOCK must be forwarded inside container"
        );
        assert!(
            env.iter().any(|e| e == "GITHUB_TOKEN=secret"),
            "dev env values must be forwarded inside container"
        );

        assert_eq!(body.image.as_deref(), Some("ghcr.io/0xff-ai/omnifs:test"));
    }
}
