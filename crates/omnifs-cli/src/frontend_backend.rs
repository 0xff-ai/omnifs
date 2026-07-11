//! The frontend backend seam: how `omnifs frontend up|down|status` and
//! `omnifs shell` launch, probe, tear down, and shell into the optional FUSE
//! frontend, independent of which runtime hosts it.
//!
//! Two guest backends implement the seam: `DockerBackend` (this module) and
//! `KrunkitBackend` (`crate::krunkit_backend`), a libkrun microVM on macOS.
//! Both run the same `omnifs-fuse` binary and Omnifs VFS wire protocol; only
//! the attach transport differs (Docker: TCP via `host.docker.internal`; krunkit: vsock,
//! bridged onto a unix socket by krunkit itself). Keeping the seam here,
//! rather than letting the frontend commands call bollard or krunkit
//! directly, is what let the second backend land without touching them
//! beyond a driver-selected constructor.
//!
//! [`Driver`] is the user-facing selector (`--driver` / `[frontend]
//! driver`); [`Via`] is the on-disk record of which backend a running
//! frontend was launched with.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use omnifs_workspace::runtime_record::Via;
use serde::Deserialize;

use crate::frontend_container::{FrontendContainerSpec, assert_locked_down};
use crate::launch_backend::GUEST_MOUNT;
use crate::runtime::Runtime;

/// How the frontend process is delivered. Selected by `--driver` (CLI flag)
/// or `[frontend] driver` (config).
#[derive(clap::ValueEnum, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Driver {
    Local,
    #[default]
    Docker,
    Krunkit,
}

impl Driver {
    /// The [`Via`] this choice records once a frontend launches under it.
    pub(crate) const fn as_via(self) -> Via {
        match self {
            Self::Local => Via::Local,
            Self::Docker => Via::Docker,
            Self::Krunkit => Via::Krunkit,
        }
    }
}

/// Backend-specific launch inputs. Keeping the variants whole prevents
/// impossible combinations such as a Docker launch with a Unix attach socket
/// or a krunkit launch without a guest disk image.
pub(crate) enum FrontendLaunchSpec {
    Docker {
        /// Recorded as a label only; it is never bind-mounted.
        home: PathBuf,
        attach_port: u16,
        attach_token: String,
    },
    Krunkit {
        /// Daemon-owned socket onto which krunkit proxies guest vsock traffic.
        attach_socket: PathBuf,
        attach_token: String,
        guest_image: PathBuf,
    },
}

/// How the CLI launches, probes, tears down, and shells into the optional
/// FUSE frontend. Docker uses the host bridge; krunkit implements the same
/// contract over a vsock transport.
pub(crate) trait FrontendBackend {
    /// Ensure the runnable artifact is present, replace any existing
    /// frontend of this backend's identity, start it, and verify the
    /// fail-closed lockdown contract before returning. A backend must never
    /// report success without having verified its own confinement.
    async fn launch(&self, spec: &FrontendLaunchSpec) -> Result<()>;

    /// Whether `path` is visible inside the running frontend. Used to poll
    /// for the FUSE mount coming up after launch.
    async fn mount_ready(&self, path: &str) -> Result<bool>;

    /// `Some(running)` when this backend's frontend exists, `None` when it
    /// does not.
    async fn is_running(&self) -> Result<Option<bool>>;

    /// Remove the frontend, if one exists.
    async fn tear_down(&self) -> Result<()>;

    /// Build the command that execs a shell (or a one-shot command) inside
    /// the running frontend, at the guest mount. Pure command construction:
    /// no I/O, so it never needs the frontend to be reachable.
    fn shell_command(&self, shell_override: Option<&str>, trailing: &[String]) -> Command;
}

/// The Docker-hosted FUSE frontend, delegating to [`Runtime`] (the bollard
/// client, already connected and scoped to one container identity by the
/// caller) and `frontend_container.rs` (container shape and the lockdown
/// assertion).
pub(crate) struct DockerBackend {
    runtime: Runtime,
}

impl DockerBackend {
    pub(crate) fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }
}

impl FrontendBackend for DockerBackend {
    async fn launch(&self, spec: &FrontendLaunchSpec) -> Result<()> {
        let FrontendLaunchSpec::Docker {
            home,
            attach_port,
            attach_token,
        } = spec
        else {
            anyhow::bail!("internal: the docker backend received a krunkit launch spec");
        };
        let body = FrontendContainerSpec {
            image: self.runtime.image(),
            home,
            attach_port: *attach_port,
            attach_token,
            // Docker Desktop (macOS) resolves `host.docker.internal` on its
            // own; native Linux does not predefine the name, so the
            // container needs the extra `--add-host` mapping. A pure
            // function of the target OS, so it is computed here rather than
            // threaded through the backend-neutral spec.
            add_host_gateway: cfg!(target_os = "linux"),
        }
        .build_body();
        self.runtime.launch_frontend_container(body).await?;

        let (mounts, env) = self.runtime.inspect_mounts_and_env().await?;
        if let Err(violation) = assert_locked_down(&mounts, &env) {
            let _ = self.runtime.remove().await;
            anyhow::bail!("refusing to run the frontend container: {violation}");
        }
        Ok(())
    }

    async fn mount_ready(&self, path: &str) -> Result<bool> {
        self.runtime.exec_path_exists(path).await
    }

    async fn is_running(&self) -> Result<Option<bool>> {
        self.runtime
            .container_running(self.runtime.container_name())
            .await
    }

    async fn tear_down(&self) -> Result<()> {
        self.runtime.remove().await
    }

    fn shell_command(&self, shell_override: Option<&str>, trailing: &[String]) -> Command {
        use std::io::IsTerminal as _;

        let mut cmd = Command::new("docker");
        cmd.arg("exec").arg("-i");
        if std::io::stdin().is_terminal() {
            cmd.arg("-t");
        }
        cmd.arg("-w").arg(GUEST_MOUNT);
        cmd.arg(self.runtime.container_name().as_str());
        if trailing.is_empty() {
            cmd.arg(shell_override.unwrap_or("/bin/sh"));
        } else {
            cmd.args(trailing);
        }
        cmd
    }
}
