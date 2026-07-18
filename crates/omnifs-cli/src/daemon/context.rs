//! Daemon-owned startup and control-plane context.

use anyhow::Context as _;

use super::app::DaemonArgs;
use omnifs_api::{
    CredentialHealth, DaemonHealth, DaemonStatus, DaemonSubsystem, FrontendInfo, HealthState,
    MountInfo, SubsystemHealth,
};
use omnifs_engine::HostContext;
use omnifs_workspace::daemon_record::{DaemonRecord, Endpoint};
use omnifs_workspace::mounts::Revision;
use omnifs_workspace::{DaemonState, FrontendState, Workspace};
use std::fmt::Write as _;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct DaemonContext {
    daemon: DaemonState,
    frontend: FrontendState,
    metrics: omnifs_workspace::metrics::Store,
    mount_revision: Revision,
    offline: bool,
    /// Random per-start id reported in status and written to the daemon record.
    instance_id: String,
    /// `--attach-tcp <port>`: bind a TCP namespace attach listener eagerly at
    /// start (`0` = ephemeral). `None` when the flag was not passed; a TCP
    /// attach listener can still be bound later through the control socket.
    attach_tcp: Option<u16>,
    process: ProcessInfo,
}

#[derive(Debug)]
struct ProcessInfo {
    pid: u32,
    executable: PathBuf,
}

impl DaemonContext {
    pub(crate) fn resolve(args: &DaemonArgs) -> anyhow::Result<Self> {
        let attach_tcp = args.attach_tcp;
        let workspace = Workspace::resolve()?;
        let daemon = workspace.daemon().clone();
        let frontend = workspace.frontend().clone();
        let metrics = workspace.metrics().clone();
        let process = ProcessInfo::current();
        anyhow::ensure!(
            args.mount_snapshot.is_dir(),
            "mount snapshot {} is not a directory",
            args.mount_snapshot.display()
        );

        Ok(Self {
            daemon,
            frontend,
            metrics,
            mount_revision: args.mount_revision.clone(),
            offline: args.offline,
            instance_id: generate_instance_id(),
            attach_tcp,
            process,
        })
    }

    pub(crate) fn prepare_startup_dirs(&self, offline: bool) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.daemon.config_dir())?;
        if !offline {
            std::fs::create_dir_all(self.daemon.cache_dir())?;
        }
        Ok(())
    }

    pub(crate) fn control_socket(&self) -> PathBuf {
        self.daemon.control_socket()
    }

    pub(crate) fn daemon_state(&self) -> &DaemonState {
        &self.daemon
    }

    pub(crate) fn attach_store(&self) -> anyhow::Result<omnifs_workspace::attach::Store> {
        Ok(self.frontend.attach_store()?)
    }

    pub(crate) fn vsock_attach_socket(&self) -> PathBuf {
        self.frontend.vsock_attach_socket()
    }

    pub(crate) fn local_attach_socket(&self) -> PathBuf {
        self.frontend.local_attach_socket()
    }

    /// Bind the host-native control socket at `<config_dir>/control.sock`.
    ///
    /// The config dir is forced to `0700` and the socket to `0600` so auth on
    /// the control plane is filesystem permissions alone. Stale-socket recovery:
    /// if the path already exists, a successful connect means another omnifs
    /// daemon is serving this workspace (a hard error pointing at `omnifs down`);
    /// a refused connection or a missing file means the socket is stale, so it is
    /// unlinked and rebound.
    pub(crate) fn bind_control_socket(&self) -> anyhow::Result<UnixListener> {
        let path = self.control_socket();
        std::fs::create_dir_all(self.daemon.config_dir())?;
        std::fs::set_permissions(
            self.daemon.config_dir(),
            std::fs::Permissions::from_mode(0o700),
        )
        .with_context(|| {
            format!(
                "restrict config dir {} to 0700",
                self.daemon.config_dir().display()
            )
        })?;

        Self::prepare_control_path(&path)?;

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind control socket {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict control socket {} to 0600", path.display()))?;
        Ok(listener)
    }

    fn prepare_control_path(path: &Path) -> anyhow::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.file_type().is_socket() => {
                // Remove only the directory entry. `remove_file` unlinks a
                // symlink itself and never follows its target.
                std::fs::remove_file(path).with_context(|| {
                    format!("remove non-socket control path {}", path.display())
                })?;
            },
            Ok(_) => match UnixStream::connect(path) {
                Ok(_) => {
                    anyhow::bail!(
                        "another omnifs daemon is already serving this workspace on {}.\n\
                         Run `omnifs down` to stop it, then try again.",
                        path.display()
                    );
                },
                // Refused/ENOENT means the previous daemon is gone; the socket
                // file is a leftover, so unlink and rebind.
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path).with_context(|| {
                        format!("remove stale control socket {}", path.display())
                    })?;
                },
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("probe control socket {}", path.display()));
                },
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect control path {}", path.display()));
            },
        }
        Ok(())
    }

    /// Assemble the initial daemon record for a host-native daemon.
    pub(crate) fn daemon_record(&self) -> DaemonRecord {
        DaemonRecord::new(
            self.mount_revision.clone(),
            Endpoint::Unix {
                path: self.control_socket(),
            },
            self.process.pid,
            self.instance_id.clone(),
            self.offline,
        )
    }

    pub(crate) fn host_context(&self) -> HostContext {
        HostContext::new(
            self.daemon.cache_dir(),
            self.daemon.config_dir(),
            self.daemon.providers_dir(),
            self.daemon.credentials_file(),
        )
    }

    pub(crate) fn clone_cache(&self) -> PathBuf {
        self.daemon.clone_cache()
    }

    pub(crate) fn mount_snapshot(&self, revision: &Revision) -> PathBuf {
        self.daemon.mount_snapshot(revision)
    }

    pub(crate) fn metrics(&self) -> &omnifs_workspace::metrics::Store {
        &self.metrics
    }

    /// The `--attach-tcp` port request, if the flag was passed. `Some(0)` asks
    /// for an ephemeral port.
    pub(crate) fn attach_tcp_port(&self) -> Option<u16> {
        self.attach_tcp
    }

    /// Build status from the live attach registry.
    pub(crate) fn status(
        &self,
        attach_serving: bool,
        frontends: Vec<FrontendInfo>,
        mounts: Vec<MountInfo>,
    ) -> DaemonStatus {
        let health = self.health(attach_serving, &frontends, &mounts);
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: self.process.pid,
            instance_id: self.instance_id.clone(),
            executable: self.process.executable.clone(),
            config_dir: self.daemon.config_dir().to_path_buf(),
            cache_dir: self.daemon.cache_dir(),
            providers_dir: self.daemon.providers_dir(),
            frontends,
            mounts,
            offline: self.offline,
            health,
        }
    }

    fn health(
        &self,
        attach_serving: bool,
        frontends: &[FrontendInfo],
        mounts: &[MountInfo],
    ) -> DaemonHealth {
        DaemonHealth::new(vec![
            SubsystemHealth::new(
                DaemonSubsystem::Control,
                HealthState::Healthy,
                format!(
                    "control socket serving on {}",
                    self.daemon.control_socket().display()
                ),
            ),
            Self::frontend_health(attach_serving, frontends),
            mount_health(mounts),
        ])
    }

    /// Listener readiness is independent of whether a frontend is currently
    /// attached. Startup flips this subsystem healthy only after mount loading and
    /// every requested listener bind have completed.
    fn frontend_health(attach_serving: bool, frontends: &[FrontendInfo]) -> SubsystemHealth {
        let mut listed = vec!["attach socket local".to_string()];
        listed.extend(frontends.iter().map(|frontend| {
            format!(
                "attached {} at {} via {}",
                frontend.fs_type,
                frontend.mount_point.display(),
                frontend.runtime
            )
        }));
        let listed = listed.join(", ");

        let (state, message) = if attach_serving {
            (
                HealthState::Healthy,
                format!("namespace listeners serving ({listed})"),
            )
        } else {
            (HealthState::Starting, format!("not serving ({listed})"))
        };
        SubsystemHealth::new(DaemonSubsystem::Frontend, state, message)
    }
}

