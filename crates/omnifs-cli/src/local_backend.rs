//! Local frontend runner lifecycle.

use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use omnifs_workspace::layout::WorkspaceLayout;

const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const MOUNT_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalProtocol {
    Fuse,
    Nfs,
}

impl LocalProtocol {
    pub(crate) const fn platform_default() -> Self {
        if cfg!(target_os = "linux") {
            Self::Fuse
        } else {
            Self::Nfs
        }
    }

    const fn binary_name(self) -> &'static str {
        match self {
            Self::Fuse => "omnifs-fuse",
            Self::Nfs => "omnifs-nfs",
        }
    }

    fn runner_beside(self, current_exe: &Path) -> Result<PathBuf> {
        Ok(current_exe
            .parent()
            .context("the omnifs executable has no parent directory")?
            .join(self.binary_name()))
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

pub(crate) struct LocalBackend {
    paths: WorkspaceLayout,
    mount_point: PathBuf,
    protocol: LocalProtocol,
    runner: PathBuf,
}

impl LocalBackend {
    pub(crate) fn new(
        paths: WorkspaceLayout,
        mount_point: PathBuf,
        protocol: LocalProtocol,
    ) -> Result<Self> {
        let current_exe = std::env::current_exe().context("resolve the omnifs executable")?;
        let runner = protocol.runner_beside(&current_exe)?;
        if !runner.is_file() {
            anyhow::bail!(
                "local {protocol} runner not found at {}; install it beside {}",
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

    pub(crate) const fn protocol(&self) -> LocalProtocol {
        self.protocol
    }

    pub(crate) async fn launch(&self, mount_name: &str) -> Result<()> {
        std::fs::create_dir_all(&self.mount_point)
            .with_context(|| format!("create mount point {}", self.mount_point.display()))?;
        if omnifs_nfs::mount_is_active(&self.mount_point) {
            anyhow::bail!(
                "refusing to start a local frontend: {} is already mounted",
                self.mount_point.display()
            );
        }
        std::fs::create_dir_all(&self.paths.cache_dir)
            .with_context(|| format!("create {}", self.paths.cache_dir.display()))?;
        let log_path = self.paths.cache_dir.join(format!(
            "frontend-{}.log",
            self.protocol.binary_name().trim_start_matches("omnifs-")
        ));
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
            .arg("--mount-point")
            .arg(&self.mount_point)
            .arg("--state-dir")
            .arg(self.paths.nfs_state_dir())
            .arg("--attach")
            .arg(self.paths.attach_socket("local"));
        command
    }

    async fn wait_until_mounted(
        &self,
        child: &mut Child,
        mount_name: &str,
        log_path: &Path,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + MOUNT_TIMEOUT;
        loop {
            if omnifs_nfs::mount_is_active(&self.mount_point) {
                let probe = self.mount_point.join(mount_name);
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
        if omnifs_nfs::mount_is_active(&self.mount_point) {
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
    fn platform_default_selects_a_packaged_runner() {
        let expected = if cfg!(target_os = "linux") {
            (LocalProtocol::Fuse, "omnifs-fuse")
        } else {
            (LocalProtocol::Nfs, "omnifs-nfs")
        };
        let protocol = LocalProtocol::platform_default();
        assert_eq!(protocol, expected.0);
        assert_eq!(protocol.binary_name(), expected.1);
    }

    #[test]
    fn runner_is_resolved_only_beside_current_executable() {
        let path = LocalProtocol::Fuse
            .runner_beside(Path::new("/opt/omnifs/bin/omnifs"))
            .unwrap();
        assert_eq!(path, Path::new("/opt/omnifs/bin/omnifs-fuse"));
    }

    #[test]
    fn runner_argv_names_local_mount_state_and_attach_paths() {
        let paths = WorkspaceLayout::under_root(Path::new("/home/user/.omnifs"));
        for protocol in [LocalProtocol::Fuse, LocalProtocol::Nfs] {
            let backend = LocalBackend {
                runner: PathBuf::from(protocol.binary_name()),
                paths: paths.clone(),
                mount_point: PathBuf::from("/home/user/omnifs"),
                protocol,
            };
            let command = backend.runner_command();
            let args = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(
                args,
                [
                    "--mount-point",
                    "/home/user/omnifs",
                    "--state-dir",
                    "/home/user/.omnifs/cache/nfs",
                    "--attach",
                    "/home/user/.omnifs/frontends/local.sock",
                ]
            );
        }
    }
}
