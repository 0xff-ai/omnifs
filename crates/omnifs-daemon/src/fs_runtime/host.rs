//! Host filesystem launch, runner probing, and control.

use crate::fs_runtime::driver::LaunchRequest;
use crate::fs_runtime::identity::{ensure_identity_unchanged, ensure_record_matches};
use crate::fs_runtime::{Candidate, RuntimeEvent, RuntimeEventSink, RuntimeStage, RuntimeState};
use anyhow::{Context as _, Result, ensure};
use futures_util::stream::{self, StreamExt as _};
use omnifs_core::{FilesystemProtocol, FilesystemRuntime, FilesystemSpec, ResourceName};
use omnifs_mtab::{RunnerClaim, RunnerRecord};
use omnifs_thin::host_control::{
    RunnerControlClient, RunnerPhase, StopOutcome, control_socket_for,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const STOP_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const OWNERSHIP_PROBE_CONCURRENCY: usize = 8;

pub(crate) fn probe(protocol: FilesystemProtocol) -> Result<()> {
    if protocol == FilesystemProtocol::Fuse && !Path::new("/dev/fuse").exists() {
        anyhow::bail!("/dev/fuse is not available on this host");
    }
    Ok(())
}

/// The host filesystem runner: a durable per-filesystem state directory
/// holding the runner record and control socket. Unlike Docker or libkrun,
/// the host runner is a bare directory with no owned process handle between
/// calls, so every method rediscovers the runner process through its record.
pub struct HostDriver {
    state_dir: PathBuf,
    log_path: PathBuf,
    control_socket: Option<PathBuf>,
    executable: PathBuf,
    events: RuntimeEventSink,
}

impl HostDriver {
    #[must_use]
    pub fn new(
        state_dir: PathBuf,
        log_path: PathBuf,
        executable: PathBuf,
        events: RuntimeEventSink,
    ) -> Self {
        Self {
            state_dir,
            log_path,
            control_socket: None,
            executable,
            events,
        }
    }

    #[must_use]
    pub fn new_with_control_socket(
        state_dir: PathBuf,
        log_path: PathBuf,
        control_socket: PathBuf,
        executable: PathBuf,
        events: RuntimeEventSink,
    ) -> Self {
        Self {
            state_dir,
            log_path,
            control_socket: Some(control_socket),
            executable,
            events,
        }
    }

    /// Prove a live, identity-matched runner, returning its confirmed record
    /// and phase together so a caller that needs to act on the identity (for
    /// example, stop it) does not have to re-read the record from disk.
    pub async fn confirmed(
        &self,
        filesystem: &ResourceName,
        spec: &FilesystemSpec,
    ) -> Result<Option<(RunnerRecord, RunnerPhase)>> {
        let mount_point = spec.location();
        let Some(record) = RunnerRecord::read(&self.state_dir)? else {
            if omnifs_nfs::mount_is_active_checked(mount_point)? {
                return Err(anyhow::anyhow!(
                    "host filesystem state exists at {} without a runner record; run `omnifs \
                     doctor` to diagnose",
                    mount_point.display()
                ));
            }
            return Ok(None);
        };
        ensure_record_matches(&record.filesystem, &record.spec, filesystem, spec)?;
        let state = RunnerControlClient::new(&record)
            .ping()
            .await
            .with_context(|| {
                format!(
                    "host filesystem at {} could not confirm runner {}; run `omnifs doctor` to \
                     diagnose",
                    mount_point.display(),
                    record.instance_id
                )
            })?;
        Ok(Some((record, state.phase)))
    }

    pub(crate) async fn launch(&self, request: &LaunchRequest<'_>) -> Result<()> {
        probe(request.spec.protocol())?;
        self.events.emit(RuntimeEvent::Stage {
            stage: RuntimeStage::StartProcess,
            runtime: FilesystemRuntime::Host,
            filesystem: request.filesystem.clone(),
            state: RuntimeState::Active,
        });
        PendingHostFilesystem::spawn(PendingHostSpawn {
            state_dir: &self.state_dir,
            log_path: &self.log_path,
            executable: &self.executable,
            filesystem: request.filesystem,
            runtime_instance: request.runtime_instance,
            spec: request.spec,
            attach_socket: request.endpoints.attach_unix()?,
            control_socket: self.control_socket.as_deref(),
        })?
        .wait_until_mounted(request.filesystem, request.spec)
        .await?;
        self.events.emit(RuntimeEvent::MountReady {
            runtime: FilesystemRuntime::Host,
            filesystem: request.filesystem.clone(),
            location: request.spec.location().to_path_buf(),
            container: None,
        });
        Ok(())
    }

    /// The one teardown entry point for a proven identity: reconfirm the
    /// runner still matches `expected`, then request its stop and wait for
    /// cleanup. Callers without an already-confirmed record obtain one from
    /// [`Self::confirmed`] first; an identity that has already disappeared is a
    /// no-op there, not an error here.
    pub async fn stop_confirmed(&self, expected: &RunnerRecord) -> Result<()> {
        let Some(record) = RunnerRecord::read(&self.state_dir)? else {
            anyhow::ensure!(
                !omnifs_nfs::mount_is_active_checked(expected.spec.location())?,
                "runner record disappeared while its exact mount remained active"
            );
            return Ok(());
        };
        ensure_identity_unchanged(Some(&record), expected, "runner")?;
        let client = RunnerControlClient::new(&record);
        if let Err(error) = client.ping().await.context("reconfirm host filesystem") {
            return wait_for_cleanup(
                &self.state_dir,
                record.spec.location(),
                StopOutcome::Stopped,
            )
            .await
            .with_context(|| format!("{error:#}"));
        }
        let (_, outcome) = client.stop().await?;
        wait_for_cleanup(&self.state_dir, record.spec.location(), outcome).await
    }

    pub async fn cleanup_stale(&self, expected: &RunnerRecord) -> Result<()> {
        let _claim = RunnerClaim::acquire(&self.state_dir)?;
        let record = RunnerRecord::read(&self.state_dir)?
            .context("runner record disappeared before stale cleanup")?;
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
        match std::fs::remove_file(self.state_dir.join("runner.json")) {
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
}

struct PendingHostFilesystem {
    child: Option<Child>,
    state_dir: PathBuf,
    instance_id: String,
    log_path: PathBuf,
}

#[derive(Clone, Copy)]
struct PendingHostSpawn<'a> {
    state_dir: &'a Path,
    log_path: &'a Path,
    executable: &'a Path,
    filesystem: &'a ResourceName,
    runtime_instance: &'a str,
    spec: &'a FilesystemSpec,
    attach_socket: &'a Path,
    control_socket: Option<&'a Path>,
}

impl PendingHostFilesystem {
    fn spawn(request: PendingHostSpawn<'_>) -> Result<Self> {
        let PendingHostSpawn {
            state_dir,
            log_path,
            executable,
            filesystem,
            runtime_instance,
            spec,
            attach_socket,
            control_socket,
        } = request;
        let state_dir = state_dir.to_path_buf();
        let instance_id = runtime_instance.to_owned();
        let control_socket = control_socket.map_or_else(
            || control_socket_for(&state_dir, &instance_id),
            Path::to_path_buf,
        );
        if let Some(parent) = control_socket.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("restrict {} to 0700", parent.display()))?;
            }
        }
        let log_parent = log_path
            .parent()
            .context("filesystem log path has no parent directory")?;
        std::fs::create_dir_all(log_parent)
            .with_context(|| format!("create {}", log_parent.display()))?;

        let mut command = Command::new(executable);
        command
            .arg("run-fs")
            .arg("--name")
            .arg(filesystem.as_str())
            .arg("--protocol")
            .arg(spec.protocol().as_str())
            .arg("--runtime")
            .arg("host")
            .arg("--location")
            .arg(spec.location())
            .args(
                spec.docker_image()
                    .map(|image| ["--docker-image", image])
                    .into_iter()
                    .flatten(),
            )
            .args(
                spec.libkrun_guest_image()
                    .map(|image| ["--libkrun-guest-image", image])
                    .into_iter()
                    .flatten(),
            )
            .arg("--runtime-instance")
            .arg(runtime_instance)
            .arg("--state-dir")
            .arg(&state_dir)
            .arg("--attach")
            .arg(attach_socket)
            .arg("--runner-instance")
            .arg(&instance_id)
            .arg("--runner-control")
            .arg(&control_socket);
        crate::fs_runtime::process::configure_detached_child(
            &mut command,
            log_path,
            crate::fs_runtime::process::LogMode::Append,
        )?;
        let child = command.spawn().with_context(|| {
            format!(
                "start host {} filesystem with {}",
                spec.protocol(),
                executable.display()
            )
        })?;
        Ok(Self {
            child: Some(child),
            state_dir,
            instance_id,
            log_path: log_path.to_path_buf(),
        })
    }

    async fn wait_until_mounted(
        self,
        filesystem: &ResourceName,
        spec: &FilesystemSpec,
    ) -> Result<()> {
        // Bundled (including `spec`) so the check closure captures nothing
        // from the enclosing scope and takes everything by explicit `&mut`
        // argument instead: an `FnMut` closure cannot itself return a future
        // that borrows its own captured environment, but a fresh reborrow of
        // an argument can.
        struct Wait<'a> {
            pending: PendingHostFilesystem,
            filesystem: &'a ResourceName,
            spec: &'a FilesystemSpec,
            last_phase: Option<RunnerPhase>,
        }
        let mut wait = Wait {
            pending: self,
            filesystem,
            spec,
            last_phase: None,
        };
        let ready = crate::fs_runtime::process::poll_until_mut(
            STARTUP_TIMEOUT,
            POLL_INTERVAL,
            &mut wait,
            |wait| {
                Box::pin(async move {
                    if let Some(status) = wait
                        .pending
                        .child
                        .as_mut()
                        .context("host filesystem child identity was lost before readiness")?
                        .try_wait()
                        .context("inspect host filesystem process")?
                    {
                        return Err(anyhow::anyhow!(
                            "host filesystem `{}` exited with {status}; see {}",
                            wait.filesystem,
                            wait.pending.log_path.display()
                        ));
                    }
                    match RunnerRecord::read(&wait.pending.state_dir) {
                        Ok(Some(record)) if record.instance_id == wait.pending.instance_id => {
                            ensure_record_matches(
                                &record.filesystem,
                                &record.spec,
                                wait.filesystem,
                                wait.spec,
                            )?;
                            if let Ok(state) = RunnerControlClient::new(&record).ping().await {
                                wait.last_phase = Some(state.phase.clone());
                                match state.phase {
                                    RunnerPhase::Mounted => return Ok(Some(())),
                                    RunnerPhase::Failed { message } => {
                                        return Err(anyhow::anyhow!(
                                            "host filesystem `{}` failed: {message}; see {}",
                                            wait.filesystem,
                                            wait.pending.log_path.display()
                                        ));
                                    },
                                    RunnerPhase::Preflight
                                    | RunnerPhase::Attaching
                                    | RunnerPhase::Mounting
                                    | RunnerPhase::Stopping
                                    | RunnerPhase::Busy => {},
                                }
                            }
                        },
                        Ok(Some(record)) => {
                            return Err(anyhow::anyhow!(
                                "host filesystem state at {} belongs to runner {}; run `omnifs \
                                 doctor` to diagnose",
                                wait.pending.state_dir.display(),
                                record.instance_id
                            ));
                        },
                        Ok(None) => {},
                        Err(error) => return Err(error.into()),
                    }
                    Ok(None)
                })
            },
        )
        .await;
        let child = wait
            .pending
            .child
            .take()
            .context("host filesystem child identity was lost after readiness polling")?;
        crate::fs_runtime::process::reap_managed_child(child);
        ready?.map_or_else(
            || {
                Err(mount_startup_timeout(
                    filesystem,
                    wait.last_phase.clone(),
                    &wait.pending.instance_id,
                    &wait.pending.log_path,
                ))
            },
            Ok,
        )
    }
}

