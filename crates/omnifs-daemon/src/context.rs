//! Daemon-owned startup and control-plane context.

use anyhow::Context as _;

use crate::app::{DaemonArgs, FrontendKind, FrontendMount};
use omnifs_api::{
    API_MAJOR, API_MINOR, DaemonBackend, DaemonHealth, DaemonStatus, DaemonSubsystem, FrontendInfo,
    HealthState, MountFailure, MountInfo, SubsystemHealth,
};
use omnifs_engine::HostContext;
use omnifs_nfs::NfsMountOptions;
use omnifs_workspace::layout::{Daemon, Workspace, WorkspaceLayout};
use omnifs_workspace::runtime_record::{
    Endpoint, FrontendKind as RecordFrontendKind, FrontendRecord, RecordedBackend, RuntimeRecord,
};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
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
    /// The requested frontend set, each with its own mount point. An absent
    /// `--frontend` flag resolves to the single platform default at the resolved
    /// mount point, unless attach sockets make this a namespace-only daemon with
    /// an empty set. The first entry is the primary, so a single-mount caller
    /// keeps today's behavior.
    frontends: Vec<FrontendMount>,
    /// Optional debug/test TCP control listener beside the always-on Unix socket.
    listen: Option<SocketAddr>,
    /// Random per-start id reported in status and written to the runtime record.
    instance_id: String,
    /// Requested namespace attach-socket names, each bound at
    /// `frontends/<name>.sock` and served over the shared namespace. Empty for a
    /// daemon that only mounts in-process frontends.
    attach_sockets: Vec<String>,
    /// `--attach-tcp <port>`: bind a TCP namespace attach listener eagerly at
    /// start (`0` = ephemeral). `None` when the flag was not passed; a TCP
    /// attach listener can still be bound later via `POST /v1/frontend/attach-target`.
    attach_tcp: Option<u16>,
    nfs: NfsContext,
    process: ProcessInfo,
}

