//! Local frontend runner lifecycle.

use std::fmt;
use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use omnifs_mtab::{MountKind, MountState};
use omnifs_workspace::daemon_record::FrontendKind;
use omnifs_workspace::layout::WorkspaceLayout;

const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const MOUNT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const THIN_RUNNER_NAME: &str = "omnifs-thin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalProtocol {
    Fuse,
    Nfs,
}

impl LocalProtocol {
    const fn subcommand(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }

    pub(crate) const fn kind(self) -> FrontendKind {
        match self {
            Self::Fuse => FrontendKind::Fuse,
            Self::Nfs => FrontendKind::Nfs,
        }
    }

    fn matches_child(self, state: &MountState, mount_point: &Path, pid: u32) -> bool {
        if state.mount_point != mount_point || state.pid != pid {
            return false;
        }
        matches!(
            (self, &state.kind),
            (Self::Fuse, MountKind::Fuse) | (Self::Nfs, MountKind::Nfs { .. })
        )
    }

    fn runner_beside(current_exe: &Path) -> Result<PathBuf> {
        Ok(current_exe
            .parent()
            .context("the omnifs executable has no parent directory")?
            .join(THIN_RUNNER_NAME))
    }
}

impl From<FrontendKind> for LocalProtocol {
    fn from(kind: FrontendKind) -> Self {
        match kind {
            FrontendKind::Fuse => Self::Fuse,
            FrontendKind::Nfs => Self::Nfs,
        }
    }
}

impl fmt::Display for LocalProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Fuse => "FUSE",
            Self::Nfs => "NFS",
        })
    }
}

pub(crate) struct HostRunner {
    paths: WorkspaceLayout,
    mount_point: PathBuf,
    protocol: LocalProtocol,
    runner: PathBuf,
}

impl HostRunner {
    pub(crate) fn new(
        paths: WorkspaceLayout,
        mount_point: PathBuf,
        protocol: LocalProtocol,
    ) -> Result<Self> {
        let current_exe = std::env::current_exe().context("resolve the omnifs executable")?;
        let runner = LocalProtocol::runner_beside(&current_exe)?;
        if !runner.is_file() {
            anyhow::bail!(
                "local {protocol} thin runner not found at {}; install it beside {}",
                runner.display(),
                current_exe.display()
            );
        }
        Ok(Self {
            paths,
            mount_point,
            protocol,
            runner,
        })
    }

