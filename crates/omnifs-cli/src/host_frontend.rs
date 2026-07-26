//! Host frontend launch, observation, and control.

use anyhow::{Context as _, Result, ensure};
use omnifs_api::FsType;
use omnifs_mtab::{RunnerLocationClaim, RunnerRecord};
use omnifs_thin::host_control::{
    RunnerControlClient, RunnerPhase, StopOutcome, control_socket_for,
};
use omnifs_workspace::FrontendState;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const STOP_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub(crate) enum HostObservation {
    Runner {
        state_dir: PathBuf,
        record: RunnerRecord,
        confirmed: Result<RunnerPhase, String>,
    },
    Invalid {
        state_dir: PathBuf,
        error: String,
    },
}

pub(crate) fn probe(filesystem: FsType) -> Result<()> {
    if filesystem == FsType::Fuse && !Path::new("/dev/fuse").exists() {
        anyhow::bail!("/dev/fuse is not available on this host");
    }
    Ok(())
}

pub(crate) async fn is_running(
    frontend: &FrontendState,
    filesystem: FsType,
    mount_point: &Path,
) -> Result<bool> {
    let state_dir = frontend.state_dir(mount_point);
    let Some(record) = RunnerRecord::read(&state_dir)? else {
        if omnifs_nfs::mount_is_active_checked(mount_point)? {
            anyhow::bail!(
                "active host frontend mount {} has no runner record; run `omnifs doctor`",
                mount_point.display()
            );
        }
        return Ok(false);
    };
    validate_record(&record, filesystem, mount_point)?;
    RunnerControlClient::new(&record)
        .ping()
        .await
        .with_context(|| {
            format!(
                "host frontend at {} could not confirm runner {}; run `omnifs doctor`",
                mount_point.display(),
                record.instance_id
            )
        })?;
    Ok(true)
}

