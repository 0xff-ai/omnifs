//! Daemon-owned startup and control-plane context.

use anyhow::Context as _;

use crate::app::{DaemonArgs, FrontendKind};
use omnifs_api::{
    API_MAJOR, API_MINOR, DaemonBackend, DaemonHealth, DaemonStatus, DaemonSubsystem, FrontendInfo,
    HealthState, MountFailure, MountInfo, OMNIFS_CONTAINER_NAME_ENV, OMNIFS_IMAGE_ENV,
    SubsystemHealth,
};
use omnifs_engine::HostContext;
use omnifs_nfs::NfsMountOptions;
use omnifs_workspace::layout::{Daemon, Workspace, WorkspaceLayout};
use omnifs_workspace::mounts::materialize::MaterializationMode;
use omnifs_workspace::runtime_record::{
    Endpoint, FrontendKind as RecordFrontendKind, FrontendRecord, RecordedBackend, RuntimeRecord,
};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct DaemonContext {
    layout: WorkspaceLayout,
    mount_point: PathBuf,
    frontend: FrontendKind,
    backend: DaemonBackend,
    host_native: bool,
    root_symlinks: bool,
    /// TCP control listen address. `Some` for the container (and the debug path
    /// when `--listen` is passed alongside the UDS); `None` when a host-native
    /// daemon serves only its Unix socket.
    listen: Option<SocketAddr>,
    /// Random per-start id reported in status and written to the runtime record.
    instance_id: String,
    nfs: NfsContext,
    process: ProcessInfo,
}

#[derive(Debug)]
struct NfsContext {
    bind: SocketAddr,
    state_dir: PathBuf,
    trace_path: Option<PathBuf>,
}

#[derive(Debug)]
struct ProcessInfo {
    pid: u32,
    executable: PathBuf,
}

