//! Shared launch choreography for `omnifs up` and `omnifs dev`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use omnifs_api::{API_MAJOR, API_MINOR, DaemonStatus};
use omnifs_creds::CredentialStore;
use omnifs_home::Paths;

use crate::backend::{Backend, LaunchParams};
use crate::catalog::ProviderCatalog;
use crate::client::DaemonClient;
use crate::launch_record::LaunchRecord;
use crate::runtime::{ContainerExtras, Runtime};
use crate::runtime_target::RuntimeTarget;
use crate::session::MountConfig;

/// Everything a caller must supply to run the full launch sequence.
pub(crate) struct LaunchSpec<'a> {
    pub runtime: &'a RuntimeTarget,
    pub paths: &'a Paths,
    pub store: Box<dyn CredentialStore>,
    /// Command name shown in Docker-readiness diagnostics, e.g. `"omnifs up"`.
    pub verb: &'static str,
    /// Mount configs to materialize and push to the daemon.
    pub configs: Vec<MountConfig>,
    /// Extra binds layered on top of materialized preopens.
    pub extras: ContainerExtras,
    /// Run the daemon host-native (spawn `omnifs daemon`) instead of
    /// launching a Docker container.
    pub host_native: bool,
}

/// Run the full materialize → connect → launch → wait → push sequence.
pub(crate) async fn launch_runtime(
    spec: LaunchSpec<'_>,
    catalog: &ProviderCatalog,
) -> anyhow::Result<()> {
    let LaunchSpec {
        runtime,
        paths,
        store,
        verb,
        configs,
        mut extras,
        host_native,
    } = spec;

    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("create runtime home {}", paths.config_dir.display()))?;

    // Host-native: the daemon loads and materializes mounts from `mounts/`
    // itself, so the CLI only spawns it and triggers a reconcile. No CLI-side
    // materialize-and-push on this path.
    if host_native {
        return launch_host_native(paths, verb).await;
    }

    anstream::println!("Computing container binds for {} mount(s)", configs.len());
    let mut preopen_binds = Vec::new();
    for config in &configs {
        // Docker needs preopen binds present before `docker create`; the spec
        // itself is not pushed, only read to derive the binds.
        let binds = config.materialize(catalog, store.as_ref(), host_native)?;
        preopen_binds.extend(binds);
    }

    // Materialized preopen binds come first; caller extras append after.
    let mut binds = preopen_binds;
    binds.append(&mut extras.binds);
    extras.binds = binds;

    let rt = Runtime::connect_ready(runtime, verb).await?;
    rt.launch_container(&paths.config_dir, extras).await?;

    if let Err(error) =
        finish_docker_launch(&rt, paths, runtime.container_name(), runtime.image()).await
    {
        if let Err(teardown) = rt.remove().await {
            anstream::eprintln!("also failed to remove the container: {teardown:#}");
        }
        return Err(error);
    }

    Ok(())
}

/// Parse a `host:port` string into a `SocketAddr`, falling back to
/// `127.0.0.1:DEFAULT_PORT` on any parse error.
fn parse_control_addr(addr: &str) -> SocketAddr {
    addr.parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], omnifs_api::DEFAULT_PORT)))
}

/// Read the daemon control address from the environment (`OMNIFS_DAEMON_ADDR`),
/// falling back to `127.0.0.1:DEFAULT_PORT` on any parse error. Both the
/// spawned daemon (`--listen`) and the client (`DaemonClient::new`) use this
/// so a per-test override moves them together.
fn resolve_control_addr() -> SocketAddr {
    parse_control_addr(&crate::inspector::daemon_addr())
}

/// Spawn a detached host-native daemon and wait for it to serve. The daemon
/// reconciles `mounts/` on start; the CLI triggers one more reconcile to
/// converge any change since and to surface per-mount failures.
async fn launch_host_native(paths: &Paths, verb: &str) -> anyhow::Result<()> {
    reject_existing_host_daemon(paths, verb).await?;
    anstream::println!("Starting omnifs daemon (host-native)");

    // Build the params and delegate spawn+wait to the backend abstraction so
    // the native path and Docker path share the same argument generator.
    let addr = resolve_control_addr();
    let params = LaunchParams {
        paths: paths.clone(),
        control_addr: addr,
        mount_point: None, // the daemon resolves its default
        backend: Backend::Native,
    };
    crate::backend::launch_native(&params).await?;

    let client = DaemonClient::new();
    match client.reconcile().await {
        Ok(report) => report_reconcile_failures(&report),
        Err(error) => {
            return Err(error);
        },
    }

    // Read daemon status to get the mount point and PID for the launch record.
    if let Ok(status) = client.status().await {
        anstream::println!("✓ Mount is serving at {}", status.mount_point.display());
        anstream::println!("✓ Runtime sees {} provider(s)", status.mounts.len());
        let record_params = LaunchParams {
            paths: paths.clone(),
            control_addr: addr,
            mount_point: Some(status.mount_point.clone()),
            backend: Backend::Native,
        };
        write_launch_record(&paths.config_dir, &record_params, Some(status.pid));
    }
    Ok(())
}

async fn reject_existing_host_daemon(paths: &Paths, verb: &str) -> anyhow::Result<()> {
    let client = DaemonClient::new();
    let Some(status) = client.status_optional().await? else {
        return Ok(());
    };

    anyhow::bail!("{}", ExistingDaemon::new(status, paths, verb))
}

struct ExistingDaemon {
    status: DaemonStatus,
    paths: Paths,
    verb: String,
}

impl ExistingDaemon {
    fn new(status: DaemonStatus, paths: &Paths, verb: &str) -> Self {
        Self {
            status,
            paths: paths.clone(),
            verb: verb.to_string(),
        }
    }

