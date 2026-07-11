//! `omnifs frontend up`: bring up the optional virtualized FUSE frontend,
//! either the Docker-hosted container (`--driver docker`, the default) or the
//! krunkit microVM (`--driver krunkit`, macOS only).
//!
//! The frontend is a separate, credential-free surface attached to a
//! host-native daemon's shared namespace over its attach transport (TCP for
//! Docker, vsock for krunkit); it is not a daemon runtime mode.

// On Linux the expected bind address comes from the Docker bridge probe, so
// `Ipv4Addr` is only named on the non-Linux (loopback) arm.
#[cfg(not(target_os = "linux"))]
use std::net::Ipv4Addr;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context as _, ensure};
use clap::Args;
use omnifs_workspace::runtime_record::{FrontendKind, FrontendRecord, RuntimeRecord, Via};

use crate::frontend_backend::{DockerBackend, Driver, FrontendBackend, FrontendLaunchSpec};
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::krunkit_backend::{GuestImageSource, KrunkitBackend};
use crate::launch::Launcher;
use crate::launch_backend::{DockerTarget, GUEST_MOUNT};
use crate::runtime::Runtime;
use crate::workspace::Workspace;

/// How long to wait for the mount to appear inside the Docker-hosted frontend
/// container before giving up (the container itself starts in well under
/// this window; a longer wait would just mask a real startup failure).
const DOCKER_MOUNT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// A krunkit microVM boots a kernel and reaches multi-user systemd before its
/// frontend runner can even attach, which takes far longer than a container
/// start; the readiness beacon (`crates/omnifs-vfs-wire/src/beacon.rs`,
/// spawned by `omnifs-fuse`) only fires once that whole chain has completed
/// and the FUSE mount is serving.
const KRUNKIT_MOUNT_PROBE_TIMEOUT: Duration = Duration::from_secs(90);
const MOUNT_PROBE_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendUpArgs {
    /// Which virtualized runtime hosts the frontend. Defaults to the
    /// `[frontend] driver` config value, or docker if that is unset too.
    #[arg(long, value_enum)]
    pub driver: Option<Driver>,
}

impl FrontendUpArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        self.run_in(&workspace).await
    }

    /// Start a frontend in an already-resolved command workspace. `omnifs up`
    /// uses this path so the frontend launch shares its mount registry and
    /// daemon client instead of constructing a second command context midway
    /// through the lifecycle operation.
    pub(crate) async fn run_in(self, workspace: &Workspace) -> anyhow::Result<()> {
        let paths = workspace.layout().clone();
        let config = workspace.config()?;

        let driver = self.driver.unwrap_or(config.frontend.driver);
        ensure_native_daemon(workspace).await?;

        let mount_name = first_mount_name(workspace)?;

        match driver {
            Driver::Docker => run_docker(workspace, &paths, &config, &mount_name).await,
            Driver::Krunkit => run_krunkit(workspace, &paths, &config, &mount_name).await,
        }
    }
}

async fn run_docker(
    workspace: &Workspace,
    paths: &omnifs_workspace::layout::WorkspaceLayout,
    config: &crate::config::Config,
    mount_name: &str,
) -> anyhow::Result<()> {
    let image = resolve_frontend_image(None, config)?;
    let container_name = frontend_container_name(paths)?;
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

    let backend = DockerBackend::new(runtime);
    let spec = FrontendLaunchSpec::Docker {
        home: paths.config_dir.clone(),
        attach_port: attach_addr.port(),
        attach_token: attach.token.clone(),
    };
    backend.launch(&spec).await?;

    wait_for_mount(&backend, mount_name, DOCKER_MOUNT_PROBE_TIMEOUT).await?;
    record_frontend(&paths.runtime_record_file(), Driver::Docker.as_via());

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

async fn run_krunkit(
    workspace: &Workspace,
    paths: &omnifs_workspace::layout::WorkspaceLayout,
    config: &crate::config::Config,
    mount_name: &str,
) -> anyhow::Result<()> {
    let guest_image = GuestImageSource::resolve(None, config)?
        .into_local_path(&paths.cache_dir)
        .await?;

    anstream::eprintln!("Requesting the daemon's vsock namespace attach listener");
    let attach = workspace.daemon().frontend_attach_target_vsock().await?;

    let backend = KrunkitBackend::new(paths.config_dir.clone());
    let spec = FrontendLaunchSpec::Krunkit {
        attach_socket: std::path::PathBuf::from(attach.socket_path),
        attach_token: attach.token.clone(),
        guest_image,
    };
    anstream::eprintln!("Starting the krunkit guest");
    backend.launch(&spec).await?;

    wait_for_mount(&backend, mount_name, KRUNKIT_MOUNT_PROBE_TIMEOUT).await?;
    record_frontend(&paths.runtime_record_file(), Driver::Krunkit.as_via());

    anstream::eprintln!("✓ {GUEST_MOUNT} is mounted inside the krunkit guest");
    anstream::eprintln!();
    anstream::eprintln!(
        "Run `{}` to open a shell inside the guest and browse {GUEST_MOUNT}.",
        crate::style::bold("omnifs shell")
    );
    Ok(())
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

/// Poll `backend.mount_ready` until it reports the mount is live or
/// `timeout` elapses. Docker polls a specific path inside the container;
/// krunkit has no equivalent exec channel and instead observes the guest's
/// whole-VM readiness beacon, ignoring the path (see
/// `KrunkitBackend::mount_ready`).
async fn wait_for_mount(
    backend: &impl FrontendBackend,
    mount_name: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let probe_path = format!("{GUEST_MOUNT}/{mount_name}");
    anstream::eprintln!("Waiting for {probe_path} inside the frontend");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if backend.mount_ready(&probe_path).await.unwrap_or(false) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "{probe_path} did not appear inside the frontend within {}s",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(MOUNT_PROBE_INTERVAL).await;
    }
}

/// Append (or replace) the virtualized frontend's entry in the host-native
/// daemon's on-disk runtime record with a read-modify-write, mirroring how
/// the daemon itself patches in its TCP attach binding
/// (`Daemon::persist_attach_record`). Best-effort: a failure here does not
/// unwind the already-running frontend, since the daemon owns the record's
/// lifecycle and will rewrite it wholesale on its next restart anyway.
///
/// Drops any prior virtualized frontend entry regardless of which backend it
/// was delivered by: at most one virtualized frontend is recorded at a time.
fn record_frontend(record_path: &std::path::Path, via: Via) {
    let patched = RuntimeRecord::update(record_path, |record| {
        record.frontends.retain(|frontend| frontend.via.is_none());
        record.frontends.push(FrontendRecord {
            kind: FrontendKind::Fuse,
            mount_point: std::path::PathBuf::from(GUEST_MOUNT),
            via: Some(via),
        });
    });
    match patched {
        Ok(true) => {},
        Ok(false) => {
            anstream::eprintln!("warning: runtime record missing; cannot record the frontend");
        },
        Err(error) => {
            anstream::eprintln!("warning: could not persist the frontend: {error:#}");
        },
    }
}
