#![allow(clippy::disallowed_macros)] // migrates in wave 2 (cli-redesign)
//! `omnifs frontend up`: start a local, Docker, or krunkit frontend process.
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

use std::path::Path;

use anyhow::{Context as _, ensure};
use clap::Args;

use crate::config::{EffectiveFrontend, HostOs, Provenance, resolve_frontends};
use crate::frontend_backend::{DockerBackend, Driver, FrontendBackend, FrontendLaunchSpec};
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::krunkit_backend::{GuestImageSource, KrunkitBackend};
use crate::launch::Launcher;
use crate::launch_backend::{DockerTarget, GUEST_MOUNT};
#[cfg(feature = "daemon")]
use crate::local_backend::{LocalBackend, LocalProtocol};
use crate::runtime::Runtime;
use crate::ui::LiveRow;
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
    /// How to deliver the frontend. Defaults to the first entry in the
    /// effective `[[frontends]]` plan (explicit config, else the platform
    /// default). When given and no plan entry uses this driver, an ad-hoc
    /// default is constructed for it (local: platform-default kind at the
    /// default mount point; docker/krunkit: fuse).
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
        ensure_native_daemon(workspace).await?;
        let mount_name = first_mount_name(workspace)?;

        let config = workspace.config()?;
        let default_mount_point = omnifs_workspace::layout::resolve_mount_point()
            .context("cannot resolve the default mount point: set HOME or OMNIFS_MOUNT_POINT")?;
        let plan = resolve_frontends(&config.frontends, HostOs::current(), &default_mount_point)?;
        let entry = self.select_entry(&plan, &default_mount_point);

        launch_entry(workspace, &entry, &mount_name).await
    }

    fn select_entry(
        &self,
        plan: &[EffectiveFrontend],
        default_mount_point: &Path,
    ) -> EffectiveFrontend {
        match self.driver {
            Some(driver) => plan
                .iter()
                .find(|entry| entry.driver == driver)
                .cloned()
                .unwrap_or_else(|| ad_hoc_default(driver, default_mount_point)),
            None => plan
                .first()
                .cloned()
                .expect("resolve_frontends always returns a non-empty plan"),
        }
    }
}

/// Launch one resolved frontend entry. Shared by `omnifs frontend up` (one
/// entry, selected by `--driver` or plan order) and `omnifs up` (every entry
/// in the plan).
pub(crate) async fn launch_entry(
    workspace: &Workspace,
    entry: &EffectiveFrontend,
    mount_name: &str,
) -> anyhow::Result<()> {
    let paths = workspace.layout().clone();
    let config = workspace.config()?;
    match entry.driver {
        #[cfg(feature = "daemon")]
        Driver::Local => {
            let mount_point = entry
                .mount_point
                .clone()
                .context("a local frontend entry always resolves a mount point")?;
            run_local(&paths, mount_name, entry.kind, mount_point).await
        },
        #[cfg(not(feature = "daemon"))]
        Driver::Local => anyhow::bail!("the local frontend requires the daemon feature"),
        Driver::Docker => run_docker(workspace, &paths, &config, mount_name).await,
        Driver::Krunkit => run_krunkit(workspace, &paths, &config, mount_name).await,
    }
}

/// An ad-hoc default entry for `--driver X` when the effective plan has no
/// entry using that driver. `LocalProtocol::platform_default()` stays alive
/// for this path (see `local_backend.rs`); without the `daemon` feature the
/// local runner does not exist at all, so the kind is derived directly from
/// the host OS instead.
#[cfg(feature = "daemon")]
fn ad_hoc_default(driver: Driver, default_mount_point: &Path) -> EffectiveFrontend {
    default_entry(
        driver,
        LocalProtocol::platform_default().kind(),
        default_mount_point,
    )
}

#[cfg(not(feature = "daemon"))]
fn ad_hoc_default(driver: Driver, default_mount_point: &Path) -> EffectiveFrontend {
    let kind = if matches!(HostOs::current(), HostOs::Linux) {
        omnifs_workspace::runtime_record::FrontendKind::Fuse
    } else {
        omnifs_workspace::runtime_record::FrontendKind::Nfs
    };
    default_entry(driver, kind, default_mount_point)
}

fn default_entry(
    driver: Driver,
    local_kind: omnifs_workspace::runtime_record::FrontendKind,
    default_mount_point: &Path,
) -> EffectiveFrontend {
    let (kind, mount_point) = match driver {
        Driver::Local => (local_kind, Some(default_mount_point.to_path_buf())),
        Driver::Docker | Driver::Krunkit => {
            (omnifs_workspace::runtime_record::FrontendKind::Fuse, None)
        },
    };
    EffectiveFrontend {
        kind,
        driver,
        mount_point,
        provenance: Provenance::Default,
    }
}