fn mount_health(mounts: &[MountInfo]) -> SubsystemHealth {
    let degraded = mounts
        .iter()
        .filter(|mount| {
            mount
                .auth_health
                .is_some_and(CredentialHealth::needs_attention)
        })
        .count();
    let state = if degraded == 0 {
        HealthState::Healthy
    } else {
        HealthState::Degraded
    };
    let mut message = format!("{} loaded", crate::ui::render::count(mounts.len(), "mount"));
    if degraded > 0 {
        let detail = mounts
            .iter()
            .filter(|mount| {
                mount
                    .auth_health
                    .is_some_and(CredentialHealth::needs_attention)
            })
            .map(|mount| format!("{}: {:?}", mount.mount, mount.auth_health))
            .collect::<Vec<_>>()
            .join("; ");
        let _ = write!(
            message,
            ", {} with a degraded credential ({detail})",
            crate::ui::render::count(degraded, "mount")
        );
    }
    SubsystemHealth::new(DaemonSubsystem::Mounts, state, message)
}

/// A random 16-lowercase-hex-character id identifying one daemon start, so the
/// CLI can tell a record overwritten by a restart from the daemon it is talking
/// to.
fn generate_instance_id() -> String {
    let mut bytes = [0_u8; 8];
    // A failure here would only weaken an id used for equality checks, never for
    // a security decision, so fall back to a pid/time-derived value rather than
    // aborting daemon startup.
    if getrandom::fill(&mut bytes).is_err() {
        let pid = u64::from(std::process::id());
        bytes.copy_from_slice(&pid.to_le_bytes());
    }
    hex::encode(bytes)
}

impl ProcessInfo {
    fn current() -> Self {
        Self {
            pid: std::process::id(),
            executable: std::env::current_exe().unwrap_or_else(|_| PathBuf::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn context(root: &Path) -> DaemonContext {
        let workspace = Workspace::under_root(root);
        let mount_revision = Revision::new("a".repeat(40)).unwrap();
        DaemonContext {
            daemon: workspace.daemon().clone(),
            frontend: workspace.frontend().clone(),
            metrics: workspace.metrics().clone(),
            mount_revision,
            offline: false,
            instance_id: "test-instance".to_owned(),
            attach_tcp: None,
            process: ProcessInfo {
                pid: std::process::id(),
                executable: PathBuf::new(),
            },
        }
    }

    #[test]
    fn prepare_control_path_replaces_reserved_regular_file() {
        let temp = TempDir::new().unwrap();
        let daemon = context(temp.path());
        std::fs::create_dir_all(daemon.daemon.config_dir()).unwrap();
        let path = daemon.control_socket();
        std::fs::write(&path, b"reserved").unwrap();

        DaemonContext::prepare_control_path(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn prepare_control_path_unlinks_symlink_without_touching_target() {
        let temp = TempDir::new().unwrap();
        let daemon = context(temp.path());
        std::fs::create_dir_all(daemon.daemon.config_dir()).unwrap();
        let target = temp.path().join("target");
        std::fs::write(&target, b"keep").unwrap();
        let path = daemon.control_socket();
        symlink(&target, &path).unwrap();

        DaemonContext::prepare_control_path(&path).unwrap();
        assert!(target.exists(), "symlink target must not be removed");
        assert!(!path.exists());
    }
}