/// One bound namespace attach socket: its name, its on-disk path (for cleanup on
/// exit), and the listener the daemon serves the wire over.
pub(crate) struct AttachSocket {
    pub name: String,
    pub path: PathBuf,
    pub listener: UnixListener,
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
        let DaemonArgs {
            nfs_port,
            nfs_state_dir,
            nfs_trace,
            listen,
            frontends,
            attach_sockets,
            attach_tcp,
        } = args;
        let workspace: Workspace<Daemon> = Workspace::resolve()?;
        let layout = workspace.into_layout();
        let attach_sockets = resolve_attach_sockets(attach_sockets)?;
        // A daemon asked for an attach socket but no `--frontend` serves the
        // namespace only: do not inject the platform default in-process frontend.
        let frontends = resolve_frontends(frontends, attach_sockets.is_empty())?;
        let process = ProcessInfo::current();
        let nfs = NfsContext {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), nfs_port),
            state_dir: nfs_state_dir.unwrap_or_else(|| layout.nfs_state_dir()),
            trace_path: nfs_trace,
        };

        Ok(Self {
            layout,
            frontends,
            listen,
            instance_id: generate_instance_id(),
            attach_sockets,
            attach_tcp,
            nfs,
            process,
        })
    }

    pub(crate) fn prepare_startup_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.layout.config_dir)?;
        for frontend in &self.frontends {
            std::fs::create_dir_all(&frontend.mount_point)?;
        }
        std::fs::create_dir_all(&self.layout.cache_dir)?;
        Ok(())
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
    /// the native backend pid, this build's instance id, and every served
    /// frontend at its mount point.
    pub(crate) fn runtime_record(&self) -> RuntimeRecord {
        let frontends = self
            .frontends
            .iter()
            .map(|frontend| FrontendRecord {
                kind: match frontend.kind {
                    FrontendKind::Fuse => RecordFrontendKind::Fuse,
                    FrontendKind::Nfs => RecordFrontendKind::Nfs,
                },
                mount_point: frontend.mount_point.clone(),
                // Every frontend a daemon builds from `--frontend` runs
                // in-process (host-native); `via` is only set for the
                // separately CLI-launched Docker frontend container.
                via: None,
            })
            .collect();
        RuntimeRecord::new(
            Endpoint::Unix {
                path: self.control_socket(),
            },
            RecordedBackend::Native {
                pid: self.process.pid,
            },
            self.instance_id.clone(),
            frontends,
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

    /// The primary (first) frontend's mount point. Status and the shutdown
    /// report key on this; a single-frontend daemon has exactly one.
    ///
    /// Smallest defensible interpretation for a namespace-only daemon (attach
    /// sockets, no in-process frontend): it has no OS mount point, so this
    /// returns the empty path rather than panicking. Status then reports an empty
    /// `mount_point`, which is truthful (nothing is mounted in-process).
    pub(crate) fn mount_point(&self) -> &Path {
        self.frontends
            .first()
            .map_or(Path::new(""), |frontend| frontend.mount_point.as_path())
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

    /// Bind every requested attach socket under `frontends/`, returning the bound
    /// listeners. Mirrors [`bind_control_socket`](Self::bind_control_socket): the
    /// `frontends/` dir is forced to `0700` and each socket to `0600`, and a
    /// stale socket (a refused connect probe) is unlinked and rebound. A live
    /// socket means another daemon owns this workspace, a hard error.
    pub(crate) fn bind_attach_sockets(&self) -> anyhow::Result<Vec<AttachSocket>> {
        if self.attach_sockets.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self.layout.frontends_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create attach socket dir {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict attach socket dir {} to 0700", dir.display()))?;

        let mut bound = Vec::with_capacity(self.attach_sockets.len());
        for name in &self.attach_sockets {
            let path = self.layout.attach_socket(name);
            if path.exists() {
                match UnixStream::connect(&path) {
                    Ok(_) => anyhow::bail!(
                        "another omnifs daemon is already serving attach socket {}.\n\
                         Run `omnifs down` to stop it, then try again.",
                        path.display()
                    ),
                    // Refused/ENOENT means the previous daemon is gone; unlink the
                    // leftover and rebind.
                    Err(_) => {
                        std::fs::remove_file(&path).with_context(|| {
                            format!("remove stale attach socket {}", path.display())
                        })?;
                    },
                }
            }
            let listener = UnixListener::bind(&path)
                .with_context(|| format!("bind attach socket {}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restrict attach socket {} to 0600", path.display()))?;
            bound.push(AttachSocket {
                name: name.clone(),
                path,
                listener,
            });
        }
        Ok(bound)
    }

    /// Bind the token-checking UDS namespace attach listener at
    /// `frontends/vsock-attach.sock` for the krunkit vsock-proxy path (see
    /// [`crate::server::Daemon::ensure_attach_uds`]). Mirrors
    /// [`bind_attach_sockets`](Self::bind_attach_sockets): the `frontends/` dir
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
        check_uds_path_length(&path)?;
        if path.exists() {
            match UnixStream::connect(&path) {
                Ok(_) => anyhow::bail!(
                    "another omnifs daemon is already serving the vsock attach socket {}.\n\
                     Run `omnifs down` to stop it, then try again.",
                    path.display()
                ),
                // Refused/ENOENT means the previous daemon is gone; unlink the
                // leftover and rebind.
                Err(_) => {
                    std::fs::remove_file(&path).with_context(|| {
                        format!("remove stale attach socket {}", path.display())
                    })?;
                },
            }
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind attach socket {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict attach socket {} to 0600", path.display()))?;
        Ok((listener, path))
    }

    /// Every requested frontend, in order. The registry builds one renderer per
    /// entry over one shared namespace.
    pub(crate) fn frontends(&self) -> &[FrontendMount] {
        &self.frontends
    }

    pub(crate) fn nfs_mount_options(&self) -> NfsMountOptions {
        let mut options = NfsMountOptions::loopback(self.nfs.state_dir.clone());
        options.bind = self.nfs.bind;
        options.trace_path.clone_from(&self.nfs.trace_path);
        options
    }

    /// `serving` is the subset of requested frontends currently present in the OS
    /// mount table. `frontend` (singular) is kept as the first served entry for
    /// pre-registry clients; `frontends` reports the whole served set.
    pub(crate) fn status(
        &self,
        serving: Vec<FrontendInfo>,
        attach_serving: bool,
        mounts: Vec<MountInfo>,
        failed: Vec<MountFailure>,
        credential_degraded: &[(String, String)],
    ) -> DaemonStatus {
        let health = self.health(
            &serving,
            attach_serving,
            &mounts,
            &failed,
            credential_degraded,
        );
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            api_major: API_MAJOR,
            api_minor: API_MINOR,
            pid: self.process.pid,
            instance_id: self.instance_id.clone(),
            executable: self.process.executable.clone(),
            mount_point: self.mount_point().to_path_buf(),
            config_dir: self.layout.config_dir.clone(),
            cache_dir: self.layout.cache_dir.clone(),
            providers_dir: self.layout.providers_dir.clone(),
            frontends: serving,
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
        serving: &[FrontendInfo],
        attach_serving: bool,
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
            self.frontend_health(serving, attach_serving),
            mount_health(mounts, failed, credential_degraded),
        ])
    }

    /// Frontend health over every requested *surface*: in-process frontends plus
    /// namespace attach sockets. `Healthy` only when every surface is up,
    /// `Degraded` when some but not all are, `Starting` when none are yet. A
    /// namespace-only daemon (attach sockets, no in-process mount) is `Healthy`
    /// once its sockets are serving, which is what `/v1/ready` gates on. The
    /// message lists the requested set so a partial outage names what is missing.
    fn frontend_health(&self, serving: &[FrontendInfo], attach_serving: bool) -> SubsystemHealth {
        let attach_requested = self.attach_sockets.len();
        let requested = self.frontends.len() + attach_requested;
        let attach_up = if attach_serving { attach_requested } else { 0 };
        let up = serving.len() + attach_up;

        let mut listed = self
            .frontends
            .iter()
            .map(|frontend| {
                format!(
                    "{} at {}",
                    frontend.kind.as_flag(),
                    frontend.mount_point.display()
                )
            })
            .collect::<Vec<_>>();
        listed.extend(
            self.attach_sockets
                .iter()
                .map(|name| format!("attach socket {name}")),
        );
        let listed = listed.join(", ");

        let (state, message) = if up == 0 {
            (HealthState::Starting, format!("not serving ({listed})"))
        } else if up < requested {
            (
                HealthState::Degraded,
                format!("{up}/{requested} surface(s) serving ({listed})"),
            )
        } else {
            (
                HealthState::Healthy,
                format!("{up}/{requested} surface(s) serving ({listed})"),
            )
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

/// Resolve the requested frontend set. An empty request injects the single
/// platform-default frontend at the resolved mount point (today's end-to-end
/// behavior) only when `inject_default` holds; a namespace-only daemon (attach
/// sockets, no `--frontend`) passes `false` and gets an empty set. A non-empty
/// request is served verbatim, after rejecting duplicate mount points and (off
/// Linux) the Linux-only FUSE frontend.
fn resolve_frontends(
    requested: Vec<FrontendMount>,
    inject_default: bool,
) -> anyhow::Result<Vec<FrontendMount>> {
    if requested.is_empty() {
        if !inject_default {
            return Ok(Vec::new());
        }
        let mount_point = omnifs_workspace::layout::resolve_mount_point().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve mount point: set HOME or OMNIFS_MOUNT_POINT")
        })?;
        return Ok(vec![FrontendMount {
            kind: FrontendKind::platform_default(),
            mount_point,
        }]);
    }

    let mut seen = std::collections::HashSet::new();
    for frontend in &requested {
        if !seen.insert(frontend.mount_point.as_path()) {
            anyhow::bail!(
                "duplicate frontend mount point {}",
                frontend.mount_point.display()
            );
        }
        #[cfg(not(target_os = "linux"))]
        if frontend.kind == FrontendKind::Fuse {
            anyhow::bail!("the fuse frontend is only available on Linux");
        }
    }
    Ok(requested)
}

/// Validate the requested attach-socket names: each is a bare `[a-z0-9-]+` label
/// (the CLI parser already enforces the charset; this rejects duplicates, which
/// would collide on one socket path).
fn resolve_attach_sockets(requested: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    for name in &requested {
        if !seen.insert(name.as_str()) {
            anyhow::bail!("duplicate attach socket name `{name}`");
        }
    }
    Ok(requested)
}

impl ProcessInfo {
    fn current() -> Self {
        Self {
            pid: std::process::id(),
            executable: std::env::current_exe().unwrap_or_else(|_| PathBuf::new()),
        }
    }
}
