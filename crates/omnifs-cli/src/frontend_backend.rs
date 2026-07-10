//! The frontend backend seam: how `omnifs frontend up|down|status` and
//! `omnifs shell` launch, probe, tear down, and shell into the optional FUSE
//! frontend, independent of which runtime hosts it.
//!
//! Two backends implement the seam: `DockerBackend` (this module) and
//! `KrunkitBackend` (`crate::krunkit_backend`), a libkrun microVM on macOS.
//! Both run the same `omnifs frontend run --kind fuse` binary and wire
//! protocol; only the attach transport differs (Docker: TCP via
//! `host.docker.internal`; krunkit: vsock, bridged onto a unix socket by
//! krunkit itself). Keeping the seam here, rather than letting the frontend
//! commands call bollard or krunkit directly, is what let the second backend
//! land without touching them beyond a driver-selected constructor.
//!
//! [`Driver`] is the user-facing selector (`--driver` / `[frontend]
//! driver`); [`Via`] is the on-disk record of which backend a running
//! frontend was launched with.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use omnifs_workspace::runtime_record::Via;
use serde::Deserialize;

use crate::frontend_container::{
    FrontendContainerSpec, assert_locked_down, build_frontend_container_body,
};
use crate::launch_backend::GUEST_MOUNT;
use crate::runtime::Runtime;

/// Which virtualized runtime hosts the optional FUSE frontend. Selected by
/// `--driver` (CLI flag) or `[frontend] driver` (config), defaulting to
/// docker in both.
#[derive(clap::ValueEnum, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Driver {
    #[default]
    Docker,
    Krunkit,
}

impl Driver {
    /// The [`Via`] this choice records once a frontend launches under it.
    pub(crate) const fn as_via(self) -> Via {
        match self {
            Self::Docker => Via::Docker,
            Self::Krunkit => Via::Krunkit,
        }
    }
}

/// The attach transport a [`FrontendLaunchSpec`] carries. Docker dials a TCP
/// port on the host bridge; krunkit proxies guest-initiated vsock
/// connections onto a unix socket the daemon already bound (`POST
/// /v1/frontend/attach-target/vsock`). Each backend's `launch` asserts it received
/// the variant its own transport needs; a mismatch is an internal dispatch
/// bug (the wrong daemon call was made), not a user-reachable error.
#[derive(Debug, Clone)]
pub(crate) enum AttachEndpoint {
    Tcp(u16),
    Unix(PathBuf),
}

/// Launch-time parameters for the frontend, independent of backend.
/// Identity (container/instance name, image, guest disk) lives on the
/// backend instance itself, constructed by the caller before
/// [`FrontendBackend::launch`] is invoked, so it is not duplicated here.
pub(crate) struct FrontendLaunchSpec {
    /// The workspace's config dir, recorded as a label only (never bind-mounted
    /// by Docker; read for path derivation by krunkit).
    pub home: PathBuf,
    /// The host-native daemon's attach listener, in whichever transport this
    /// backend dials.
    pub attach: AttachEndpoint,
    pub attach_token: String,
}

/// How the CLI launches, probes, tears down, and shells into the optional
/// FUSE frontend. Docker (`DockerBackend`) is the only implementation today;
/// a future libkrun/krunkit backend on macOS implements the same contract
/// over a vsock transport instead of Docker's TCP bridge.
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
        let AttachEndpoint::Tcp(attach_port) = spec.attach else {
            anyhow::bail!("internal: the docker backend requires a tcp attach endpoint");
        };
        let body = build_frontend_container_body(&FrontendContainerSpec {
            image: self.runtime.image(),
            home: &spec.home,
            attach_port,
            attach_token: &spec.attach_token,
            // Docker Desktop (macOS) resolves `host.docker.internal` on its
            // own; native Linux does not predefine the name, so the
            // container needs the extra `--add-host` mapping. A pure
            // function of the target OS, so it is computed here rather than
            // threaded through the backend-neutral spec.
            add_host_gateway: cfg!(target_os = "linux"),
        });
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
