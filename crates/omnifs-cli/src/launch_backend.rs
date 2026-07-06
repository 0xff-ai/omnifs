//! The launch backend: native-vs-Docker target types and the spawn/reclaim
//! operations behind them.
//!
//! `LaunchBackend` is the one type callers branch on. Native spawn builds
//! typed [`omnifs_daemon::DaemonArgs`] so flag knowledge stays next to the
//! daemon argument surface. `LaunchBackend::reclaim` tears down
//! backend-specific resources after a control-API shutdown; callers (down.rs,
//! reset.rs) never branch on native-vs-docker themselves.

use std::fmt;
use std::net::SocketAddr;
use std::path::Path;

#[cfg(feature = "daemon")]
use anyhow::Context as _;
use anyhow::Result;
use omnifs_api::DaemonBackend;
#[cfg(feature = "daemon")]
use omnifs_daemon::DaemonArgs;

use crate::config::{Config, ConfiguredBackend, resolve_setting};

pub(crate) const CONTAINER_NAME: &str = "omnifs";

/// Whether this binary was produced by the release packaging lane
/// (`OMNIFS_RELEASE` set at compile time) or a local/dev build. Release
/// binaries default to the registry image for their version; dev binaries
/// default to the locally built dev image and never pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildChannel {
    Release,
    Dev,
}

pub(crate) const BUILD_CHANNEL: BuildChannel = match option_env!("OMNIFS_RELEASE") {
    Some(_) => BuildChannel::Release,
    None => BuildChannel::Dev,
};

pub(crate) const RELEASE_IMAGE: &str =
    concat!("ghcr.io/0xff-ai/omnifs:", env!("CARGO_PKG_VERSION"));
pub(crate) const DEV_IMAGE: &str = "omnifs:dev";

pub(crate) const fn default_image_for(channel: BuildChannel) -> &'static str {
    match channel {
        BuildChannel::Release => RELEASE_IMAGE,
        BuildChannel::Dev => DEV_IMAGE,
    }
}

pub(crate) const ENV_IMAGE: &str = omnifs_api::OMNIFS_IMAGE_ENV;
pub(crate) const ENV_CONTAINER_NAME: &str = omnifs_api::OMNIFS_CONTAINER_NAME_ENV;

// The guest-container paths the launcher targets. The container declares the
// same values as image ENV (see `Dockerfile`), and the daemon resolves them
// from the `OMNIFS_HOME` / `OMNIFS_MOUNT_POINT` env vars; these consts are the
// host launcher's view of that boundary, used to build bind mounts, set the
// `docker exec` working directory, and wait for the mount.
pub(crate) const GUEST_HOME: &str = "/root/.omnifs";
pub(crate) const GUEST_MOUNT: &str = "/omnifs";

/// How the omnifs process is running, which sets its default tracing level.
#[derive(Clone, Copy)]
pub(crate) enum RunMode {
    /// A foreground CLI invocation: stays quiet so ordinary commands are not
    /// noisy.
    Foreground,
    /// A background daemon the CLI spawned: defaults louder so its startup
    /// diagnostics are captured in daemon.log rather than hidden.
    Spawned,
}