impl DaemonContext {
    pub(crate) fn resolve(args: DaemonArgs) -> anyhow::Result<Self> {
        let workspace: Workspace<Daemon> = Workspace::resolve()?;
        let layout = workspace.into_layout();
        let frontend = FrontendKind::platform_default();
        let mount_point = omnifs_workspace::layout::resolve_mount_point().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve mount point: set HOME or OMNIFS_MOUNT_POINT")
        })?;
        let process = ProcessInfo::current();
        let backend = if args.host_native {
            DaemonBackend::Native { pid: process.pid }
        } else {
            DaemonBackend::Docker {
                container_name: std::env::var(OMNIFS_CONTAINER_NAME_ENV).unwrap_or_default(),
                image: std::env::var(OMNIFS_IMAGE_ENV).unwrap_or_default(),
            }
        };
        let nfs = NfsContext {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.nfs_port),
            state_dir: args.nfs_state_dir.unwrap_or_else(|| layout.nfs_state_dir()),
            trace_path: args.nfs_trace,
        };

        Ok(Self {
            layout,
            mount_point,
            frontend,
            backend,
            host_native: args.host_native,
            root_symlinks: args.root_symlinks,
            listen: args.listen,
            instance_id: generate_instance_id(),
            nfs,
            process,
        })
    }

    pub(crate) fn prepare_startup_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.layout.config_dir)?;
        std::fs::create_dir_all(&self.mount_point)?;
        std::fs::create_dir_all(&self.layout.cache_dir)?;
        Ok(())
    }

    pub(crate) fn is_host_native(&self) -> bool {
        self.host_native
    }

    pub(crate) fn listen(&self) -> Option<SocketAddr> {
        self.listen
    }

    pub(crate) fn control_socket(&self) -> PathBuf {
        self.layout.control_socket()
    }

    pub(crate) fn runtime_record_file(&self) -> PathBuf {
        self.layout.runtime_record_file()
    }

    pub(crate) fn bind_control_listener(addr: SocketAddr) -> anyhow::Result<TcpListener> {
        TcpListener::bind(addr).map_err(|error| {
            anyhow::anyhow!(
                "cannot bind control API listener on {addr}: {error}\n\
                 \n\
                 Likely cause: another omnifs daemon is already running on that port.\n\
                 Run `omnifs down` to stop it, then try again."
            )
        })
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
        std::fs::create_dir_all(&self.layout.config_dir)?;
        std::fs::set_permissions(
            &self.layout.config_dir,
            std::fs::Permissions::from_mode(0o700),
        )
        .with_context(|| {
            format!(
                "restrict config dir {} to 0700",
                self.layout.config_dir.display()
            )
        })?;

        if path.exists() {
            match UnixStream::connect(&path) {
                Ok(_) => {
                    anyhow::bail!(
                        "another omnifs daemon is already serving this workspace on {}.\n\
                         Run `omnifs down` to stop it, then try again.",
                        path.display()
                    );
                },
                // Refused/ENOENT means the previous daemon is gone; the socket
                // file is a leftover, so unlink and rebind.
                Err(_) => {
                    std::fs::remove_file(&path).with_context(|| {
                        format!("remove stale control socket {}", path.display())
                    })?;
                },
            }
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind control socket {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict control socket {} to 0600", path.display()))?;
        Ok(listener)
    }

    /// Assemble the runtime record for a host-native daemon: a unix endpoint,
    /// the native backend pid, this build's instance id, and the serving
    /// frontend at its mount point.
    pub(crate) fn runtime_record(&self) -> RuntimeRecord {
        RuntimeRecord::new(
            Endpoint::Unix {
                path: self.control_socket(),
            },
            RecordedBackend::Native {
                pid: self.process.pid,
            },
            self.instance_id.clone(),
            vec![FrontendRecord {
                kind: match self.frontend {
                    FrontendKind::Fuse => RecordFrontendKind::Fuse,
                    FrontendKind::Nfs => RecordFrontendKind::Nfs,
                },
                mount_point: self.mount_point.clone(),
            }],
        )
    }

    pub(crate) fn host_context(&self) -> HostContext {
        HostContext::new(
            &self.layout.cache_dir,
            &self.layout.config_dir,
            &self.layout.providers_dir,
            &self.layout.credentials_file,
        )
    }

    pub(crate) fn cache_dir(&self) -> &Path {
        &self.layout.cache_dir
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.layout.config_dir
    }

    pub(crate) fn mounts_dir(&self) -> &Path {
        &self.layout.mounts_dir
    }

    pub(crate) fn providers_dir(&self) -> &Path {
        &self.layout.providers_dir
    }

    /// The launch backend mapped to the telemetry vocabulary, recorded on every
    /// daemon lifecycle event.
    pub(crate) fn telemetry_backend(&self) -> omnifs_workspace::telemetry::Backend {
        match &self.backend {
            DaemonBackend::Native { .. } => omnifs_workspace::telemetry::Backend::Native,
            DaemonBackend::Docker { .. } => omnifs_workspace::telemetry::Backend::Docker,
        }
    }

    pub(crate) fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    pub(crate) fn frontend(&self) -> FrontendKind {
        self.frontend
    }

    pub(crate) fn root_symlinks(&self) -> bool {
        self.root_symlinks
    }

    pub(crate) fn materialization_mode(&self) -> MaterializationMode {
        match &self.backend {
            DaemonBackend::Native { .. } => MaterializationMode::HostNative,
            DaemonBackend::Docker { .. } => MaterializationMode::Docker,
        }
    }

    pub(crate) fn nfs_mount_options(&self) -> NfsMountOptions {
        let mut options = NfsMountOptions::loopback(self.nfs.state_dir.clone());
        options.bind = self.nfs.bind;
        options.trace_path.clone_from(&self.nfs.trace_path);
        options
    }

    pub(crate) fn status(
        &self,
        frontend: Option<FrontendInfo>,
        mounts: Vec<MountInfo>,
        failed: Vec<MountFailure>,
        credential_degraded: &[(String, String)],
    ) -> DaemonStatus {
        let health = self.health(frontend.as_ref(), &mounts, &failed, credential_degraded);
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            api_major: API_MAJOR,
            api_minor: API_MINOR,
            pid: self.process.pid,
            instance_id: self.instance_id.clone(),
            executable: self.process.executable.clone(),
            mount_point: self.mount_point.clone(),
            config_dir: self.layout.config_dir.clone(),
            cache_dir: self.layout.cache_dir.clone(),
            providers_dir: self.layout.providers_dir.clone(),
            frontend,
            backend: self.backend.clone(),
            mounts,
            failed,
            health,
        }
    }

    fn health(
        &self,
        frontend: Option<&FrontendInfo>,
        mounts: &[MountInfo],
        failed: &[MountFailure],
        credential_degraded: &[(String, String)],
    ) -> DaemonHealth {
        DaemonHealth::new(vec![
            SubsystemHealth::new(
                DaemonSubsystem::Control,
                HealthState::Healthy,
                match self.listen {
                    Some(addr) => format!("control API serving on {addr}"),
                    None => format!(
                        "control API serving on {}",
                        self.layout.control_socket().display()
                    ),
                },
            ),
            SubsystemHealth::new(
                DaemonSubsystem::Backend,
                HealthState::Healthy,
                backend_health_message(&self.backend),
            ),
            self.frontend_health(frontend),
            mount_health(mounts, failed, credential_degraded),
        ])
    }

    fn frontend_health(&self, frontend: Option<&FrontendInfo>) -> SubsystemHealth {
        match frontend {
            Some(frontend) => SubsystemHealth::new(
                DaemonSubsystem::Frontend,
                HealthState::Healthy,
                format!(
                    "{} serving at {}",
                    frontend.fs_type,
                    self.mount_point.display()
                ),
            ),
            None => SubsystemHealth::new(
                DaemonSubsystem::Frontend,
                HealthState::Starting,
                format!("not serving at {}", self.mount_point.display()),
            ),
        }
    }
}