fn mount_startup_timeout(
    filesystem: &ResourceName,
    last_phase: Option<RunnerPhase>,
    instance_id: &str,
    log_path: &Path,
) -> anyhow::Error {
    let phase = phase_label(last_phase);
    anyhow::anyhow!(
        "host filesystem `{filesystem}` did not confirm mount startup within {}s; last proved \
         phase was {phase}; runner {instance_id} was left alive for safe cleanup; see {}",
        STARTUP_TIMEOUT.as_secs(),
        log_path.display(),
    )
}

fn phase_label(phase: Option<RunnerPhase>) -> String {
    phase.map_or_else(|| "unconfirmed".to_owned(), |phase| format!("{phase:?}"))
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
    crate::fs_runtime::process::poll_until(STOP_TIMEOUT, POLL_INTERVAL, || async {
        let record_gone = RunnerRecord::read(state_dir)?.is_none();
        let mount_gone = !omnifs_nfs::mount_is_active(mount_point);
        Ok((record_gone && mount_gone).then_some(()))
    })
    .await?
    .map_or_else(
        || {
            if let Some(message) = &busy_message {
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
        },
        Ok,
    )
}

pub(crate) async fn owned(state_root: &Path) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    let mut readable = Vec::new();
    let entries = match std::fs::read_dir(state_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidates),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let state_dir = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                candidates.push(Candidate::Invalid {
                    backend: "host",
                    target: Some(state_root.display().to_string()),
                    error: error.to_string(),
                });
                continue;
            },
        };
        let record = match RunnerRecord::read(&state_dir) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                candidates.push(Candidate::Invalid {
                    backend: "host",
                    target: Some(state_dir.display().to_string()),
                    error: error.to_string(),
                });
                continue;
            },
        };
        readable.push((state_dir, record));
    }
    candidates.extend(
        confirm_records(readable, |record| async move {
            RunnerControlClient::new(&record)
                .ping()
                .await
                .map(|state| state.phase)
                .map_err(|error| error.to_string())
        })
        .await,
    );
    Ok(candidates)
}