    pub(crate) async fn launch(&self, mount_name: Option<&str>) -> Result<()> {
        std::fs::create_dir_all(&self.mount_point)
            .with_context(|| format!("create mount point {}", self.mount_point.display()))?;
        if omnifs_nfs::mount_is_active_checked(&self.mount_point)
            .with_context(|| format!("inspect mount point {}", self.mount_point.display()))?
        {
            self.validate_active_mount_recovery()?;
        }
        std::fs::create_dir_all(&self.paths.cache_dir)
            .with_context(|| format!("create {}", self.paths.cache_dir.display()))?;
        let log_path = self
            .paths
            .cache_dir
            .join(format!("frontend-{}.log", self.protocol.subcommand()));
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open frontend log {}", log_path.display()))?;
        let stderr = log
            .try_clone()
            .with_context(|| format!("clone frontend log {}", log_path.display()))?;

        let mut command = self.runner_command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "start local {} frontend with {}",
                self.protocol,
                self.runner.display()
            )
        })?;
        self.wait_until_mounted(&mut child, mount_name, &log_path)
            .await
    }

    fn runner_command(&self) -> Command {
        let mut command = Command::new(&self.runner);
        command
            .arg(self.protocol.subcommand())
            .arg("--mount-point")
            .arg(&self.mount_point)
            .arg("--state-dir")
            .arg(self.state_dir())
            .arg("--attach")
            .arg(self.paths.local_attach_socket());
        command
    }

    fn state_dir(&self) -> PathBuf {
        self.paths
            .frontend_state_dir(self.protocol.kind(), &self.mount_point)
    }

    fn validate_active_mount_recovery(&self) -> Result<()> {
        if self.protocol != LocalProtocol::Nfs || !omnifs_nfs::mount_is_omnifs(&self.mount_point) {
            anyhow::bail!(
                "refusing to start a local frontend: {} is already mounted",
                self.mount_point.display()
            );
        }
        self.persisted_nfs_addr()?;
        Ok(())
    }

    fn persisted_nfs_addr(&self) -> Result<SocketAddr> {
        MountState::read_unique(&self.state_dir())
            .and_then(|state| state.nfs_addr_for(&self.mount_point))
            .with_context(|| {
                format!(
                    "refusing to recover active NFS mount {} without its unique typed state",
                    self.mount_point.display()
                )
            })
    }

    fn direct_child_owns_mount(&self, pid: u32) -> bool {
        MountState::read_unique(&self.state_dir()).is_ok_and(|state| {
            self.protocol.matches_child(&state, &self.mount_point, pid)
                && crate::host_teardown::local_mount_is_owned(&state)
        })
    }

    async fn wait_until_mounted(
        &self,
        child: &mut Child,
        mount_name: Option<&str>,
        log_path: &Path,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + MOUNT_TIMEOUT;
        loop {
            if omnifs_nfs::mount_is_active(&self.mount_point) {
                let probe = mount_name.map_or_else(
                    || self.mount_point.clone(),
                    |name| self.mount_point.join(name),
                );
                match probe.try_exists() {
                    Ok(true) => return Ok(()),
                    Ok(false) => {},
                    Err(error) => {
                        self.terminate(child).await;
                        return Err(error).with_context(|| {
                            format!("probe projected mount path {}", probe.display())
                        });
                    },
                }
            }
            let status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    self.terminate(child).await;
                    return Err(error).context("inspect local frontend process");
                },
            };
            if let Some(status) = status {
                anyhow::bail!(
                    "local {} frontend exited with {status}; see {}",
                    self.protocol,
                    log_path.display()
                );
            }
            if tokio::time::Instant::now() >= deadline {
                self.terminate(child).await;
                anyhow::bail!(
                    "local {} frontend did not mount {} within {}s; see {}",
                    self.protocol,
                    self.mount_point.display(),
                    MOUNT_TIMEOUT.as_secs(),
                    log_path.display()
                );
            }
            tokio::time::sleep(MOUNT_POLL_INTERVAL).await;
        }
    }

    async fn terminate(&self, child: &mut Child) {
        #[cfg(unix)]
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status();
        #[cfg(not(unix))]
        let _ = child.kill();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while tokio::time::Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            tokio::time::sleep(MOUNT_POLL_INTERVAL).await;
        }
        if matches!(child.try_wait(), Ok(None)) && self.direct_child_owns_mount(child.id()) {
            let _ = omnifs_mtab::UnmountCommand::forced(
                omnifs_mtab::Platform::current(),
                &self.mount_point,
            )
            .run_quiet();
            let _ = crate::host_teardown::poll_until_unmounted(
                &self.mount_point,
                MOUNT_POLL_INTERVAL,
                30,
            );
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_is_resolved_only_beside_current_executable() {
        let path = LocalProtocol::runner_beside(Path::new("/opt/omnifs/bin/omnifs")).unwrap();
        assert_eq!(path, Path::new("/opt/omnifs/bin").join(THIN_RUNNER_NAME));
    }

    #[test]
    fn runner_argv_names_local_mount_state_and_attach_paths() {
        let paths = WorkspaceLayout::under_root(Path::new("/home/user/.omnifs"));
        for protocol in [LocalProtocol::Fuse, LocalProtocol::Nfs] {
            let runner = HostRunner {
                runner: PathBuf::from(THIN_RUNNER_NAME),
                paths: paths.clone(),
                mount_point: PathBuf::from("/home/user/omnifs"),
                protocol,
            };
            let command = runner.runner_command();
            let args = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(args[0], protocol.subcommand());
            assert_eq!(
                args[1..4],
                ["--mount-point", "/home/user/omnifs", "--state-dir"]
            );
            assert_eq!(Path::new(&args[4]), runner.state_dir());
            assert_eq!(
                args[5..],
                ["--attach", "/home/user/.omnifs/frontends/local.sock"]
            );
        }
    }

    #[test]
    fn both_protocols_use_one_thin_runner_with_distinct_subcommands() {
        let paths = WorkspaceLayout::under_root(Path::new("/home/user/.omnifs"));
        let runner = |protocol: LocalProtocol| HostRunner {
            runner: LocalProtocol::runner_beside(Path::new("/opt/omnifs/bin/omnifs")).unwrap(),
            paths: paths.clone(),
            mount_point: PathBuf::from("/home/user/omnifs"),
            protocol,
        };
        let fuse = runner(LocalProtocol::Fuse).runner_command();
        let nfs = runner(LocalProtocol::Nfs).runner_command();
        assert_eq!(fuse.get_program(), nfs.get_program());
        assert_eq!(fuse.get_args().next().unwrap(), "fuse");
        assert_eq!(nfs.get_args().next().unwrap(), "nfs");
    }

    #[test]
    fn state_dirs_are_stable_and_isolated_by_protocol_and_mount() {
        let paths = WorkspaceLayout::under_root(Path::new("/home/user/.omnifs"));
        let runner = |protocol: LocalProtocol, mount_point: &'static str| HostRunner {
            runner: PathBuf::from(THIN_RUNNER_NAME),
            paths: paths.clone(),
            mount_point: PathBuf::from(mount_point),
            protocol,
        };
        let first = runner(LocalProtocol::Nfs, "/mnt/first");
        let same = runner(LocalProtocol::Nfs, "/mnt/first");
        let normalized_same = runner(LocalProtocol::Nfs, "/mnt/./first/");
        let other = runner(LocalProtocol::Nfs, "/mnt/other");
        let fuse = runner(LocalProtocol::Fuse, "/mnt/first");

        assert_eq!(first.state_dir(), same.state_dir());
        assert_eq!(first.state_dir(), normalized_same.state_dir());
        assert_ne!(first.state_dir(), other.state_dir());
        assert_ne!(first.state_dir(), fuse.state_dir());
        assert!(first.state_dir().starts_with(paths.frontend_state_root()));
    }

    #[test]
    fn nfs_recovery_requires_unique_matching_nfs_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = WorkspaceLayout::under_root(tmp.path());
        let runner = HostRunner {
            runner: PathBuf::from("omnifs-thin"),
            paths,
            mount_point: PathBuf::from("/mnt/omnifs"),
            protocol: LocalProtocol::Nfs,
        };
        let addr: SocketAddr = "127.0.0.1:2049".parse().unwrap();
        let state =
            omnifs_mtab::StateFile::write_nfs(&runner.mount_point, addr, &runner.state_dir())
                .unwrap();
        assert_eq!(runner.persisted_nfs_addr().unwrap(), addr);

        drop(state);
        let wrong =
            omnifs_mtab::StateFile::write_fuse(&runner.mount_point, &runner.state_dir()).unwrap();
        assert!(runner.persisted_nfs_addr().is_err());
        drop(wrong);
    }

    #[test]
    fn rollback_ownership_requires_the_direct_child_and_typed_mount() {
        let state = MountState {
            version: MountState::VERSION,
            mount_point: PathBuf::from("/mnt/omnifs"),
            pid: 42,
            kind: MountKind::Nfs {
                addr: "127.0.0.1:2049".parse().unwrap(),
            },
        };
        assert!(LocalProtocol::Nfs.matches_child(&state, Path::new("/mnt/omnifs"), 42));
        assert!(!LocalProtocol::Nfs.matches_child(&state, Path::new("/mnt/omnifs"), 43));
        assert!(!LocalProtocol::Fuse.matches_child(&state, Path::new("/mnt/omnifs"), 42));
        assert!(!LocalProtocol::Nfs.matches_child(&state, Path::new("/mnt/other"), 42));
    }
}
