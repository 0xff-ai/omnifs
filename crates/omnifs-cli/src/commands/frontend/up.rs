//! `omnifs frontend up`: bring up the Docker-hosted FUSE frontend.
//!
//! The frontend is a separate, credential-free container attached to a
//! host-native daemon's shared namespace over TCP; it is not a daemon runtime
//! mode.

// On Linux the expected bind address comes from the Docker bridge probe, so
// `Ipv4Addr` is only named on the non-Linux (loopback) arm.
#[cfg(not(target_os = "linux"))]
use std::net::Ipv4Addr;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context as _, ensure};
use clap::Args;
use omnifs_workspace::layout::OMNIFS_HOME_ENV;
use omnifs_workspace::runtime_record::{FrontendKind, FrontendRecord, RuntimeRecord, Via};

use crate::frontend_backend::{DockerBackend, FrontendBackend, FrontendLaunchSpec};
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::launch::Launcher;
use crate::launch_backend::{DockerTarget, GUEST_MOUNT};
use crate::runtime::Runtime;
use crate::workspace::Workspace;

/// How long to wait for the mount to appear inside the frontend container
/// before giving up (the container itself starts in well under this window;
/// a longer wait would just mask a real startup failure).
const MOUNT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MOUNT_PROBE_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendUpArgs {}

impl FrontendUpArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let paths = workspace.layout().clone();

        ensure_native_daemon(&workspace).await?;

        let config = workspace.config()?;
        let image = resolve_frontend_image(None, &config)?;
        let is_default_home = std::env::var_os(OMNIFS_HOME_ENV).is_none();
        let container_name = frontend_container_name(&paths.config_dir, is_default_home)?;
        let target = DockerTarget::new(
            container_name.as_str().to_string(),
            image.as_str().to_string(),
        )?;

        let runtime = Runtime::connect_ready(&target, "omnifs frontend up").await?;
        #[cfg(target_os = "linux")]
        let (bind_ip, expected_bind_ip) = {
            let bind_ip = runtime.frontend_attach_bind_ip().await?;
            (Some(bind_ip), bind_ip)
        };
        #[cfg(not(target_os = "linux"))]
        let (bind_ip, expected_bind_ip) = (None, Ipv4Addr::LOCALHOST);

        anstream::eprintln!("Requesting the daemon's TCP namespace attach target");
        let attach = workspace.daemon().frontend_attach_target(bind_ip).await?;
        let attach_addr = attach_addr(&attach.addr)?;
        ensure!(
            attach_addr.ip() == IpAddr::V4(expected_bind_ip),
            "daemon already serves its attach listener on {}; restart it with `omnifs down`, then re-run `omnifs frontend up`",
            attach_addr.ip()
        );
        let attach_port = attach_addr.port();

        let backend = DockerBackend::new(runtime);
        let spec = FrontendLaunchSpec {
            home: paths.config_dir.clone(),
            attach_port,
            attach_token: attach.token.clone(),
        };
        backend.launch(&spec).await?;

        let mount_name = first_mount_name(&workspace)?;
        wait_for_mount(&backend, &mount_name).await?;

        record_frontend_via_docker(&paths.runtime_record_file());

        anstream::eprintln!(
            "✓ {GUEST_MOUNT} is mounted inside `{}`",
            target.container_name()
        );
        anstream::eprintln!();
        anstream::eprintln!(
            "Run `{}` to open a shell inside the container and browse {GUEST_MOUNT}.",
            crate::style::bold("omnifs shell")
        );
        Ok(())
    }
}

/// Ensure the host-native daemon is serving, reusing the same launch
/// machinery `omnifs up` uses. A no-op when one is already running (the
/// daemon only ever runs host-native, so any running daemon qualifies).
async fn ensure_native_daemon(workspace: &Workspace) -> anyhow::Result<()> {
    if workspace.daemon().status_optional().await?.is_some() {
        return Ok(());
    }

    let launcher = Launcher::new(workspace, "omnifs frontend up");
    launcher.launch().await?;
    Ok(())
}

/// Parse the attach listener address returned by the daemon. The container
/// reaches the same port through `host.docker.internal` on every platform.
fn attach_addr(addr: &str) -> anyhow::Result<SocketAddr> {
    addr.parse()
        .with_context(|| format!("attach listener address `{addr}` is invalid"))
}

fn first_mount_name(workspace: &Workspace) -> anyhow::Result<String> {
    workspace
        .mounts()?
        .into_iter()
        .map(|mount| mount.name.to_string())
        .next()
        .context("no mounts configured; run `omnifs init <provider>` before `omnifs frontend up`")
}

/// Poll `docker exec test -e /omnifs/<mount>` until it succeeds or the
/// timeout elapses.
async fn wait_for_mount(backend: &impl FrontendBackend, mount_name: &str) -> anyhow::Result<()> {
    let probe_path = format!("{GUEST_MOUNT}/{mount_name}");
    anstream::eprintln!("Waiting for {probe_path} inside the frontend container");
    let deadline = tokio::time::Instant::now() + MOUNT_PROBE_TIMEOUT;
    loop {
        if backend.mount_ready(&probe_path).await.unwrap_or(false) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "{probe_path} did not appear inside the frontend container within {}s",
                MOUNT_PROBE_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(MOUNT_PROBE_INTERVAL).await;
    }
}

/// Append (or replace) the Docker-hosted frontend's entry in the host-native
/// daemon's on-disk runtime record with a read-modify-write, mirroring how
/// the daemon itself patches in its TCP attach binding
/// (`Daemon::persist_attach_record`). Best-effort: a failure here does not
/// unwind the already-running frontend, since the daemon owns the record's
/// lifecycle and will rewrite it wholesale on its next restart anyway.
fn record_frontend_via_docker(record_path: &std::path::Path) {
    let patched = RuntimeRecord::update(record_path, |record| {
        record
            .frontends
            .retain(|frontend| frontend.via != Some(Via::Docker));
        record.frontends.push(FrontendRecord {
            kind: FrontendKind::Fuse,
            mount_point: std::path::PathBuf::from(GUEST_MOUNT),
            via: Some(Via::Docker),
        });
    });
    match patched {
        Ok(true) => {},
        Ok(false) => anstream::eprintln!(
            "warning: runtime record missing; cannot record the frontend container"
        ),
        Err(error) => {
            anstream::eprintln!("warning: could not persist the frontend container: {error:#}");
        },
    }
}