async fn confirm_records<F, Fut>(records: Vec<(PathBuf, RunnerRecord)>, probe: F) -> Vec<Candidate>
where
    F: Fn(RunnerRecord) -> Fut + Clone,
    Fut: Future<Output = Result<RunnerPhase, String>>,
{
    stream::iter(records)
        .map(|(state_dir, record)| {
            let probe = probe.clone();
            async move {
                let confirmed = probe(record.clone()).await;
                Candidate::Host {
                    state_dir,
                    record,
                    confirmed,
                }
            }
        })
        .buffered(OWNERSHIP_PROBE_CONCURRENCY)
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Barrier;

    fn record(name: &str) -> RunnerRecord {
        let state_dir = PathBuf::from(format!("/tmp/omnifs-host-probe-{name}"));
        RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            pid: 42,
            process_group: 42,
            filesystem: ResourceName::new(name).unwrap(),
            spec: FilesystemSpec::new(
                FilesystemProtocol::Nfs,
                FilesystemRuntime::Host,
                PathBuf::from(format!("/mnt/{name}")),
                None,
                None,
            )
            .unwrap(),
            control_socket: state_dir.join("control.sock"),
        }
    }

    #[tokio::test]
    async fn concurrent_probe_results_keep_each_host_candidate() {
        let barrier = Arc::new(Barrier::new(3));
        let records = ["confirmed", "dead", "later"]
            .into_iter()
            .map(|name| (PathBuf::from(format!("/tmp/{name}")), record(name)))
            .collect();
        let probe_barrier = Arc::clone(&barrier);
        let candidates = tokio::time::timeout(
            Duration::from_secs(1),
            confirm_records(records, move |record| {
                let barrier = Arc::clone(&probe_barrier);
                async move {
                    barrier.wait().await;
                    if record.filesystem.as_str() == "dead" {
                        Err("runner control is unavailable".to_owned())
                    } else {
                        Ok(RunnerPhase::Mounted)
                    }
                }
            }),
        )
        .await
        .expect("host probes did not run concurrently");

        assert_eq!(candidates.len(), 3);
        assert!(candidates.iter().any(|candidate| matches!(
            candidate,
            Candidate::Host {
                record,
                confirmed: Ok(RunnerPhase::Mounted),
                ..
            } if record.filesystem.as_str() == "confirmed"
        )));
        assert!(candidates.iter().any(|candidate| matches!(
            candidate,
            Candidate::Host {
                record,
                confirmed: Err(error),
                ..
            } if record.filesystem.as_str() == "dead" && error == "runner control is unavailable"
        )));
        assert!(candidates.iter().any(|candidate| matches!(
            candidate,
            Candidate::Host {
                record,
                confirmed: Ok(RunnerPhase::Mounted),
                ..
            } if record.filesystem.as_str() == "later"
        )));
    }

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
            filesystem: ResourceName::new("main").unwrap(),
            spec: FilesystemSpec::new(
                if cfg!(target_os = "linux") {
                    FilesystemProtocol::Fuse
                } else {
                    FilesystemProtocol::Nfs
                },
                FilesystemRuntime::Host,
                mount_point.clone(),
                None,
                None,
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
        let state_root = temp.path().join("state");
        let valid_id = ResourceName::new("valid").unwrap();
        let invalid_id = ResourceName::new("invalid").unwrap();
        let valid_dir = state_root.join(valid_id.as_str());
        let invalid_dir = state_root.join(invalid_id.as_str());
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::create_dir_all(&invalid_dir).unwrap();
        let record = RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            pid: 42,
            process_group: 42,
            filesystem: valid_id,
            spec: FilesystemSpec::new(
                FilesystemProtocol::Nfs,
                FilesystemRuntime::Host,
                PathBuf::from("/mnt/valid"),
                None,
                None,
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

        let candidates = owned(&state_root).await.unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|candidate| matches!(
            candidate,
            Candidate::Host { record, .. }
                if record.spec.location() == Path::new("/mnt/valid")
        )));
        assert!(candidates.iter().any(|candidate| matches!(
            candidate,
            Candidate::Invalid { target, .. }
                if target.as_deref() == Some(invalid_dir.display().to_string().as_str())
        )));
    }
}