pub(crate) async fn launch(
    frontend: &FrontendState,
    mount_point: PathBuf,
    filesystem: FsType,
) -> Result<()> {
    probe(filesystem)?;
    let state_dir = frontend.state_dir(&mount_point);
    let instance_id = new_instance_id()?;
    let control_socket = control_socket_for(&state_dir, &instance_id);
    let executable = std::env::current_exe().context("resolve the omnifs executable")?;
    let log_path = frontend.host_log(filesystem);
    let log_parent = log_path
        .parent()
        .context("frontend log path has no parent directory")?;
    std::fs::create_dir_all(log_parent)
        .with_context(|| format!("create {}", log_parent.display()))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open frontend log {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("clone frontend log {}", log_path.display()))?;

    let mut command = Command::new(&executable);
    command
        .arg("run-frontend")
        .arg(filesystem.as_str())
        .arg("--runtime")
        .arg("host")
        .arg("--mount-point")
        .arg(&mount_point)
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--attach")
        .arg(frontend.attach_socket())
        .arg("--runner-instance")
        .arg(&instance_id)
        .arg("--runner-control")
        .arg(&control_socket)
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
            "start host {} frontend with {}",
            filesystem,
            executable.display()
        )
    })?;

    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    let mut last_phase = None;
    loop {
        if let Some(status) = child.try_wait().context("inspect host frontend process")? {
            anyhow::bail!(
                "host {filesystem} frontend exited with {status}; see {}",
                log_path.display()
            );
        }
        match RunnerRecord::read(&state_dir) {
            Ok(Some(record)) if record.instance_id == instance_id => {
                validate_record(&record, filesystem, &mount_point)?;
                if let Ok(state) = RunnerControlClient::new(&record).ping().await {
                    last_phase = Some(state.phase.clone());
                    match state.phase {
                        RunnerPhase::Mounted => return Ok(()),
                        RunnerPhase::Failed { message } => anyhow::bail!(
                            "host {filesystem} frontend failed: {message}; see {}",
                            log_path.display()
                        ),
                        RunnerPhase::Preflight
                        | RunnerPhase::Attaching
                        | RunnerPhase::Mounting
                        | RunnerPhase::Stopping
                        | RunnerPhase::Busy => {},
                    }
                }
            },
            Ok(Some(record)) => anyhow::bail!(
                "host frontend state at {} belongs to runner {}; run `omnifs doctor`",
                state_dir.display(),
                record.instance_id
            ),
            Ok(None) => {},
            Err(error) => return Err(error.into()),
        }
        if tokio::time::Instant::now() >= deadline {
            let phase = phase_label(last_phase);
            anyhow::bail!(
                "host {filesystem} frontend did not confirm mount startup within {}s; \
                 last proved phase was {phase}; runner {} was left alive for safe cleanup; see {}",
                STARTUP_TIMEOUT.as_secs(),
                instance_id,
                log_path.display()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn phase_label(phase: Option<RunnerPhase>) -> String {
    phase.map_or_else(|| "unconfirmed".to_owned(), |phase| format!("{phase:?}"))
}

pub(crate) async fn stop(
    frontend: &FrontendState,
    filesystem: FsType,
    mount_point: &Path,
) -> Result<()> {
    let state_dir = frontend.state_dir(mount_point);
    let Some(record) = RunnerRecord::read(&state_dir)? else {
        ensure!(
            !omnifs_nfs::mount_is_active_checked(mount_point)?,
            "active host frontend mount {} has no runner record; run `omnifs doctor`",
            mount_point.display()
        );
        return Ok(());
    };
    validate_record(&record, filesystem, mount_point)?;
    let (_, outcome) = RunnerControlClient::new(&record).stop().await?;
    match outcome {
        StopOutcome::Stopped => {},
        StopOutcome::Busy { message } | StopOutcome::Failed { message } => {
            anyhow::bail!("{message}")
        },
    }

    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    loop {
        let record_gone = RunnerRecord::read(&state_dir)?.is_none();
        let mount_gone = !omnifs_nfs::mount_is_active(mount_point);
        if record_gone && mount_gone {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "host frontend reported stopped but cleanup at {} did not finish within {}s",
                mount_point.display(),
                STOP_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(crate) async fn observe(frontend: &FrontendState) -> Result<Vec<HostObservation>> {
    let mut observations = Vec::new();
    let entries = match std::fs::read_dir(frontend.state_root()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(observations),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let state_dir = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                observations.push(HostObservation::Invalid {
                    state_dir: frontend.state_root().to_path_buf(),
                    error: error.to_string(),
                });
                continue;
            },
        };
        let record = match RunnerRecord::read(&state_dir) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                observations.push(HostObservation::Invalid {
                    state_dir,
                    error: error.to_string(),
                });
                continue;
            },
        };
        let confirmed = RunnerControlClient::new(&record)
            .ping()
            .await
            .map(|state| state.phase)
            .map_err(|error| error.to_string());
        observations.push(HostObservation::Runner {
            state_dir,
            record,
            confirmed,
        });
    }
    Ok(observations)
}

pub(crate) async fn stop_confirmed(state_dir: &Path, expected_instance: &str) -> Result<()> {
    let record = RunnerRecord::read(state_dir)?
        .context("runner record disappeared before confirmed stop")?;
    ensure!(
        record.instance_id == expected_instance,
        "runner instance changed before confirmed stop"
    );
    let client = RunnerControlClient::new(&record);
    client.ping().await.context("reconfirm host frontend")?;
    let (_, outcome) = client.stop().await?;
    match outcome {
        StopOutcome::Stopped => Ok(()),
        StopOutcome::Busy { message } | StopOutcome::Failed { message } => {
            anyhow::bail!("{message}")
        },
    }
}

pub(crate) async fn cleanup_stale(state_dir: &Path, expected_instance: &str) -> Result<()> {
    let _claim = RunnerLocationClaim::acquire(state_dir)?;
    let record =
        RunnerRecord::read(state_dir)?.context("runner record disappeared before stale cleanup")?;
    ensure!(
        record.instance_id == expected_instance,
        "runner instance changed before stale cleanup"
    );
    ensure!(
        RunnerControlClient::new(&record).ping().await.is_err(),
        "runner became reachable before stale cleanup"
    );
    ensure!(
        !omnifs_nfs::mount_is_active_checked(&record.mount_point)?,
        "mount became active before stale cleanup"
    );
    ensure!(
        !omnifs_mtab::process_group_exists(record.process_group)?,
        "recorded process group {} still exists; refusing stale cleanup",
        record.process_group
    );
    match std::fs::remove_file(state_dir.join("runner.json")) {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }
    match std::fs::remove_file(&record.control_socket) {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn validate_record(record: &RunnerRecord, filesystem: FsType, mount_point: &Path) -> Result<()> {
    ensure!(
        record.filesystem == filesystem && record.mount_point == mount_point,
        "runner record does not match requested {filesystem} frontend at {}",
        mount_point.display()
    );
    Ok(())
}

fn new_instance_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate host frontend instance id")?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::Workspace;

    #[tokio::test]
    async fn corrupt_runner_leaf_does_not_hide_a_valid_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::under_root(temp.path());
        let frontend = workspace.frontend();
        let valid_dir = frontend.state_dir(Path::new("/mnt/valid"));
        let invalid_dir = frontend.state_dir(Path::new("/mnt/invalid"));
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::create_dir_all(&invalid_dir).unwrap();
        let record = RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            pid: 42,
            process_group: 42,
            filesystem: FsType::Nfs,
            mount_point: PathBuf::from("/mnt/valid"),
            control_socket: valid_dir.join("control.sock"),
        };
        std::fs::write(
            valid_dir.join("runner.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        std::fs::write(invalid_dir.join("runner.json"), b"{broken").unwrap();

        let observations = observe(frontend).await.unwrap();
        assert_eq!(observations.len(), 2);
        assert!(observations.iter().any(|observation| matches!(
            observation,
            HostObservation::Runner { record, .. }
                if record.mount_point == Path::new("/mnt/valid")
        )));
        assert!(observations.iter().any(|observation| matches!(
            observation,
            HostObservation::Invalid { state_dir, .. } if state_dir == &invalid_dir
        )));
    }
}
