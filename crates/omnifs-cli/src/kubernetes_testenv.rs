//! Throwaway Kubernetes cluster for `omnifs dev`.
//!
//! Brings up the k3s + `kubectl proxy` compose stack under
//! `providers/kubernetes/testenv/`. The proxy re-exposes the API server as a
//! plain-HTTP unix socket on a shared host directory; the omnifs container
//! bind-mounts that directory and the kubernetes provider reads the socket
//! through its default `unix://` endpoint. omnifs holds no cluster credential
//! and needs no egress capability.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::session::MountConfig;

/// Compose project name, so teardown can find the stack without the file.
const PROJECT: &str = "omnifs-devcluster";
/// Where the socket directory is mounted inside the omnifs container; matches
/// the kubernetes dev mount's `unix:///run/omnifs/k8s.sock` endpoint.
const GUEST_SOCK_DIR: &str = "/run/omnifs";

fn testenv_dir(workspace: &Path) -> PathBuf {
    workspace.join("providers/kubernetes/testenv")
}

fn compose_file(workspace: &Path) -> PathBuf {
    testenv_dir(workspace).join("compose.yaml")
}

/// A `docker compose` command scoped to the testenv project and compose file.
/// Callers append the subcommand (`up`/`down`) and any env.
fn compose_cmd(compose: &Path) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["compose", "-p", PROJECT, "-f"]).arg(compose);
    cmd
}

/// Bring up the k3s + proxy stack and wait until it is healthy. Returns the
/// socket bind to layer onto the omnifs container and the kubernetes mount to
/// inject into the daemon.
pub(crate) fn up(workspace: &Path, sock_dir: &Path) -> anyhow::Result<(String, MountConfig)> {
    let compose = compose_file(workspace);
    if !compose.is_file() {
        bail!(
            "dev cluster compose file not found at {}",
            compose.display()
        );
    }
    std::fs::create_dir_all(sock_dir)
        .with_context(|| format!("create socket dir {}", sock_dir.display()))?;

    anstream::println!("Starting dev Kubernetes cluster (k3s); first run pulls the image");
    let status = compose_cmd(&compose)
        .args(["up", "-d", "--wait"])
        .env("OMNIFS_K8S_SOCK_DIR", sock_dir)
        .status()
        .context("invoke docker compose up")?;
    if !status.success() {
        bail!("docker compose up failed for the dev cluster");
    }
    anstream::println!("✓ Dev cluster ready");

    let mount = MountConfig::from_path(&testenv_dir(workspace).join("dev-mount.json"))?;
    Ok((format!("{}:{GUEST_SOCK_DIR}", sock_dir.display()), mount))
}

/// Tear down the dev cluster stack. Best-effort and idempotent: a no-op when
/// the workspace has no compose file or nothing is running.
///
/// Called from `omnifs dev` on failure and, workspace-gated, from `omnifs down`
/// so a contributor's full stop also tears down the dev cluster. Outside a
/// workspace checkout there is no cluster, so production `down` never reaches
/// the compose command.
pub(crate) fn down(workspace: &Path) -> anyhow::Result<()> {
    let compose = compose_file(workspace);
    if !compose.is_file() {
        return Ok(());
    }
    let status = compose_cmd(&compose)
        .args(["down", "-v"])
        // compose.yaml requires OMNIFS_K8S_SOCK_DIR to interpolate the proxy's
        // socket bind even on teardown, where the value is unused.
        .env("OMNIFS_K8S_SOCK_DIR", GUEST_SOCK_DIR)
        .status()
        .context("invoke docker compose down")?;
    if !status.success() {
        anstream::eprintln!("note: `docker compose down` for the dev cluster returned non-zero");
    }
    Ok(())
}