#[cfg(feature = "daemon")]
async fn run_local(
    paths: &omnifs_workspace::layout::WorkspaceLayout,
    mount_name: &str,
    kind: omnifs_workspace::runtime_record::FrontendKind,
    mount_point: std::path::PathBuf,
) -> anyhow::Result<()> {
    let backend = LocalBackend::new(paths.clone(), mount_point.clone(), kind.into())?;
    crate::ui::narrate(format!("Starting the local {} frontend", kind.label()));
    backend.launch(mount_name).await?;

    let probe_path = mount_point.join(mount_name);
    crate::ui::narrate(format!("✓ {} is mounted", probe_path.display()));
    crate::ui::narrate("");
    crate::ui::narrate(format!(
        "Run `{}` to browse the local mount.",
        crate::style::bold("omnifs shell")
    ));
    Ok(())
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

    crate::ui::narrate("Requesting the daemon's TCP namespace attach target");
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

    wait_for_mount(
        &backend,
        mount_name,
        DOCKER_MOUNT_PROBE_TIMEOUT,
        "fuse (docker)",
    )
    .await?;

    crate::ui::narrate("");
    crate::ui::narrate(format!(
        "Run `{}` to open a shell inside the container and browse {GUEST_MOUNT}.",
        crate::style::bold("omnifs shell")
    ));
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

    crate::ui::narrate("Requesting the daemon's vsock namespace attach listener");
    let attach = workspace.daemon().frontend_attach_target_vsock().await?;

    let backend = KrunkitBackend::new(paths.config_dir.clone());
    let spec = FrontendLaunchSpec::Krunkit {
        attach_socket: std::path::PathBuf::from(attach.socket_path),
        attach_token: attach.token.clone(),
        guest_image,
    };
    crate::ui::narrate("Starting the krunkit guest");
    backend.launch(&spec).await?;

    wait_for_mount(
        &backend,
        mount_name,
        KRUNKIT_MOUNT_PROBE_TIMEOUT,
        "krunkit guest",
    )
    .await?;

    crate::ui::narrate("");
    crate::ui::narrate(format!(
        "Run `{}` to open a shell inside the guest and browse {GUEST_MOUNT}.",
        crate::style::bold("omnifs shell")
    ));
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

pub(crate) fn first_mount_name(workspace: &Workspace) -> anyhow::Result<String> {
    workspace
        .mounts()?
        .into_iter()
        .map(|mount| mount.name.to_string())
        .next()
        .context(
            "no mounts configured; run `omnifs mount add <provider>` before `omnifs frontend up`",
        )
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
    progress_key: &str,
) -> anyhow::Result<()> {
    let probe_path = format!("{GUEST_MOUNT}/{mount_name}");
    let mut row = LiveRow::start(progress_key, "waiting for mount");
    let deadline = tokio::time::Instant::now() + timeout;
    let failure = loop {
        row.update_elapsed("waiting for mount");
        match backend.mount_ready(&probe_path).await {
            Ok(true) => {
                row.settle_ok(format!("{GUEST_MOUNT} mounted"));
                return Ok(());
            },
            Ok(false) => {},
            Err(error) => break error.context(format!("probe {probe_path} readiness")),
        }
        if tokio::time::Instant::now() >= deadline {
            break anyhow::anyhow!(
                "{probe_path} did not appear inside the frontend within {}s",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(MOUNT_PROBE_INTERVAL).await;
    };

    row.settle_fail("mount failed");

    match backend.tear_down().await {
        Ok(()) => Err(failure),
        Err(cleanup) => Err(failure.context(format!("frontend cleanup also failed: {cleanup:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    enum Probe {
        Pending,
        Error,
    }

    struct FakeBackend {
        probe: Probe,
        cleanup_fails: bool,
        cleanup_calls: AtomicUsize,
    }

    impl FakeBackend {
        fn new(probe: Probe, cleanup_fails: bool) -> Self {
            Self {
                probe,
                cleanup_fails,
                cleanup_calls: AtomicUsize::new(0),
            }
        }
    }

    impl FrontendBackend for FakeBackend {
        async fn launch(&self, _spec: &FrontendLaunchSpec) -> anyhow::Result<()> {
            unreachable!()
        }

        async fn mount_ready(&self, _path: &str) -> anyhow::Result<bool> {
            match self.probe {
                Probe::Pending => Ok(false),
                Probe::Error => anyhow::bail!("readiness probe failed"),
            }
        }

        async fn is_running(&self) -> anyhow::Result<Option<bool>> {
            unreachable!()
        }

        async fn tear_down(&self) -> anyhow::Result<()> {
            self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
            if self.cleanup_fails {
                anyhow::bail!("cleanup failed");
            }
            Ok(())
        }

        fn shell_command(&self, _shell_override: Option<&str>, _trailing: &[String]) -> Command {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn probe_error_tears_down_launched_frontend() {
        let backend = FakeBackend::new(Probe::Error, false);

        let error = wait_for_mount(&backend, "github", Duration::ZERO, "test frontend")
            .await
            .unwrap_err();

        assert_eq!(backend.cleanup_calls.load(Ordering::SeqCst), 1);
        assert!(format!("{error:#}").contains("readiness probe failed"));
    }

    #[tokio::test]
    async fn timeout_tears_down_launched_frontend() {
        let backend = FakeBackend::new(Probe::Pending, false);

        let error = wait_for_mount(&backend, "github", Duration::ZERO, "test frontend")
            .await
            .unwrap_err();

        assert_eq!(backend.cleanup_calls.load(Ordering::SeqCst), 1);
        assert!(format!("{error:#}").contains("did not appear"));
    }

    #[tokio::test]
    async fn cleanup_failure_keeps_readiness_failure() {
        let backend = FakeBackend::new(Probe::Error, true);

        let error = wait_for_mount(&backend, "github", Duration::ZERO, "test frontend")
            .await
            .unwrap_err();
        let message = format!("{error:#}");

        assert_eq!(backend.cleanup_calls.load(Ordering::SeqCst), 1);
        assert!(message.contains("readiness probe failed"));
        assert!(message.contains("frontend cleanup also failed: cleanup failed"));
    }
}
