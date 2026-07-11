//! Daemon-owned startup and control-plane context.

use anyhow::Context as _;

use crate::app::DaemonArgs;
use omnifs_api::{
    API_MAJOR, API_MINOR, DaemonBackend, DaemonHealth, DaemonStatus, DaemonSubsystem, FrontendInfo,
    HealthState, MountFailure, MountInfo, SubsystemHealth,
};
use omnifs_engine::HostContext;
use omnifs_workspace::layout::{Daemon, Workspace, WorkspaceLayout};
use omnifs_workspace::runtime_record::{Endpoint, RecordedBackend, RuntimeRecord};
use std::fmt::Write as _;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Conservative `sockaddr_un.sun_path` byte budget: Linux allows 108 bytes,
/// macOS 104; this stays under both with margin rather than chasing the exact
/// per-platform figure.
const UDS_PATH_BYTE_LIMIT: usize = 100;

/// Fail loudly, naming the path and the limit, when `path` would not fit in a
/// `sockaddr_un` on either Linux or macOS. The kernel would otherwise accept
/// only the bind call's first N bytes, corrupting the path silently; a long
/// `OMNIFS_HOME` must be shortened by the operator, not truncated for them.
fn check_uds_path_length(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let len = path.as_os_str().as_bytes().len();
    anyhow::ensure!(
        len < UDS_PATH_BYTE_LIMIT,
        "attach socket path {} is {len} bytes, at or beyond the {UDS_PATH_BYTE_LIMIT}-byte \
         sockaddr_un budget (Linux allows 108, macOS 104); shorten OMNIFS_HOME or move it closer \
         to the filesystem root",
        path.display()
    );
    Ok(())
}

#[derive(Debug)]
pub(crate) struct DaemonContext {
    layout: WorkspaceLayout,
    /// Optional debug/test TCP control listener beside the always-on Unix socket.
    listen: Option<SocketAddr>,
    /// Random per-start id reported in status and written to the runtime record.
    instance_id: String,
    /// `--attach-tcp <port>`: bind a TCP namespace attach listener eagerly at
    /// start (`0` = ephemeral). `None` when the flag was not passed; a TCP
    /// attach listener can still be bound later via `POST /v1/frontend/attach-target`.
    attach_tcp: Option<u16>,
    process: ProcessInfo,
}

/// One bound namespace attach socket: its name, its on-disk path (for cleanup on
/// exit), and the listener the daemon serves the wire over.
pub(crate) struct AttachSocket {
    pub path: PathBuf,
    pub listener: UnixListener,
}

#[derive(Debug)]
struct ProcessInfo {
    pid: u32,
    executable: PathBuf,
}

impl DaemonContext {
    pub(crate) fn resolve(args: &DaemonArgs) -> anyhow::Result<Self> {
        let listen = args.listen;
        let attach_tcp = args.attach_tcp;
        let workspace: Workspace<Daemon> = Workspace::resolve()?;
        let layout = workspace.into_layout();
        let process = ProcessInfo::current();

        Ok(Self {
            layout,
            listen,
            instance_id: generate_instance_id(),
            attach_tcp,
            process,
        })
    }