/// The default `RUST_LOG` level for each run mode, and the one place the level
/// strings live. Every default-level site (the CLI's own tracing filter and the
/// spawned daemon's `RUST_LOG`) selects through here instead of spelling a
/// literal.
pub(crate) const fn default_daemon_log_level(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Foreground => "warn",
        RunMode::Spawned => "info",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ContainerName(String);

impl ContainerName {
    pub(crate) fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        validate_container_name(&name)?;
        Ok(Self(name))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn validate_container_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("container name must not be empty");
    }
    if name.len() > 64 {
        anyhow::bail!("container name must be at most 64 characters");
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("container name must not be empty");
    };
    if !first.is_ascii_alphanumeric() {
        anyhow::bail!("container name must start with an ASCII letter or digit");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        anyhow::bail!("container name may only contain ASCII letters, digits, _, ., and -");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageRef(String);

impl ImageRef {
    pub(crate) fn new(image: impl Into<String>) -> anyhow::Result<Self> {
        let image = image.into();
        if image.trim().is_empty() {
            anyhow::bail!("image reference must not be empty");
        }
        Ok(Self(image))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// omnifs only pulls images whose reference names a registry host
/// (first path segment contains `.` or `:`, or is `localhost`). Bare
/// references like `omnifs:dev` are local build products: a Docker Hub
/// `library/omnifs` would never be a legitimate runtime image, so treating
/// registry-less references as local-only can't hide a real image.
pub(crate) fn names_registry(image: &str) -> bool {
    // Per docker's reference grammar the registry, if present, is the first
    // path segment before the first `/`. A reference with no `/` (`omnifs:dev`)
    // has no registry component. A first segment is a registry iff it carries a
    // host marker: a dot, a port colon, or the literal `localhost`.
    match image.split_once('/') {
        None => false,
        Some((first, _)) => first.contains('.') || first.contains(':') || first == "localhost",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerTarget {
    container_name: ContainerName,
    image: ImageRef,
}

impl DockerTarget {
    pub(crate) fn default_target() -> anyhow::Result<Self> {
        Self::new(
            CONTAINER_NAME.to_string(),
            default_image_for(BUILD_CHANNEL).to_string(),
        )
    }

    pub(crate) fn new(container_name: String, image: String) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: ContainerName::new(container_name)?,
            image: ImageRef::new(image)?,
        })
    }

    pub(crate) fn resolve(
        container_name: Option<String>,
        image: Option<String>,
        config: &Config,
    ) -> anyhow::Result<Self> {
        let container_name = Self::resolve_container_name(container_name, config)?;
        let image = Self::resolve_image(image, config)?;
        Ok(Self {
            container_name,
            image,
        })
    }

    pub(crate) fn from_config(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: Self::container_name_from_config(config)?,
            image: Self::image_from_config(config)?,
        })
    }

    pub(crate) fn resolve_container_name(
        container_name: Option<String>,
        config: &Config,
    ) -> anyhow::Result<ContainerName> {
        let name = resolve_setting(
            container_name,
            ENV_CONTAINER_NAME,
            || config.system.container_name.clone(),
            CONTAINER_NAME.to_string(),
        );
        ContainerName::new(name)
    }

    pub(crate) fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub(crate) fn image(&self) -> &ImageRef {
        &self.image
    }

    fn resolve_image(image: Option<String>, config: &Config) -> anyhow::Result<ImageRef> {
        let image = resolve_setting(
            image,
            ENV_IMAGE,
            || config.system.image.clone(),
            default_image_for(BUILD_CHANNEL).to_string(),
        );
        ImageRef::new(image)
    }

    fn container_name_from_config(config: &Config) -> anyhow::Result<ContainerName> {
        let container_name = config
            .system
            .container_name
            .clone()
            .unwrap_or_else(|| CONTAINER_NAME.to_string());
        ContainerName::new(container_name)
    }

    fn image_from_config(config: &Config) -> anyhow::Result<ImageRef> {
        let image = config
            .system
            .image
            .clone()
            .unwrap_or_else(|| default_image_for(BUILD_CHANNEL).to_string());
        ImageRef::new(image)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaunchBackend {
    Native,
    Docker(DockerTarget),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BackendOverrides {
    pub(crate) runtime: Option<ConfiguredBackend>,
}

impl LaunchBackend {
    /// Resolve the launch backend from CLI overrides and persisted config.
    /// An empty override set reproduces the configured backend behavior.
    pub(crate) fn resolve(overrides: BackendOverrides, config: &Config) -> anyhow::Result<Self> {
        let backend = overrides.runtime.unwrap_or_else(|| config.backend());
        match backend {
            ConfiguredBackend::Native => Ok(Self::Native),
            ConfiguredBackend::Docker => Ok(Self::Docker(DockerTarget::from_config(config)?)),
        }
    }

    /// Reclaim backend-specific resources after a graceful control-API shutdown
    /// has been attempted. For native: sweep any stale mount. For Docker: stop
    /// and remove the container.
    ///
    /// `mount_point` is the mount to sweep if the daemon is already dead and
    /// left a stale mount behind. `nfs_state_dir` is where the non-Linux daemon
    /// records its mount-state files (derived from the caller's resolved paths,
    /// so it honors `OMNIFS_HOME`/cache overrides). The unmount is always forced,
    /// since reclaim runs only after the daemon stopped managing its own mount.
    pub(crate) async fn reclaim(
        &self,
        mount_point: Option<&Path>,
        nfs_state_dir: &Path,
    ) -> Result<()> {
        match self {
            LaunchBackend::Native => reclaim_native(mount_point, nfs_state_dir),
            LaunchBackend::Docker(target) => reclaim_docker(target.container_name()).await,
        }
    }
}

impl TryFrom<&DaemonBackend> for LaunchBackend {
    type Error = anyhow::Error;

    fn try_from(backend: &DaemonBackend) -> Result<Self> {
        match backend {
            DaemonBackend::Native { .. } => Ok(Self::Native),
            DaemonBackend::Docker {
                container_name,
                image,
            } => Ok(Self::Docker(DockerTarget::new(
                container_name.clone(),
                image.clone(),
            )?)),
        }
    }
}

// --- Native launch -----------------------------------------------------------

#[cfg(feature = "daemon")]
pub(crate) async fn launch_native(
    cache_dir: &Path,
    control_addr: SocketAddr,
    telemetry_enabled: bool,
) -> Result<()> {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::process::Command;

    use crate::client::DaemonClient;

    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("create cache dir {}", cache_dir.display()))?;

    let binary = std::env::current_exe().context("resolve the omnifs executable")?;
    let log_path = cache_dir.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("clone daemon log handle {}", log_path.display()))?;

    let daemon_args = DaemonArgs::host_native(control_addr);
    let argv = daemon_args.to_argv();
    let mut command = Command::new(&binary);
    for arg in &argv {
        command.arg(arg);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    // Default the spawned daemon's log level when the user has not set
    // RUST_LOG. The CLI's own foreground tracing is quieter, which would
    // otherwise hide the daemon's startup diagnostics in daemon.log.
    if std::env::var_os("RUST_LOG").is_none() {
        command.env("RUST_LOG", default_daemon_log_level(RunMode::Spawned));
    }

    // Carry the telemetry off-switch into the daemon child. Only set it when
    // disabled: an unset `OMNIFS_TELEMETRY` reads as enabled.
    if !telemetry_enabled {
        command.env(omnifs_workspace::telemetry::ENV_SWITCH, "0");
    }

    // Own process group so the daemon is not signalled when the CLI or its
    // shell exits.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn omnifs daemon ({})", binary.display()))?;

    // Poll readiness at a 100ms cadence (snappy startup) for up to 30s; fail
    // fast if the child exits first.
    let child_pid = child.id();
    let client = DaemonClient::new();
    for _ in 0..300 {
        if let Some(status) = child.try_wait().context("poll daemon child status")? {
            let tail = read_log_tail(&log_path);
            anyhow::bail!("omnifs daemon exited before the mount became ready ({status})\n{tail}");
        }
        if client.ready().await {
            if let Some(pid) = child_pid {
                match client.status().await {
                    Ok(status) if status.pid == pid => {
                        // Confirmed our daemon; drop the handle (kill_on_drop
                        // is false) to detach it.
                        drop(child);
                        return Ok(());
                    },
                    Ok(status) => {
                        let tail = read_log_tail(&log_path);
                        let _ = child.kill().await;
                        anyhow::bail!(
                            "daemon readiness came from pid {}, not spawned pid {pid}; \
                             another omnifs daemon is already serving on the control port\n{tail}",
                            status.pid
                        );
                    },
                    // Ready, but our token is rejected: a daemon owned by a
                    // different workspace holds the port. Fail fast with the
                    // foreign-daemon diagnosis instead of polling until timeout.
                    Err(error)
                        if crate::error::exit_code(&error)
                            == crate::error::ExitCode::AuthRequired =>
                    {
                        let _ = child.kill().await;
                        return Err(crate::client::foreign_daemon_error(&format!(
                            "http://{control_addr}"
                        )));
                    },
                    // A transient status error during our own startup: keep
                    // polling until ready or the timeout.
                    Err(_) => {},
                }
            } else {
                drop(child);
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let tail = read_log_tail(&log_path);
    let _ = child.kill().await;
    anyhow::bail!("omnifs daemon did not become ready within 30s\n{tail}")
}

#[cfg(not(feature = "daemon"))]
pub(crate) async fn launch_native(
    _cache_dir: &Path,
    _control_addr: SocketAddr,
    _telemetry_enabled: bool,
) -> Result<()> {
    anyhow::bail!(
        "this omnifs binary was built without host-native daemon support; \
         configure the Docker runtime or rebuild with the `daemon` feature"
    )
}

#[cfg(feature = "daemon")]
fn read_log_tail(log_path: &Path) -> String {
    const TAIL: usize = 4096;
    match std::fs::read(log_path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(TAIL);
            format!(
                "--- {} (tail) ---\n{}",
                log_path.display(),
                String::from_utf8_lossy(&bytes[start..])
            )
        },
        Err(error) => format!("(could not read {}: {error})", log_path.display()),
    }
}

// --- Native reclaim ----------------------------------------------------------

/// Sweep any stale mount left by a dead host-native daemon. On Linux the FUSE
/// mount at `mount_point` is unmounted directly; on other platforms the NFS
/// mount-state files under `nfs_state_dir` drive the sweep.
#[cfg(feature = "daemon")]
pub(crate) fn reclaim_native(mount_point: Option<&Path>, nfs_state_dir: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = nfs_state_dir;
        let Some(mp) = mount_point else {
            anstream::println!("Nothing to tear down.");
            return Ok(());
        };
        if crate::host_teardown::teardown_host_native_fuse(mp)? {
            anstream::println!("✓ Unmounted {}", mp.display());
        } else {
            anstream::println!("Nothing to tear down.");
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        // The mount point is host-visible; the NFS server's mount-state files
        // (pid, mount point, version) live under `nfs_state_dir` and are what
        // drive an actual unmount. The caller derives `nfs_state_dir` from its
        // resolved paths, so it honors OMNIFS_HOME and cache-dir overrides.
        let _ = mount_point;
        sweep_nfs_state_dir(nfs_state_dir)
    }
}

#[cfg(not(feature = "daemon"))]
pub(crate) fn reclaim_native(_mount_point: Option<&Path>, _nfs_state_dir: &Path) -> Result<()> {
    anyhow::bail!(
        "this omnifs binary was built without host-native daemon support; \
         rerun teardown with a default omnifs build"
    )
}

#[cfg(feature = "daemon")]
#[cfg(not(target_os = "linux"))]
fn sweep_nfs_state_dir(state_dir: &Path) -> Result<()> {
    let summary = crate::host_teardown::teardown_host_native_nfs(state_dir)?;
    if summary.unmounted > 0 {
        anstream::println!("✓ Unmounted {} host-native mount(s)", summary.unmounted);
    }
    if summary.swept_orphans > 0 {
        anstream::println!(
            "✓ Swept {} orphaned mount-state file(s)",
            summary.swept_orphans
        );
    }
    if summary.unmounted == 0 && summary.swept_orphans == 0 {
        if summary.skipped > 0 {
            anstream::println!(
                "No teardown performed; {} mount-state file(s) were unreadable (see warnings above).",
                summary.skipped
            );
        } else {
            anstream::println!("Nothing to tear down.");
        }
    }
    if !summary.failed.is_empty() {
        anyhow::bail!("{} mount(s) could not be unmounted", summary.failed.len());
    }
    Ok(())
}

// --- Docker reclaim ----------------------------------------------------------

async fn reclaim_docker(container_name: &ContainerName) -> Result<()> {
    let runtime = DockerTarget::default_target()
        .and_then(|target| crate::runtime::Runtime::connect_for(&target));
    match runtime {
        Ok(runtime) => {
            runtime.remove_existing(container_name).await?;
            anstream::println!("✓ Container `{container_name}` removed");
            Ok(())
        },
        Err(error) => {
            anstream::eprintln!(
                "⚠  Docker not reachable; could not remove container `{container_name}`: {error}"
            );
            Ok(())
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[allow(unsafe_code)] // env::set_var/remove_var require unsafe; guarded by ENV_LOCK.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let saved: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(*key).ok()))
            .collect();

        // SAFETY: ENV_LOCK is held for the entire duration of this call,
        // so no other thread is reading or writing the environment concurrently.
        for (key, value) in vars {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        f();

        // SAFETY: ENV_LOCK is still held; restoring the saved values is subject
        // to the same serialization guarantee as the writes above.
        for (key, original) in &saved {
            match original {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }

    #[test]
    fn dev_channel_defaults_to_local_dev_image() {
        assert_eq!(default_image_for(BuildChannel::Dev), "omnifs:dev");
    }

    #[test]
    fn names_registry_table() {
        // (reference, expects a registry host, note)
        let cases = [
            ("omnifs:dev", false),
            ("omnifs:abc123-dev", false),
            // A Docker Hub org path is a pull target in docker semantics, but
            // NOT for us: its first segment carries no host marker.
            ("myorg/omnifs:1.0", false),
            ("ghcr.io/0xff-ai/omnifs:0.2.1", true),
            ("localhost:5000/omnifs:x", true),
            ("registry.local/omnifs", true),
        ];
        for (image, expected) in cases {
            assert_eq!(
                names_registry(image),
                expected,
                "names_registry({image:?}) should be {expected}"
            );
        }
    }

    #[test]
    fn docker_image_resolution_precedence() {
        with_env(&[(ENV_IMAGE, None), (ENV_CONTAINER_NAME, None)], || {
            let config = Config {
                system: crate::config::ConfigSystem {
                    image: Some("ghcr.io/example/custom:1.2.3".into()),
                    ..Default::default()
                },
                ..Default::default()
            };
            let target = DockerTarget::resolve(None, None, &config).unwrap();
            assert_eq!(target.image().as_str(), "ghcr.io/example/custom:1.2.3");
        });

        with_env(
            &[
                (ENV_IMAGE, Some("ghcr.io/example/env:9.9.9")),
                (ENV_CONTAINER_NAME, None),
            ],
            || {
                let config = Config {
                    system: crate::config::ConfigSystem {
                        image: Some("ghcr.io/example/config:1.0.0".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let target = DockerTarget::resolve(None, None, &config).unwrap();
                assert_eq!(target.image().as_str(), "ghcr.io/example/env:9.9.9");

                let target =
                    DockerTarget::resolve(None, Some("ghcr.io/example/cli:2.0.0".into()), &config)
                        .unwrap();
                assert_eq!(target.image().as_str(), "ghcr.io/example/cli:2.0.0");
            },
        );
    }

    #[test]
    fn from_config_ignores_env_docker_target_overrides() {
        with_env(
            &[
                (ENV_IMAGE, Some("ghcr.io/example/env:9.9.9")),
                (ENV_CONTAINER_NAME, Some("omnifs-env")),
            ],
            || {
                let config = Config {
                    system: crate::config::ConfigSystem {
                        runtime: Some(ConfiguredBackend::Docker),
                        image: Some("ghcr.io/example/config:1.0.0".into()),
                        container_name: Some("omnifs-config".into()),
                    },
                    ..Default::default()
                };
                let backend = LaunchBackend::resolve(BackendOverrides::default(), &config).unwrap();
                let LaunchBackend::Docker(target) = backend else {
                    panic!("expected docker backend");
                };
                assert_eq!(target.image().as_str(), "ghcr.io/example/config:1.0.0");
                assert_eq!(target.container_name().as_str(), "omnifs-config");
            },
        );
    }

    #[test]
    fn docker_container_name_resolution_precedence() {
        with_env(
            &[(ENV_IMAGE, None), (ENV_CONTAINER_NAME, Some("omnifs-env"))],
            || {
                let config = Config {
                    system: crate::config::ConfigSystem {
                        container_name: Some("omnifs-config".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let container_name = DockerTarget::resolve_container_name(None, &config).unwrap();
                assert_eq!(container_name.as_str(), "omnifs-env");

                let container_name =
                    DockerTarget::resolve_container_name(Some("omnifs-cli".into()), &config)
                        .unwrap();
                assert_eq!(container_name.as_str(), "omnifs-cli");
            },
        );
    }
}
