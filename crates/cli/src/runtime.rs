use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, DeviceMapping, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, RemoveContainerOptions,
    StartContainerOptions, StopContainerOptions,
};
use futures_util::{StreamExt, TryStreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::container_name::ContainerName;
use crate::error::WithHint;
use crate::image_ref::ImageRef;
use crate::runtime_target::RuntimeTarget;
use crate::session::{CONTAINER_NAME, HOST_CRED_DIR, HOST_FUSE_MOUNT, IMAGE, MountConfig, Session};

const HOST_MOUNTS_DIR: &str = "/root/.omnifs/mounts";
const HOST_CREDENTIALS_FILE: &str = "/root/.omnifs/credentials.json";

/// Extra bind mounts on top of the canonical session wiring.
/// `omnifs dev` uses this for the GitHub token secret file and DB fixture;
/// `omnifs up` leaves it empty.
#[derive(Debug, Default)]
pub(crate) struct ContainerExtras {
    pub(crate) binds: Vec<String>,
    pub(crate) env: Vec<String>,
}

pub(crate) struct Runtime {
    docker: Docker,
    container_name: ContainerName,
    image: ImageRef,
}

impl Runtime {
    /// Connect to the Docker daemon without binding container/image targets.
    /// Teardown paths pass an explicit [`ContainerName`] to [`Self::remove_existing`].
    pub(crate) fn connect_docker() -> Result<Self> {
        Ok(Self {
            docker: connect_docker_client()?,
            container_name: ContainerName::new(CONTAINER_NAME)?,
            image: ImageRef::new(IMAGE)?,
        })
    }

    pub(crate) fn connect_for(target: &RuntimeTarget) -> Result<Self> {
        Ok(Self {
            docker: connect_docker_client()?,
            container_name: target.container_name().clone(),
            image: target.image().clone(),
        })
    }

    pub(crate) async fn connect_ready(
        target: &RuntimeTarget,
        command: &'static str,
    ) -> Result<Self> {
        anstream::println!("Connecting to Docker");
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

    pub(crate) async fn pull_image_with_progress(&self, image: &str) -> Result<()> {
        anstream::println!("Pulling {image}");
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
                anstream::println!("  {status}");
            }
        }
        multi.clear().ok();
        anstream::println!("✓ Image ready");
        Ok(())
    }

    pub(crate) async fn launch_container(
        &self,
        session: &Session,
        extras: ContainerExtras,
    ) -> Result<()> {
        self.ensure_image().await?;
        self.remove().await?;

        anstream::println!(
            "Creating container `{}` from image `{}`",
            self.container_name,
            self.image
        );
        let create = self.build_container_body(session, extras);
        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(self.container_name.as_str().to_string()),
                    ..Default::default()
                }),
                create,
            )
            .await
            .with_context(|| format!("create container `{}`", self.container_name))?;
        anstream::println!("Starting container `{}`", self.container_name);
        self.docker
            .start_container(self.container_name.as_str(), None::<StartContainerOptions>)
            .await
            .with_context(|| format!("start container `{}`", self.container_name))?;
        Ok(())
    }

    pub(crate) async fn remove(&self) -> Result<()> {
        self.remove_existing(&self.container_name).await
    }

    pub(crate) async fn remove_existing(&self, name: &ContainerName) -> Result<()> {
        anstream::print!("Checking for existing container `{name}` ");
        std::io::stdout().flush().ok();
        match self
            .docker
            .inspect_container(name.as_str(), None::<InspectContainerOptions>)
            .await
        {
            Ok(_) => {
                anstream::println!("found");
                // Best-effort stop, then remove. Bollard returns errors for
                // already-stopped containers; we don't care about that case.
                anstream::println!("Stopping existing container `{name}` (1s timeout)");
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
                anstream::println!("Removing existing container `{name}`");
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
            }) => anstream::println!("none"),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect container `{name}`"));
            },
        }
        Ok(())
    }

    pub(crate) async fn wait_for_fuse_mount(&self) -> Result<()> {
        use bollard::exec::{CreateExecOptions, StartExecResults};

        anstream::println!(
            "Waiting for {HOST_FUSE_MOUNT} inside `{}`",
            self.container_name
        );
        for attempt in 0..60 {
            let exec = self
                .docker
                .create_exec(
                    self.container_name.as_str(),
                    CreateExecOptions::<&str> {
                        cmd: Some(vec![
                            "sh",
                            "-lc",
                            &format!("grep -qs ' {HOST_FUSE_MOUNT} ' /proc/mounts"),
                        ]),
                        attach_stdout: Some(true),
                        attach_stderr: Some(true),
                        ..Default::default()
                    },
                )
                .await;
            if let Ok(exec) = exec
                && let Ok(StartExecResults::Attached { mut output, .. }) =
                    self.docker.start_exec(&exec.id, None).await
            {
                // Drain output so the exec finishes before we inspect its exit code.
                while output.next().await.is_some() {}
                if let Ok(info) = self.docker.inspect_exec(&exec.id).await
                    && info.exit_code == Some(0)
                {
                    anstream::println!("✓ FUSE mount is ready");
                    return Ok(());
                }
            }
            if attempt > 0 && attempt % 5 == 0 {
                anstream::print!(".");
                std::io::stdout().flush().ok();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        anstream::println!();
        Err(anyhow::anyhow!(
            "{HOST_FUSE_MOUNT} did not become available inside `{}` within 60s",
            self.container_name
        ))
        .with_hint(format!(
            "`docker logs {}` may show why the daemon failed to mount",
            self.container_name
        ))
        .with_hint("Run `omnifs doctor` to verify Docker, FUSE, and image cache")
    }

    pub(crate) async fn verify_status(&self, configs: &[MountConfig]) -> Result<()> {
        use bollard::container::LogOutput;
        use bollard::exec::{CreateExecOptions, StartExecResults};

        anstream::println!("Checking runtime provider status");
        let exec = self
            .docker
            .create_exec(
                self.container_name.as_str(),
                CreateExecOptions::<&str> {
                    cmd: Some(vec!["omnifs", "status", "--json"]),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("create exec in `{}`", self.container_name))?;

        let StartExecResults::Attached { mut output, .. } = self
            .docker
            .start_exec(&exec.id, None)
            .await
            .with_context(|| format!("start exec in `{}`", self.container_name))?
        else {
            anyhow::bail!("expected attached exec output");
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(msg) = output.next().await {
            match msg.context("read exec output")? {
                LogOutput::StdOut { message } => stdout.extend_from_slice(&message),
                LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
                _ => {},
            }
        }

        let info = self
            .docker
            .inspect_exec(&exec.id)
            .await
            .context("inspect exec")?;
        let exit_code = info.exit_code.unwrap_or(-1);

        if exit_code != 0 {
            anyhow::bail!(
                "`omnifs status --json` inside `{container_name}` exited with {}:\n{}{}",
                exit_code,
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr),
                container_name = self.container_name
            );
        }
        let payload: crate::status::StatusJson =
            serde_json::from_slice(&stdout).with_context(|| {
                format!(
                    "parse `omnifs status --json` output from `{}`:\n{}",
                    self.container_name,
                    String::from_utf8_lossy(&stdout)
                )
            })?;
        let missing = configs
            .iter()
            .filter(|cfg| {
                !payload.providers.iter().any(|p| match p {
                    crate::status::ProviderStatusJson::Ready {
                        mount,
                        provider_present,
                        ..
                    } => mount == cfg.name.as_str() && *provider_present,
                    crate::status::ProviderStatusJson::Invalid { .. } => false,
                })
            })
            .map(|cfg| cfg.name.to_string())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            anyhow::bail!(
                "container started, but provider status is not ready (missing/unready: {}):\n{}",
                missing.join(", "),
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        let invalid: Vec<_> = payload
            .providers
            .iter()
            .filter_map(|p| match p {
                crate::status::ProviderStatusJson::Invalid { config_path, .. } => {
                    Some(config_path.display().to_string())
                },
                crate::status::ProviderStatusJson::Ready { .. } => None,
            })
            .collect();
        if !invalid.is_empty() {
            anyhow::bail!(
                "container started, but {} provider config(s) are invalid: {}",
                invalid.len(),
                invalid.join(", ")
            );
        }
        anstream::println!("✓ Runtime sees {} provider(s)", configs.len());
        Ok(())
    }

    async fn ensure_image(&self) -> Result<()> {
        anstream::print!("Checking image `{}` ", self.image);
        std::io::stdout().flush().ok();
        match self.docker.inspect_image(self.image.as_str()).await {
            Ok(_) => {
                anstream::println!("present");
                Ok(())
            },
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                anstream::println!("missing");
                self.pull_image_with_progress(self.image.as_str()).await
            },
            Err(error) => Err(error).with_context(|| format!("inspect image `{}`", self.image)),
        }
    }

    fn build_container_body(
        &self,
        session: &Session,
        extras: ContainerExtras,
    ) -> ContainerCreateBody {
        let mut binds = vec![
            format!("{}:{HOST_CRED_DIR}:ro", session.creds_dir().display()),
            format!("{}:{HOST_MOUNTS_DIR}:ro", session.mounts_dir().display()),
        ];
        if session.credentials_file().exists() {
            binds.push(format!(
                "{}:{HOST_CREDENTIALS_FILE}",
                session.credentials_file().display()
            ));
        }

        // SSH_AUTH_SOCK bind enables git callouts. Skip if unset; only providers
        // that perform git operations will notice (and they'll error clearly).
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

        // Docker socket bind powers the in-container docker provider. Optional.
        let docker_sock = PathBuf::from("/var/run/docker.sock");
        if docker_sock.exists() {
            binds.push("/var/run/docker.sock:/var/run/docker.sock:ro".to_string());
        }

        binds.extend(extras.binds);

        let host_config = HostConfig {
            binds: Some(binds),
            devices: Some(vec![DeviceMapping {
                path_on_host: Some("/dev/fuse".to_string()),
                path_in_container: Some("/dev/fuse".to_string()),
                cgroup_permissions: Some("rwm".to_string()),
            }]),
            cap_add: Some(vec!["SYS_ADMIN".to_string()]),
            security_opt: Some(vec!["apparmor:unconfined".to_string()]),
            ..Default::default()
        };

        let mut env = vec![
            "SSH_AUTH_SOCK=/ssh-agent".to_string(),
            "GIT_SSH_COMMAND=ssh -F /dev/null -o StrictHostKeyChecking=accept-new".to_string(),
        ];
        env.extend(extras.env);

        ContainerCreateBody {
            image: Some(self.image.as_str().to_string()),
            env: Some(env),
            host_config: Some(host_config),
            ..Default::default()
        }
    }
}

fn connect_docker_client() -> Result<Docker> {
    Docker::connect_with_local_defaults().context("connect to Docker daemon (is it running?)")
}
