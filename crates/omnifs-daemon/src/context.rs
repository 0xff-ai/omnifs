//! Daemon-owned startup and control-plane context.

use crate::app::{DaemonArgs, FrontendKind};
use omnifs_api::{
    API_MAJOR, API_MINOR, DaemonBackend, DaemonHealth, DaemonStatus, DaemonSubsystem, FrontendInfo,
    HealthState, MountFailure, MountInfo, SubsystemHealth,
};
use omnifs_host::HostContext;
use omnifs_nfs::NfsMountOptions;
use omnifs_workspace::layout::{Workspace, WorkspaceLayout};
use omnifs_workspace::mounts::materialize::MaterializationMode;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct DaemonContext {
    layout: WorkspaceLayout,
    mount_point: PathBuf,
    frontend: FrontendKind,
    backend: DaemonBackend,
    root_symlinks: bool,
    listen: SocketAddr,
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
        let workspace = Workspace::resolve()?;
        let layout = workspace.into_layout();
        let frontend = FrontendKind::platform_default();
        let mount_point = omnifs_workspace::layout::resolve_mount_point().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve mount point: set HOME or OMNIFS_MOUNT_POINT")
        })?;
        let backend = if args.host_native {
            DaemonBackend::Native
        } else {
            DaemonBackend::Docker
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
            root_symlinks: args.root_symlinks,
            listen: args.listen,
            nfs,
            process: ProcessInfo::current(),
        })
    }

    pub(crate) fn prepare_startup_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.mount_point)?;
        std::fs::create_dir_all(&self.layout.cache_dir)?;
        Ok(())
    }

    pub(crate) fn bind_control_listener(&self) -> anyhow::Result<TcpListener> {
        TcpListener::bind(self.listen).map_err(|error| {
            anyhow::anyhow!(
                "cannot bind control API listener on {}: {error}\n\
                 \n\
                 Likely cause: another omnifs daemon is already running on that port.\n\
                 Run `omnifs down` to stop it, then try again.",
                self.listen
            )
        })
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
        match self.backend {
            DaemonBackend::Native => MaterializationMode::HostNative,
            DaemonBackend::Docker => MaterializationMode::Docker,
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
    ) -> DaemonStatus {
        let health = self.health(frontend.as_ref(), &mounts, &failed);
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            api_major: API_MAJOR,
            api_minor: API_MINOR,
            pid: self.process.pid,
            executable: self.process.executable.clone(),
            mount_point: self.mount_point.clone(),
            config_dir: self.layout.config_dir.clone(),
            cache_dir: self.layout.cache_dir.clone(),
            providers_dir: self.layout.providers_dir.clone(),
            frontend,
            backend: self.backend,
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
    ) -> DaemonHealth {
        DaemonHealth::new(vec![
            SubsystemHealth::new(
                DaemonSubsystem::Control,
                HealthState::Healthy,
                format!("control API serving on {}", self.listen),
            ),
            SubsystemHealth::new(
                DaemonSubsystem::Backend,
                HealthState::Healthy,
                match self.backend {
                    DaemonBackend::Native => "native daemon",
                    DaemonBackend::Docker => "docker container",
                },
            ),
            self.frontend_health(frontend),
            mount_health(mounts, failed),
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

fn mount_health(mounts: &[MountInfo], failed: &[MountFailure]) -> SubsystemHealth {
    let state = match (mounts.is_empty(), failed.is_empty()) {
        (_, true) => HealthState::Healthy,
        (false, false) => HealthState::Degraded,
        (true, false) => HealthState::Unhealthy,
    };
    let message = if failed.is_empty() {
        format!("{} mount(s) loaded", mounts.len())
    } else {
        format!("{} mount(s) loaded, {} failed", mounts.len(), failed.len())
    };
    SubsystemHealth::new(DaemonSubsystem::Mounts, state, message)
}

impl ProcessInfo {
    fn current() -> Self {
        Self {
            pid: std::process::id(),
            executable: std::env::current_exe().unwrap_or_else(|_| PathBuf::new()),
        }
    }
}