fn backend_health_message(backend: &DaemonBackend) -> String {
    match backend {
        DaemonBackend::Native { pid } => format!("native daemon pid {pid}"),
        DaemonBackend::Docker {
            container_name,
            image,
        } if !container_name.is_empty() && !image.is_empty() => {
            format!("docker container {container_name} from {image}")
        },
        DaemonBackend::Docker { .. } => "docker container identity unavailable".to_string(),
    }
}

/// `credential_degraded` is `(mount, reason)` for a mount whose auth was not
/// ready at mount-start (see `Runtime::credential_warning`). Unlike `failed`,
/// a credential-degraded mount is still loaded and present in `mounts`; it
/// only pulls the Mounts subsystem down to `Degraded`, never `Unhealthy`.
fn mount_health(
    mounts: &[MountInfo],
    failed: &[MountFailure],
    credential_degraded: &[(String, String)],
) -> SubsystemHealth {
    let state = if !failed.is_empty() && mounts.is_empty() {
        HealthState::Unhealthy
    } else if !failed.is_empty() || !credential_degraded.is_empty() {
        HealthState::Degraded
    } else {
        HealthState::Healthy
    };
    let mut message = if failed.is_empty() {
        format!("{} mount(s) loaded", mounts.len())
    } else {
        format!("{} mount(s) loaded, {} failed", mounts.len(), failed.len())
    };
    if !credential_degraded.is_empty() {
        let detail = credential_degraded
            .iter()
            .map(|(mount, reason)| format!("{mount}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ");
        let _ = write!(
            message,
            ", {} mount(s) with a degraded credential ({detail})",
            credential_degraded.len()
        );
    }
    SubsystemHealth::new(DaemonSubsystem::Mounts, state, message)
}

/// A random 16-lowercase-hex-character id, generated from the same CSPRNG the
/// control token uses. Identifies one daemon start so the CLI can tell a record
/// overwritten by a restart from the daemon it is talking to.
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
