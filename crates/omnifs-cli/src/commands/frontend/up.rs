//! `omnifs frontend up`: bring up the Docker-hosted FUSE frontend.
//!
//! The frontend is a separate, credential-free container attached to a
//! host-native daemon's shared namespace over TCP; it is not a daemon runtime
//! mode; `[system].runtime` never references it.

use std::time::Duration;

use anyhow::Context as _;
use clap::Args;
use omnifs_api::DaemonBackend;
use omnifs_workspace::layout::OMNIFS_HOME_ENV;
use omnifs_workspace::runtime_record::{FrontendKind, FrontendRecord, RuntimeRecord, Via};

use crate::config::ConfiguredBackend;
use crate::frontend_container::{
    FrontendContainerSpec, build_frontend_container_body, frontend_container_name,
    resolve_frontend_image,
};
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

        anstream::eprintln!("Requesting the daemon's TCP namespace attach listener");
        let attach = workspace.daemon().attach_listeners(0).await?;
        let attach_port = attach_port(&attach.addr)?;

        let config = workspace.config()?;
        let image = resolve_frontend_image(None, &config)?;
        let is_default_home = std::env::var_os(OMNIFS_HOME_ENV).is_none();
        let container_name = frontend_container_name(&paths.config_dir, is_default_home)?;
        let target = DockerTarget::new(
            container_name.as_str().to_string(),
            image.as_str().to_string(),
        )?;

        let runtime = Runtime::connect_ready(&target, "omnifs frontend up").await?;

        let body = build_frontend_container_body(&FrontendContainerSpec {
            image: target.image(),
            home: &paths.config_dir,
            attach_port,
            attach_token: &attach.token,
            add_host_gateway: cfg!(target_os = "linux"),
        });
        runtime.launch_frontend_container(body).await?;

        assert_container_locked_down(&runtime).await?;

        let mount_name = first_mount_name(&workspace)?;
        wait_for_mount(&runtime, &mount_name).await?;

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

/// Ensure a host-native daemon is serving, reusing the same launch machinery
/// `omnifs up` uses. A running Docker-backend daemon is refused: the
/// Docker-hosted frontend attaches over the host-native TCP namespace
/// listener, which a containerized daemon has no reason to serve (its
/// in-process frontend already renders FUSE).
async fn ensure_native_daemon(workspace: &Workspace) -> anyhow::Result<()> {
    if let Some(status) = workspace.daemon().status_optional().await? {
        return match status.backend {
            DaemonBackend::Native { .. } => Ok(()),
            DaemonBackend::Docker { container_name, .. } => Err(anyhow::anyhow!(
                "the running daemon is a Docker-backend daemon (container `{container_name}`); \
                 the Docker-hosted FUSE frontend attaches to a host-native daemon instead.\n\
                 Run `omnifs down` to stop it, then `omnifs up --runtime native` before \
                 `omnifs frontend up`."
            )),
        };
    }

    let launcher = Launcher::new(workspace, "omnifs frontend up")
        .with_runtime_override(Some(ConfiguredBackend::Native));
    launcher.launch().await?;
    Ok(())
}

/// Parse the port out of the attach listener's `addr` (`"127.0.0.1:PORT"`),
/// which is what the container dials at `host.docker.internal:<port>`.
fn attach_port(addr: &str) -> anyhow::Result<u16> {
    addr.rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .with_context(|| format!("attach listener address `{addr}` has no parseable port"))
}

/// Immediately after start, assert the no-credentials contract: no mounts of
/// any kind, and an env set that is exactly the attach vars plus the image's
/// own defaults. On violation, kill the container rather than leave a
/// misconfigured frontend running.
async fn assert_container_locked_down(runtime: &Runtime) -> anyhow::Result<()> {
    let (mounts, env) = runtime.inspect_mounts_and_env().await?;
    if let Err(violation) = crate::frontend_container::assert_locked_down(&mounts, &env) {
        let _ = runtime.remove().await;
        anyhow::bail!("refusing to run the frontend container: {violation}");
    }
    Ok(())
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
async fn wait_for_mount(runtime: &Runtime, mount_name: &str) -> anyhow::Result<()> {
    let probe_path = format!("{GUEST_MOUNT}/{mount_name}");
    anstream::eprintln!("Waiting for {probe_path} inside the frontend container");
    let deadline = tokio::time::Instant::now() + MOUNT_PROBE_TIMEOUT;
    loop {
        if runtime.exec_path_exists(&probe_path).await.unwrap_or(false) {
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
    let record = match RuntimeRecord::read(record_path) {
        Ok(Some(record)) => record,
        Ok(None) => {
            anstream::eprintln!(
                "warning: runtime record missing; cannot record the frontend container"
            );
            return;
        },
        Err(error) => {
            anstream::eprintln!("warning: could not read the runtime record: {error:#}");
            return;
        },
    };
    let mut record = record;
    record
        .frontends
        .retain(|frontend| frontend.via != Some(Via::Docker));
    record.frontends.push(FrontendRecord {
        kind: FrontendKind::Fuse,
        mount_point: std::path::PathBuf::from(GUEST_MOUNT),
        via: Some(Via::Docker),
    });
    if let Err(error) = record.write(record_path) {
        anstream::eprintln!("warning: could not persist the frontend container: {error:#}");
    }
}
