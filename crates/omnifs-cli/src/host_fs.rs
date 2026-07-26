//! Host filesystem launch, runner probing, and control.

use anyhow::{Context as _, Result, ensure};
use omnifs_core::fs;
use omnifs_mtab::{RunnerClaim, RunnerRecord};
use omnifs_thin::host_control::{
    RunnerControlClient, RunnerPhase, StopOutcome, control_socket_for,
};
use omnifs_workspace::FilesystemState;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const STOP_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub(crate) enum RunnerProbe {
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

pub(crate) fn probe(protocol: fs::Protocol) -> Result<()> {
    if protocol == fs::Protocol::Fuse && !Path::new("/dev/fuse").exists() {
        anyhow::bail!("/dev/fuse is not available on this host");
    }
    Ok(())
}

pub(crate) async fn phase(
    filesystem: &FilesystemState,
    spec: &fs::Spec,
) -> Result<Option<RunnerPhase>> {
    let mount_point = spec.location();
    let state_dir = filesystem.state_dir(spec.id());
    let Some(record) = RunnerRecord::read(&state_dir)? else {
        if omnifs_nfs::mount_is_active_checked(mount_point)? {
            anyhow::bail!(
                "active host filesystem mount {} has no runner record; run `omnifs doctor`",
                mount_point.display()
            );
        }
        return Ok(None);
    };
    validate_record(&record, spec)?;
    let state = RunnerControlClient::new(&record)
        .ping()
        .await
        .with_context(|| {
            format!(
                "host filesystem at {} could not confirm runner {}; run `omnifs doctor`",
                mount_point.display(),
                record.instance_id
            )
        })?;
    Ok(Some(state.phase))
}

pub(crate) async fn launch(filesystem: &FilesystemState, spec: &fs::Spec) -> Result<()> {
    probe(spec.protocol())?;
    PendingHostFilesystem::spawn(filesystem, spec)?
        .wait_until_mounted(spec)
        .await
}

struct PendingHostFilesystem {
    child: Child,
    state_dir: PathBuf,
    instance_id: String,
    log_path: PathBuf,
}

impl PendingHostFilesystem {
    fn spawn(filesystem: &FilesystemState, spec: &fs::Spec) -> Result<Self> {
        let state_dir = filesystem.state_dir(spec.id());
        let instance_id = new_instance_id()?;
        let control_socket = control_socket_for(&state_dir, &instance_id);
        let executable = std::env::current_exe().context("resolve the omnifs executable")?;
        let log_path = filesystem.host_log(spec.id());
        let log_parent = log_path
            .parent()
            .context("filesystem log path has no parent directory")?;
        std::fs::create_dir_all(log_parent)
            .with_context(|| format!("create {}", log_parent.display()))?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open filesystem log {}", log_path.display()))?;
        let stderr = log
            .try_clone()
            .with_context(|| format!("clone filesystem log {}", log_path.display()))?;

        let mut command = Command::new(&executable);
        command
            .arg("run-fs")
            .arg("--name")
            .arg(spec.id().as_str())
            .arg("--protocol")
            .arg(spec.protocol().as_str())
            .arg("--runtime")
            .arg("host")
            .arg("--location")
            .arg(spec.location())
            .arg("--state-dir")
            .arg(&state_dir)
            .arg("--attach")
            .arg(filesystem.attach_socket())
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
        let child = command.spawn().with_context(|| {
            format!(
                "start host {} filesystem with {}",
                spec.protocol(),
                executable.display()
            )
        })?;
        Ok(Self {
            child,
            state_dir,
            instance_id,
            log_path,
        })
    }

    async fn wait_until_mounted(mut self, spec: &fs::Spec) -> Result<()> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        let mut last_phase = None;
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .context("inspect host filesystem process")?
            {
                anyhow::bail!(
                    "host filesystem `{}` exited with {status}; see {}",
                    spec.id(),
                    self.log_path.display()
                );
            }
            match RunnerRecord::read(&self.state_dir) {
                Ok(Some(record)) if record.instance_id == self.instance_id => {
                    validate_record(&record, spec)?;
                    if let Ok(state) = RunnerControlClient::new(&record).ping().await {
                        last_phase = Some(state.phase.clone());
                        match state.phase {
                            RunnerPhase::Mounted => return Ok(()),
                            RunnerPhase::Failed { message } => anyhow::bail!(
                                "host filesystem `{}` failed: {message}; see {}",
                                spec.id(),
                                self.log_path.display()
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
                    "host filesystem state at {} belongs to runner {}; run `omnifs doctor`",
                    self.state_dir.display(),
                    record.instance_id
                ),
                Ok(None) => {},
                Err(error) => return Err(error.into()),
            }
            if tokio::time::Instant::now() >= deadline {
                let phase = phase_label(last_phase);
                anyhow::bail!(
                    "host filesystem `{}` did not confirm mount startup within {}s; \
                     last proved phase was {phase}; runner {} was left alive for safe cleanup; see {}",
                    spec.id(),
                    STARTUP_TIMEOUT.as_secs(),
                    self.instance_id,
                    self.log_path.display()
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

fn phase_label(phase: Option<RunnerPhase>) -> String {
    phase.map_or_else(|| "unconfirmed".to_owned(), |phase| format!("{phase:?}"))
}

pub(crate) async fn stop(filesystem: &FilesystemState, spec: &fs::Spec) -> Result<()> {
    let mount_point = spec.location();
    let state_dir = filesystem.state_dir(spec.id());
    let Some(record) = RunnerRecord::read(&state_dir)? else {
        ensure!(
            !omnifs_nfs::mount_is_active_checked(mount_point)?,
            "active host filesystem mount {} has no runner record; run `omnifs doctor`",
            mount_point.display()
        );
        return Ok(());
    };
    validate_record(&record, spec)?;
    let (_, outcome) = RunnerControlClient::new(&record).stop().await?;
    wait_for_cleanup(&state_dir, mount_point, outcome).await
}

async fn wait_for_cleanup(
    state_dir: &Path,
    mount_point: &Path,
    outcome: StopOutcome,
) -> Result<()> {
    let busy_message = match outcome {
        StopOutcome::Stopped => None,
        StopOutcome::Busy { message } => Some(message),
        StopOutcome::Failed { message } => anyhow::bail!("{message}"),
    };
    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    loop {
        let record_gone = RunnerRecord::read(state_dir)?.is_none();
        let mount_gone = !omnifs_nfs::mount_is_active(mount_point);
        if record_gone && mount_gone {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            if let Some(message) = busy_message {
                anyhow::bail!(
                    "{message}; cleanup did not finish within {}s",
                    STOP_TIMEOUT.as_secs()
                );
            }
            anyhow::bail!(
                "host filesystem reported stopped but cleanup at {} did not finish within {}s",
                mount_point.display(),
                STOP_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(crate) async fn probe_all(filesystem: &FilesystemState) -> Result<Vec<RunnerProbe>> {
    let mut probes = Vec::new();
    let entries = match std::fs::read_dir(filesystem.state_root()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(probes),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let state_dir = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                probes.push(RunnerProbe::Invalid {
                    state_dir: filesystem.state_root().to_path_buf(),
                    error: error.to_string(),
                });
                continue;
            },
        };
        let record = match RunnerRecord::read(&state_dir) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                probes.push(RunnerProbe::Invalid {
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
        probes.push(RunnerProbe::Runner {
            state_dir,
            record,
            confirmed,
        });
    }
    Ok(probes)
}

pub(crate) async fn stop_confirmed(state_dir: &Path, expected: &RunnerRecord) -> Result<()> {
    let record = RunnerRecord::read(state_dir)?
        .context("runner record disappeared before confirmed stop")?;
    ensure!(
        record == *expected,
        "runner identity changed before confirmed stop"
    );
    let client = RunnerControlClient::new(&record);
    client.ping().await.context("reconfirm host filesystem")?;
    let (_, outcome) = client.stop().await?;
    wait_for_cleanup(state_dir, record.spec.location(), outcome).await
}

pub(crate) async fn cleanup_stale(state_dir: &Path, expected: &RunnerRecord) -> Result<()> {
    let _claim = RunnerClaim::acquire(state_dir)?;
    let record =
        RunnerRecord::read(state_dir)?.context("runner record disappeared before stale cleanup")?;
    ensure!(
        record == *expected,
        "runner identity changed before stale cleanup"
    );
    ensure!(
        RunnerControlClient::new(&record).ping().await.is_err(),
        "runner became reachable before stale cleanup"
    );
    ensure!(
        !omnifs_nfs::mount_is_active_checked(record.spec.location())?,
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

fn validate_record(record: &RunnerRecord, spec: &fs::Spec) -> Result<()> {
    ensure!(
        record.spec == *spec,
        "runner record does not match configured filesystem `{}`",
        spec.id()
    );
    Ok(())
}

fn new_instance_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate host filesystem instance id")?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::Workspace;

    #[tokio::test]
    async fn busy_stop_waits_for_runner_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let mount_point = temp.path().join("mount");
        let record = RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            pid: 42,
            process_group: 42,
            spec: fs::Spec::new(
                fs::Id::new("main").unwrap(),
                fs::Protocol::Fuse,
                fs::Runtime::Host,
                mount_point.clone(),
            )
            .unwrap(),
            control_socket: state_dir.join("control.sock"),
        };
        std::fs::write(
            state_dir.join("runner.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let record_path = state_dir.join("runner.json");
        tokio::spawn(async move {
            tokio::time::sleep(POLL_INTERVAL).await;
            std::fs::remove_file(record_path).unwrap();
        });

        wait_for_cleanup(
            &state_dir,
            &mount_point,
            StopOutcome::Busy {
                message: "cleanup is still running".to_owned(),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn corrupt_runner_leaf_does_not_hide_a_valid_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::under_root(temp.path());
        let filesystem = workspace.filesystem_state();
        let valid_id = fs::Id::new("valid").unwrap();
        let invalid_id = fs::Id::new("invalid").unwrap();
        let valid_dir = filesystem.state_dir(&valid_id);
        let invalid_dir = filesystem.state_dir(&invalid_id);
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::create_dir_all(&invalid_dir).unwrap();
        let record = RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            pid: 42,
            process_group: 42,
            spec: fs::Spec::new(
                valid_id,
                fs::Protocol::Nfs,
                fs::Runtime::Host,
                PathBuf::from("/mnt/valid"),
            )
            .unwrap(),
            control_socket: valid_dir.join("control.sock"),
        };
        std::fs::write(
            valid_dir.join("runner.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        std::fs::write(invalid_dir.join("runner.json"), b"{broken").unwrap();

        let probes = probe_all(filesystem).await.unwrap();
        assert_eq!(probes.len(), 2);
        assert!(probes.iter().any(|probe| matches!(
            probe,
            RunnerProbe::Runner { record, .. }
                if record.spec.location() == Path::new("/mnt/valid")
        )));
        assert!(probes.iter().any(|probe| matches!(
            probe,
            RunnerProbe::Invalid { state_dir, .. } if state_dir == &invalid_dir
        )));
    }
}
