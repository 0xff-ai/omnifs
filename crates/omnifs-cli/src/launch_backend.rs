//! Host-native daemon launch and the naming types used by the Docker frontend.
//!
use std::fmt;
use std::path::Path;

#[cfg(feature = "daemon")]
use anyhow::Context as _;
use anyhow::Result;

/// Whether this binary was produced by the release packaging lane
/// (`OMNIFS_RELEASE` set at compile time) or a local/dev build. Release
/// binaries default to the registry image for their version; dev binaries
/// default to the locally built dev image and never pull. Used by the
/// optional Docker-hosted FUSE frontend's image resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildChannel {
    Release,
    Dev,
}

impl BuildChannel {
    /// Why a missing registry-less image is never pulled. Only a dev binary
    /// defaults to a local image, so release errors must not call it a dev build.
    pub(crate) const fn pull_refusal_reason(self) -> &'static str {
        match self {
            Self::Dev => {
                "this omnifs binary is a dev build; it uses the locally built frontend image \
                 and never pulls from a registry"
            },
            Self::Release => {
                "registry-less image references are local build products; omnifs never pulls \
                 them from a registry"
            },
        }
    }

    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Release => "release",
        }
    }

    pub(crate) const fn version_suffix(self) -> &'static str {
        match self {
            Self::Dev => " (dev build)",
            Self::Release => "",
        }
    }
}

pub(crate) const BUILD_CHANNEL: BuildChannel = match option_env!("OMNIFS_RELEASE") {
    Some(_) => BuildChannel::Release,
    None => BuildChannel::Dev,
};

// The frontend container's mount path inside the container. The daemon
// resolves its own host-native mount point independently; this is the
// launcher's view of the frontend's guest boundary, used to build the
// `docker exec` working directory and wait for the mount. See
// `frontend_container.rs` and `commands/shell.rs`.
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

impl RunMode {
    /// The default `RUST_LOG` level for this run mode.
    pub(crate) const fn default_log_level(self) -> &'static str {
        match self {
            Self::Foreground => "warn",
            Self::Spawned => "info",
        }
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
/// references like `omnifs-frontend:dev` are local build products: a Docker
/// Hub `library/omnifs-frontend` would never be a legitimate frontend image,
/// so treating registry-less references as local-only can't hide a real
/// image.
pub(crate) fn names_registry(image: &str) -> bool {
    // Per docker's reference grammar the registry, if present, is the first
    // path segment before the first `/`. A reference with no `/` (`omnifs-frontend:dev`)
    // has no registry component. A first segment is a registry iff it carries a
    // host marker: a dot, a port colon, or the literal `localhost`.
    match image.split_once('/') {
        None => false,
        Some((first, _)) => first.contains('.') || first.contains(':') || first == "localhost",
    }
}

/// A Docker container's name and image, addressed together. Built directly by
/// the frontend commands (`omnifs frontend enable|disable`); the daemon no
/// longer runs in a container, so there is no resolution chain here to guess
/// its identity from config or environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerTarget {
    container_name: ContainerName,
    image: ImageRef,
}

impl DockerTarget {
    pub(crate) fn new(container_name: String, image: String) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: ContainerName::new(container_name)?,
            image: ImageRef::new(image)?,
        })
    }

    pub(crate) fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub(crate) fn image(&self) -> &ImageRef {
        &self.image
    }
}

// --- Native launch -----------------------------------------------------------

#[cfg(feature = "daemon")]
pub(crate) async fn launch_native(
    paths: &omnifs_workspace::layout::WorkspaceLayout,
    tcp_addr: Option<std::net::SocketAddr>,
    telemetry_enabled: bool,
) -> Result<()> {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::process::Command;

    use crate::client::DaemonClient;

    let cache_dir = &paths.cache_dir;
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

    let mut command = Command::new(&binary);
    command.arg("daemon");
    if let Some(tcp_addr) = tcp_addr {
        command.arg("--listen").arg(tcp_addr.to_string());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    // Default the spawned daemon's log level when the user has not set
    // RUST_LOG. The CLI's own foreground tracing is quieter, which would
    // otherwise hide the daemon's startup diagnostics in daemon.log.
    if std::env::var_os("RUST_LOG").is_none() {
        command.env("RUST_LOG", RunMode::Spawned.default_log_level());
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
    // fast if the child exits first. The client resolves through the runtime
    // record the daemon writes on start, so `for_layout` sees the daemon the
    // moment it publishes its record.
    let child_pid = child.id();
    let client = DaemonClient::for_layout(paths);
    for _ in 0..300 {
        if let Some(status) = child.try_wait().context("poll daemon child status")? {
            let cause = log_cause_suffix(&log_path);
            anyhow::bail!(
                "omnifs daemon exited before the mount became ready ({status}){cause}; run `omnifs logs` for the full daemon log"
            );
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
                        let _ = child.kill().await;
                        anyhow::bail!(
                            "daemon readiness came from pid {}, not spawned pid {pid}; \
                             another omnifs daemon is already serving on the control port",
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
                        let label = tcp_addr.map_or_else(
                            || "the control socket".to_string(),
                            |addr| format!("http://{addr}"),
                        );
                        return Err(crate::client::foreign_daemon_error(&label));
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
    let cause = log_cause_suffix(&log_path);
    let _ = child.kill().await;
    anyhow::bail!(
        "omnifs daemon did not become ready within 30s{cause}; run `omnifs logs` for the full daemon log"
    )
}

#[cfg(not(feature = "daemon"))]
pub(crate) async fn launch_native(
    _paths: &omnifs_workspace::layout::WorkspaceLayout,
    _tcp_addr: Option<std::net::SocketAddr>,
    _telemetry_enabled: bool,
) -> Result<()> {
    anyhow::bail!(
        "this omnifs binary was built without host-native daemon support; \
         rebuild with the `daemon` feature"
    )
}

#[cfg(feature = "daemon")]
/// The daemon's last non-empty log line, which is almost always its fatal
/// error (a startup crash writes the cause last). Surfacing that one line keeps
/// the failure legible; `omnifs logs` shows the rest. Dumping the whole tail
/// buried the cause under repeated warnings.
fn last_log_line(log_path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(log_path).ok()?;
    contents
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::to_owned)
}

/// Format the daemon's fatal line as a `: cause` suffix, or nothing.
fn log_cause_suffix(log_path: &Path) -> String {
    last_log_line(log_path).map_or_else(String::new, |line| format!(": {line}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_registry_table() {
        // (reference, expects a registry host, note)
        let cases = [
            ("omnifs-frontend:dev", false),
            ("omnifs-frontend:abc123-dev", false),
            // A Docker Hub org path is a pull target in docker semantics, but
            // NOT for us: its first segment carries no host marker.
            ("myorg/omnifs-frontend:1.0", false),
            ("ghcr.io/0xff-ai/omnifs-frontend:0.2.1", true),
            ("localhost:5000/omnifs-frontend:x", true),
            ("registry.local/omnifs-frontend", true),
        ];
        for (image, expected) in cases {
            assert_eq!(
                names_registry(image),
                expected,
                "names_registry({image:?}) should be {expected}"
            );
        }
    }
}
