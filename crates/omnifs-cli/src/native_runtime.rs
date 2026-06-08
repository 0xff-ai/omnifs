use anyhow::Context;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::paths::Paths;
use crate::runtime_target::NativeTarget;
use crate::session::Session;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const STARTUP_POLL: Duration = Duration::from_millis(250);

pub(crate) fn launch(
    paths: &Paths,
    target: &NativeTarget,
    session: &Session,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(target.mount_point())
        .with_context(|| format!("create mount point {}", target.mount_point().display()))?;
    std::fs::create_dir_all(&paths.cache_dir)
        .with_context(|| format!("create cache dir {}", paths.cache_dir.display()))?;
    std::fs::create_dir_all(state_dir(paths))
        .with_context(|| format!("create NFS state dir {}", state_dir(paths).display()))?;

    let log_path = log_path(paths);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open native runtime log {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("clone native runtime log {}", log_path.display()))?;

    let mut child = Command::new(std::env::current_exe().context("resolve current executable")?)
        .args([
            "daemon",
            "nfs-mount",
            "--mount-point",
            target.mount_point().to_string_lossy().as_ref(),
            "--config-dir",
            session.root().to_string_lossy().as_ref(),
            "--providers-dir",
            paths.providers_dir.to_string_lossy().as_ref(),
            "--cache-dir",
            paths.cache_dir.to_string_lossy().as_ref(),
            "--state-dir",
            state_dir(paths).to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("spawn native NFS daemon")?;

    wait_until_ready(paths, target.mount_point(), &mut child, &log_path)?;
    anstream::println!(
        "✓ {} is mounted with native NFS",
        target.mount_point().display()
    );
    anstream::println!("Runtime log: {}", log_path.display());
    Ok(())
}

pub(crate) fn down(paths: &Paths, target: &NativeTarget) -> anyhow::Result<bool> {
    let states = omnifs_nfs::read_mount_states(&state_dir(paths))?;
    let mount_point = target.mount_point();
    let should_unmount = states.iter().any(|state| state.mount_point == mount_point);
    if !should_unmount && !mount_is_active(mount_point) {
        return Ok(false);
    }
    omnifs_nfs::unmount(mount_point)?;
    Ok(true)
}

pub(crate) fn logs(paths: &Paths, follow: bool) -> anyhow::Result<()> {
    let log_path = log_path(paths);
    if follow {
        let status = Command::new("tail")
            .args(["-F", log_path.to_string_lossy().as_ref()])
            .status()
            .with_context(|| format!("tail {}", log_path.display()))?;
        if !status.success() {
            anyhow::bail!("tail exited with {status}");
        }
        return Ok(());
    }

    match std::fs::read_to_string(&log_path) {
        Ok(contents) => anstream::print!("{contents}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!(
                "native runtime log does not exist at {}",
                log_path.display()
            );
        },
        Err(error) => return Err(error).with_context(|| format!("read {}", log_path.display())),
    }
    Ok(())
}

pub(crate) fn state_dir(paths: &Paths) -> PathBuf {
    paths.config_dir.join("nfs")
}

pub(crate) fn log_path(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("native.log")
}

fn wait_until_ready(
    paths: &Paths,
    mount_point: &Path,
    child: &mut std::process::Child,
    log_path: &Path,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if mount_state_exists(paths, mount_point) && mount_is_active(mount_point) {
            return Ok(());
        }

        if let Some(status) = child.try_wait().context("poll native NFS daemon")? {
            anyhow::bail!(
                "native NFS daemon exited before mounting {} ({status}); see {}",
                mount_point.display(),
                log_path.display()
            );
        }

        if Instant::now() >= deadline {
            anyhow::bail!(
                "native NFS mount {} did not become ready within {}s; see {}",
                mount_point.display(),
                STARTUP_TIMEOUT.as_secs(),
                log_path.display()
            );
        }

        std::thread::sleep(STARTUP_POLL);
    }
}

fn mount_state_exists(paths: &Paths, mount_point: &Path) -> bool {
    omnifs_nfs::read_mount_states(&state_dir(paths)).is_ok_and(|states| {
        states
            .iter()
            .any(|state| state.mount_point.as_path() == mount_point)
    })
}

fn mount_is_active(mount_point: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/mounts").is_ok_and(|mounts| {
            mounts
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .any(|mounted| mounted == mount_point.to_string_lossy())
        })
    }

    #[cfg(target_os = "macos")]
    {
        let wanted = normalized_mount_point(mount_point);
        Command::new("mount").output().is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(macos_mount_point)
                .any(|mounted| normalized_mount_point(&mounted) == wanted)
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = mount_point;
        false
    }
}

#[cfg(target_os = "macos")]
fn macos_mount_point(line: &str) -> Option<PathBuf> {
    let (_, rest) = line.split_once(" on ")?;
    let (mount_point, _) = rest.rsplit_once(" (")?;
    Some(PathBuf::from(mount_point))
}

#[cfg(target_os = "macos")]
fn normalized_mount_point(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
