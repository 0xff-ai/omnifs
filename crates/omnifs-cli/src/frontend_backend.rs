//! The frontend backend seam: how `omnifs frontend enable|disable` and
//! `omnifs shell` launch, probe, tear down, and shell into the optional FUSE
//! frontend, independent of which runtime hosts it.
//!
//! Two guest backends implement the seam: `DockerBackend` (this module) and
//! `KrunkitBackend` (`crate::krunkit_backend`), a libkrun microVM on macOS.
//! Both run the same `omnifs-thin fuse` runner and Omnifs VFS wire protocol; only
//! the attach transport differs (Docker: TCP via `host.docker.internal`; krunkit: vsock,
//! bridged onto a unix socket by krunkit itself). Keeping the seam here,
//! rather than letting the frontend commands call bollard or krunkit
//! directly, is what let the second backend land without touching them
//! beyond an environment-selected constructor.

use std::process::Command;

use crate::frontend_container::{FrontendContainerSpec, assert_locked_down};
use crate::launch_backend::GUEST_MOUNT;
use crate::runtime::Runtime;
use anyhow::Result;

/// How the CLI launches, probes, tears down, and shells into the optional
/// FUSE frontend. Docker uses the host bridge; krunkit implements the same
/// contract over a vsock transport.
pub(crate) trait FrontendBackend {
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

    pub(crate) async fn launch(
        &self,
        home: &std::path::Path,
        attach_port: u16,
        attach_token: &str,
    ) -> Result<()> {
        let body = FrontendContainerSpec {
            image: self.runtime.image(),
            home,
            attach_port,
            attach_token,
            // Docker Desktop (macOS) resolves `host.docker.internal` on its
            // own; native Linux does not predefine the name, so the
            // container needs the extra `--add-host` mapping.
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
}

impl FrontendBackend for DockerBackend {
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