    pub(crate) fn prepare_startup_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.layout.config_dir)?;
        std::fs::create_dir_all(&self.layout.cache_dir)?;
        Ok(())
    }

    pub(crate) fn control_socket(&self) -> PathBuf {
        self.layout.control_socket()
    }

    pub(crate) fn runtime_record_file(&self) -> PathBuf {
        self.layout.runtime_record_file()
    }

    pub(crate) fn vsock_attach_socket(&self) -> PathBuf {
        self.layout.vsock_attach_socket()
    }

    pub(crate) fn bind_control_listener(&self) -> anyhow::Result<Option<TcpListener>> {
        self.listen
            .map(|addr| {
                TcpListener::bind(addr).map_err(|error| {
                    anyhow::anyhow!(
                        "cannot bind control API listener on {addr}: {error}\n\
                         \n\
                         Likely cause: another omnifs daemon is already running on that port.\n\
                         Run `omnifs down` to stop it, then try again."
                    )
                })
            })
            .transpose()
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
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(&path).with_context(|| {
                        format!("remove stale control socket {}", path.display())
                    })?;
                },
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("probe control socket {}", path.display()));
                },
            }
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind control socket {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict control socket {} to 0600", path.display()))?;
        Ok(listener)
    }

    /// Assemble the initial runtime record for a host-native daemon.
    pub(crate) fn runtime_record(&self) -> RuntimeRecord {
        RuntimeRecord::new(
            Endpoint::Unix {
                path: self.control_socket(),
            },
            RecordedBackend::Native {
                pid: self.process.pid,
            },
            self.instance_id.clone(),
            Vec::new(),
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

    /// This daemon start's instance id, reported in status, the runtime record,
    /// and the Omnifs VFS wire protocol handshake.
    pub(crate) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The `--attach-tcp` port request, if the flag was passed. `Some(0)` asks
    /// for an ephemeral port.
    pub(crate) fn attach_tcp_port(&self) -> Option<u16> {
        self.attach_tcp
    }

    /// Bind `frontends/local.sock`. Mirrors
    /// [`bind_control_socket`](Self::bind_control_socket): the
    /// `frontends/` dir is forced to `0700` and each socket to `0600`, and a
    /// stale socket (a refused connect probe) is unlinked and rebound. A live
    /// socket means another daemon owns this workspace, a hard error.
    pub(crate) fn bind_local_attach_socket(&self) -> anyhow::Result<AttachSocket> {
        let dir = self.layout.frontends_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create attach socket dir {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict attach socket dir {} to 0700", dir.display()))?;

        let path = self.layout.local_attach_socket();
        let listener = Self::bind_attach_socket_at(&path, "local attach socket")?;
        Ok(AttachSocket { path, listener })
    }

    /// Bind the token-checking UDS namespace attach listener at
    /// `frontends/vsock-attach.sock` for the krunkit vsock-proxy path (see
    /// [`crate::server::Daemon::ensure_attach_uds`]). Mirrors
    /// [`bind_local_attach_socket`](Self::bind_local_attach_socket): the `frontends/` dir
    /// is forced to `0700` and the socket to `0600`, and a stale socket (a
    /// refused connect probe) is unlinked and rebound before the fresh one is
    /// created. Unlike that listener, this path is fixed rather than
    /// caller-named, so it also guards the `sockaddr_un` byte budget: a path
    /// that would not fit fails loudly instead of being silently truncated by
    /// the kernel.
    pub(crate) fn bind_vsock_attach_socket(&self) -> anyhow::Result<(UnixListener, PathBuf)> {
        let dir = self.layout.frontends_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create attach socket dir {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict attach socket dir {} to 0700", dir.display()))?;

        let path = self.layout.vsock_attach_socket();
        let listener = Self::bind_attach_socket_at(&path, "vsock attach socket")?;
        Ok((listener, path))
    }

    fn bind_attach_socket_at(path: &Path, description: &str) -> anyhow::Result<UnixListener> {
        check_uds_path_length(path)?;
        if path.exists() {
            match UnixStream::connect(path) {
                Ok(_) => anyhow::bail!(
                    "another omnifs daemon is already serving {description} {}.\n\
                     Run `omnifs down` to stop it, then try again.",
                    path.display()
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path).with_context(|| {
                        format!("remove stale {description} {}", path.display())
                    })?;
                },
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("probe existing {description} {}", path.display())
                    });
                },
            }
        }
        let listener = UnixListener::bind(path)
            .with_context(|| format!("bind {description} {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict {description} {} to 0600", path.display()))?;
        Ok(listener)
    }

    /// Build status from the live attach registry. The compatibility
    /// `mount_point` field is derived from the first local attachment.
    pub(crate) fn status(
        &self,
        attach_serving: bool,
        frontends: Vec<FrontendInfo>,
        mounts: Vec<MountInfo>,
        failed: Vec<MountFailure>,
        credential_degraded: &[(String, String)],
    ) -> DaemonStatus {
        let health = self.health(
            attach_serving,
            &frontends,
            &mounts,
            &failed,
            credential_degraded,
        );
        let mount_point = frontends
            .iter()
            .find(|frontend| frontend.delivery == omnifs_api::FrontendDelivery::Local)
            .map_or_else(PathBuf::new, |frontend| frontend.mount_point.clone());
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            api_major: API_MAJOR,
            api_minor: API_MINOR,
            pid: self.process.pid,
            instance_id: self.instance_id.clone(),
            executable: self.process.executable.clone(),
            mount_point,
            config_dir: self.layout.config_dir.clone(),
            cache_dir: self.layout.cache_dir.clone(),
            providers_dir: self.layout.providers_dir.clone(),
            frontends,
            backend: DaemonBackend::Native {
                pid: self.process.pid,
            },
            mounts,
            failed,
            health,
        }
    }

    fn health(
        &self,
        attach_serving: bool,
        frontends: &[FrontendInfo],
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
                format!("native daemon pid {}", self.process.pid),
            ),
            Self::frontend_health(attach_serving, frontends),
            mount_health(mounts, failed, credential_degraded),
        ])
    }

    /// Listener readiness is independent of whether a frontend is currently
    /// attached. Startup flips this subsystem healthy only after reconcile and
    /// every requested listener bind have completed.
    fn frontend_health(attach_serving: bool, frontends: &[FrontendInfo]) -> SubsystemHealth {
        let mut listed = vec!["attach socket local".to_string()];
        listed.extend(frontends.iter().map(|frontend| {
            format!(
                "attached {} at {} via {}",
                frontend.fs_type,
                frontend.mount_point.display(),
                frontend.delivery
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