    fn daemon_executable(&self) -> &Path {
        self.status.executable.as_path()
    }

    fn paths_match(&self) -> bool {
        same_path(&self.status.config_dir, &self.paths.config_dir)
            && same_path(&self.status.cache_dir, &self.paths.cache_dir)
    }

    fn executable_matches(&self) -> Option<bool> {
        let daemon = self.daemon_executable();
        if daemon.as_os_str().is_empty() {
            return None;
        }
        std::env::current_exe()
            .map(|current| same_path(daemon, &current))
            .ok()
    }

    fn title(&self) -> &'static str {
        if self.status.api_major != API_MAJOR || self.status.version != env!("CARGO_PKG_VERSION")
        {
            "A different omnifs daemon is already running"
        } else if !self.paths_match() {
            "An omnifs daemon is already running for a different home"
        } else if self.executable_matches() == Some(false) {
            "A different omnifs daemon is already running"
        } else {
            "omnifs daemon is already running"
        }
    }
}

impl std::fmt::Display for ExistingDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.title())?;
        writeln!(
            f,
            "  daemon  v{}  API {}.{}  pid {}  {}",
            self.status.version,
            self.status.api_major,
            self.status.api_minor,
            self.status.pid,
            display_path(self.daemon_executable())
        )?;
        writeln!(
            f,
            "  this    v{}  API {}.{}       {}",
            env!("CARGO_PKG_VERSION"),
            API_MAJOR,
            API_MINOR,
            display_path(&std::env::current_exe().unwrap_or_else(|_| PathBuf::new()))
        )?;
        writeln!(f)?;
        writeln!(f, "  daemon config  {}", self.status.config_dir.display())?;
        writeln!(f, "  this config    {}", self.paths.config_dir.display())?;
        writeln!(f, "  daemon cache   {}", self.status.cache_dir.display())?;
        writeln!(f, "  this cache     {}", self.paths.cache_dir.display())?;
        writeln!(f)?;
        if self.status.api_major != API_MAJOR || self.status.version != env!("CARGO_PKG_VERSION")
        {
            writeln!(
                f,
                "This looks like an upgrade boundary. Stop the running daemon, then rerun `{}`:",
                self.verb
            )?;
        } else if self.executable_matches() == Some(false) {
            writeln!(
                f,
                "This looks like a different omnifs build or worktree. Stop it before rerunning `{}`:",
                self.verb
            )?;
        } else {
            writeln!(
                f,
                "Stop or restart the running daemon before rerunning `{}`:",
                self.verb
            )?;
        }
        write!(f, "  omnifs down\n  {}", self.verb)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    canonical_path(left) == canonical_path(right)
}

fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "<unknown>".to_string()
    } else {
        path.display().to_string()
    }
}

/// Print any mounts that failed to converge during reconcile as warnings; a
/// failed mount does not abort the launch, since the rest are serving.
fn report_reconcile_failures(report: &omnifs_api::ReconcileReport) {
    for failure in &report.failed {
        anstream::eprintln!(
            "warning: mount `{}` did not load: {}",
            failure.mount,
            failure.reason
        );
    }
}

/// Docker path: wait for the in-container daemon to serve (it reconciles from
/// `mounts/` on start), then converge once more over the control API to surface
/// any per-mount failure. No spec crosses the wire.
async fn finish_docker_launch(
    rt: &Runtime,
    paths: &Paths,
    container_name: &crate::container_name::ContainerName,
    image: &crate::image_ref::ImageRef,
) -> anyhow::Result<()> {
    rt.wait_for_daemon_ready().await?;
    let client = DaemonClient::new();
    client.require_compatible().await?;
    let report = client.reconcile().await?;
    report_reconcile_failures(&report);
    if let Ok(status) = client.status().await {
        anstream::println!("✓ Runtime sees {} provider(s)", status.mounts.len());
        let addr: SocketAddr = format!("127.0.0.1:{}", omnifs_api::DEFAULT_PORT)
            .parse()
            .expect("static address is valid");
        let record_params = LaunchParams {
            paths: paths.clone(),
            control_addr: addr,
            mount_point: Some(status.mount_point.clone()),
            backend: Backend::Docker {
                container_name: container_name.clone(),
                image: image.clone(),
            },
        };
        write_launch_record(&paths.config_dir, &record_params, None);
    }
    Ok(())
}

/// Build and persist the launch record at `<config_dir>/launch.json`.
/// Best-effort: a failure here is logged but does not abort the launch, since
/// the daemon is already serving.
fn write_launch_record(config_dir: &Path, params: &LaunchParams, daemon_pid: Option<u32>) {
    match LaunchRecord::new(params, daemon_pid) {
        Ok(record) => {
            if let Err(error) = record.write(config_dir) {
                anstream::eprintln!("warning: could not write launch record: {error:#}");
            }
        },
        Err(error) => {
            anstream::eprintln!("warning: could not build launch record: {error:#}");
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed address string must fall back to the default port rather
    /// than panicking or propagating an error.
    #[test]
    fn parse_control_addr_falls_back_on_bad_input() {
        let addr = parse_control_addr("not-a-valid-socket-addr!!!");
        assert_eq!(addr.port(), omnifs_api::DEFAULT_PORT);
        assert_eq!(addr.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
    }

    /// A well-formed address string must be used as-is.
    #[test]
    fn parse_control_addr_parses_valid_input() {
        let addr = parse_control_addr("127.0.0.1:19999");
        assert_eq!(addr.port(), 19999);
        assert_eq!(addr.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
    }
}
