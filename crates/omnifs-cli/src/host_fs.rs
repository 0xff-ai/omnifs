//! Host filesystem launch, runner probing, and control.

use crate::client_fs_state::ClientFilesystemState;
use crate::error::WithHint;
use crate::filesystem_driver::{Candidate, ensure_identity_unchanged};
use anyhow::{Context as _, Result, ensure};
use omnifs_core::fs;
use omnifs_mtab::{RunnerClaim, RunnerRecord};
use omnifs_thin::host_control::{
    RunnerControlClient, RunnerPhase, RunnerState, StopOutcome, control_socket_for,
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const STOP_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn probe(protocol: fs::Protocol) -> Result<()> {
    if protocol == fs::Protocol::Fuse && !Path::new("/dev/fuse").exists() {
        anyhow::bail!("/dev/fuse is not available on this host");
    }
    Ok(())
}

/// The host filesystem runner: a durable per-filesystem state directory
/// holding the runner record and control socket. Unlike Docker or libkrun,
/// the host runner is a bare directory with no owned process handle between
/// calls, so every method rediscovers the runner process through its record.
pub(crate) struct HostDriver {
    state_dir: PathBuf,
}

impl HostDriver {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    /// Prove a live, identity-matched runner, returning its confirmed record
    /// and state together so a caller that needs to act on the identity (for
    /// example, stop it) does not have to re-read the record from disk.
    pub(crate) async fn confirmed(
        &self,
        spec: &fs::Spec,
    ) -> Result<Option<(RunnerRecord, RunnerState)>> {
        let mount_point = spec.location();
        let Some(record) = RunnerRecord::read(&self.state_dir)? else {
            if omnifs_nfs::mount_is_active_checked(mount_point)? {
                return Err(anyhow::anyhow!(
                    "host filesystem state exists at {} without a runner record",
                    mount_point.display()
                ))
                .with_hint("Run `omnifs doctor` to diagnose");
            }
            return Ok(None);
        };
        crate::filesystem_driver::ensure_record_matches(&record.spec, spec)?;
        let state = RunnerControlClient::new(&record)
            .ping()
            .await
            .with_context(|| {
                format!(
                    "host filesystem at {} could not confirm runner {}",
                    mount_point.display(),
                    record.instance_id
                )
            })
            .with_hint("Run `omnifs doctor` to diagnose")?;
        Ok(Some((record, state)))
    }

    pub(crate) async fn launch(
        &self,
        ctx: &crate::filesystem_driver::LaunchContext<'_>,
    ) -> Result<()> {
        probe(ctx.spec.protocol())?;
        PendingHostFilesystem::spawn(
            &self.state_dir,
            ctx.client_state,
            ctx.client_owner,
            ctx.spec,
            ctx.attach_unix()?,
        )?
        .wait_until_mounted(ctx.spec)
        .await?;
        let key_width = crate::ui::output::Output::ledger_block_width(&["filesystem"]);
        ctx.output.ledger_row(
            &crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "filesystem",
                format!(
                    "{} mounted at {}",
                    ctx.spec.id(),
                    ctx.spec.location().display()
                ),
            ),
            key_width,
        );
        Ok(())
    }

    /// The one teardown entry point for a proven identity: reconfirm the
    /// runner still matches `expected`, then request its stop and wait for
    /// cleanup. Callers without an already-confirmed record obtain one from
    /// [`Self::confirmed`] first; an identity that has already disappeared is a
    /// no-op there, not an error here.
    pub(crate) async fn stop_confirmed(&self, expected: &RunnerRecord) -> Result<()> {
        let record = RunnerRecord::read(&self.state_dir)?
            .context("runner record disappeared before confirmed stop")?;
        ensure_identity_unchanged(Some(&record), expected, "runner")?;
        let client = RunnerControlClient::new(&record);
        client.ping().await.context("reconfirm host filesystem")?;
        let outcome = client.stop().await?;
        wait_for_cleanup(&self.state_dir, record.spec.location(), outcome).await
    }

    /// Ends a proven runner before a new one starts: a runner of an active NFS
    /// mount that answers `Restart` releases the mount, which the new runner
    /// serves in place; any other runner, as one of a build without `Restart`,
    /// stops.
    pub(crate) async fn end_for_restart(
        &self,
        record: &RunnerRecord,
        state: &RunnerState,
    ) -> Result<()> {
        if state.restart && omnifs_nfs::mount_is_active_checked(record.spec.location())? {
            self.release_confirmed(record).await
        } else {
            self.stop_confirmed(record).await
        }
    }

    /// Restarts a proven runner of an active NFS mount in place: ask it to
    /// release its mount, and wait for the exit of its process, so that a new
    /// runner serves the mount at the address of its state with its
    /// filehandle table.
    pub(crate) async fn release_confirmed(&self, expected: &RunnerRecord) -> Result<()> {
        let record = RunnerRecord::read(&self.state_dir)?
            .context("runner record disappeared before its restart")?;
        ensure_identity_unchanged(Some(&record), expected, "runner")?;
        // The exit is watched before the runner is reconfirmed, so that the
        // process it watches is the runner that answers, and before its
        // restart, which leaves the mount to the next runner.
        let exit = process_exit(record.pid)?.context("the runner ended before its restart")?;
        let client = RunnerControlClient::new(&record);
        client.ping().await.context("reconfirm host filesystem")?;
        client.restart().await?;
        drop(exit.readable().await?);
        Ok(())
    }

    pub(crate) async fn cleanup_stale(&self, expected: &RunnerRecord) -> Result<()> {
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
    child: Child,
    state_dir: PathBuf,
    instance_id: String,
    log_path: PathBuf,
}

impl PendingHostFilesystem {
    fn spawn(
        state_dir: &Path,
        filesystem: &ClientFilesystemState,
        client_owner: omnifs_core::ClientOwnerId,
        spec: &fs::Spec,
        attach_socket: &Path,
    ) -> Result<Self> {
        let state_dir = state_dir.to_path_buf();
        let instance_id = crate::process::new_instance_id("host filesystem")?;
        let control_socket = control_socket_for(&state_dir, &instance_id);
        let executable = std::env::current_exe().context("resolve the omnifs executable")?;
        let log_path = filesystem.host_log(spec.id());
        let log_parent = log_path
            .parent()
            .context("filesystem log path has no parent directory")?;
        std::fs::create_dir_all(log_parent)
            .with_context(|| format!("create {}", log_parent.display()))?;

        let mut command = Command::new(&executable);
        command
            .arg("run-fs")
            .arg("--client-owner")
            .arg(client_owner.to_string())
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
            .arg(attach_socket)
            .arg("--runner-instance")
            .arg(&instance_id)
            .arg("--runner-control")
            .arg(&control_socket);
        crate::process::configure_detached_child(
            &mut command,
            &log_path,
            crate::process::LogMode::Append,
        )?;
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

    async fn wait_until_mounted(self, spec: &fs::Spec) -> Result<()> {
        // Bundled (including `spec`) so the check closure captures nothing
        // from the enclosing scope and takes everything by explicit `&mut`
        // argument instead: an `FnMut` closure cannot itself return a future
        // that borrows its own captured environment, but a fresh reborrow of
        // an argument can.
        struct Wait<'a> {
            pending: PendingHostFilesystem,
            spec: &'a fs::Spec,
            last_phase: Option<RunnerPhase>,
        }
        let mut wait = Wait {
            pending: self,
            spec,
            last_phase: None,
        };
        let ready =
            crate::process::poll_until_mut(STARTUP_TIMEOUT, POLL_INTERVAL, &mut wait, |wait| {
                Box::pin(async move {
                    if let Some(status) = wait
                        .pending
                        .child
                        .try_wait()
                        .context("inspect host filesystem process")?
                    {
                        return Err(anyhow::anyhow!(
                            "host filesystem `{}` exited with {status}",
                            wait.spec.id()
                        ))
                        .with_hint(format!("See {}", wait.pending.log_path.display()));
                    }
                    match RunnerRecord::read(&wait.pending.state_dir) {
                        Ok(Some(record)) if record.instance_id == wait.pending.instance_id => {
                            crate::filesystem_driver::ensure_record_matches(
                                &record.spec,
                                wait.spec,
                            )?;
                            if let Ok(state) = RunnerControlClient::new(&record).ping().await {
                                wait.last_phase = Some(state.phase.clone());
                                match state.phase {
                                    RunnerPhase::Mounted => return Ok(Some(())),
                                    RunnerPhase::Failed { message } => {
                                        return Err(anyhow::anyhow!(
                                            "host filesystem `{}` failed: {message}",
                                            wait.spec.id()
                                        ))
                                        .with_hint(format!(
                                            "See {}",
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
                                "host filesystem state at {} belongs to runner {}",
                                wait.pending.state_dir.display(),
                                record.instance_id
                            ))
                            .with_hint("Run `omnifs doctor` to diagnose");
                        },
                        Ok(None) => {},
                        Err(error) => return Err(error.into()),
                    }
                    Ok(None)
                })
            })
            .await?;
        ready.map_or_else(
            || {
                let phase = phase_label(wait.last_phase.clone());
                Err(anyhow::anyhow!(
                    "host filesystem `{}` did not confirm mount startup within {}s; \
                     last proved phase was {phase}; runner {} was left alive for safe cleanup",
                    spec.id(),
                    STARTUP_TIMEOUT.as_secs(),
                    wait.pending.instance_id
                ))
                .with_hint(format!("See {}", wait.pending.log_path.display()))
            },
            Ok,
        )
    }
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
    crate::process::poll_until(STOP_TIMEOUT, POLL_INTERVAL, || async {
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

/// The exit of the process `pid`, an event of the kernel that becomes
/// readable at it; none for a process that has already exited.
fn process_exit(pid: u32) -> Result<Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>> {
    Ok(omnifs_mtab::process_exit(pid)?
        .map(|exit| tokio::io::unix::AsyncFd::with_interest(exit, tokio::io::Interest::READABLE))
        .transpose()?)
}

pub(crate) async fn owned(filesystem: &ClientFilesystemState) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    let entries = match std::fs::read_dir(filesystem.state_root()) {
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
                    target: Some(filesystem.state_root().display().to_string()),
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
        let confirmed = RunnerControlClient::new(&record)
            .ping()
            .await
            .map(|state| state.phase)
            .map_err(|error| error.to_string());
        candidates.push(Candidate::Host {
            state_dir,
            record,
            confirmed,
        });
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit of a process is an event that a wait takes at the exit, and
    /// a process that has exited has none.
    #[tokio::test]
    async fn the_exit_of_a_runner_is_an_event() {
        let mut child = std::process::Command::new("sleep")
            .arg("1")
            .spawn()
            .unwrap();
        let exit = process_exit(child.id())
            .unwrap()
            .expect("a running process");
        drop(
            tokio::time::timeout(Duration::from_secs(10), exit.readable())
                .await
                .expect("the exit is an event")
                .unwrap(),
        );
        child.wait().unwrap();
        assert!(process_exit(child.id()).unwrap().is_none());
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
            spec: fs::Spec::new(
                fs::Id::new("main").unwrap(),
                fs::Protocol::Fuse,
                fs::Runtime::Host,
                mount_point.clone(),
            )
            .unwrap(),
            control_socket: state_dir.join("control.sock"),
            program: None,
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

    /// A runner of a build before `Restart`, which answers `Ping` without it,
    /// ends at `Stop` when its filesystem restarts.
    #[tokio::test]
    async fn a_runner_of_a_build_without_restart_ends_at_stop() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let instance = "0123456789abcdef0123456789abcdef";
        let spec = fs::Spec::new(
            fs::Id::new("main").unwrap(),
            fs::Protocol::Nfs,
            fs::Runtime::Host,
            temp.path().join("mount"),
        )
        .unwrap();
        let record = RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: instance.to_owned(),
            pid: 42,
            process_group: 42,
            spec: spec.clone(),
            control_socket: state_dir.join("control.sock"),
            program: None,
        };
        let record_path = state_dir.join("runner.json");
        std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&record.control_socket).unwrap();
        let runner = tokio::spawn(async move {
            let mut requests = Vec::new();
            while requests.last().is_none_or(|kind| kind != "stop") {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut line = String::new();
                tokio::io::BufReader::new(reader)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let kind = request["request"]["kind"].as_str().unwrap().to_owned();
                let reply = match kind.as_str() {
                    "ping" => serde_json::json!({"kind": "pong"}),
                    "stop" => {
                        std::fs::remove_file(&record_path).unwrap();
                        serde_json::json!({"kind": "stop", "value": "stopped"})
                    },
                    kind => panic!("a runner before `Restart` got `{kind}`"),
                };
                let envelope = serde_json::json!({
                    "version": 1,
                    "instance_id": instance,
                    "phase": "mounted",
                    "reply": reply,
                });
                writer
                    .write_all(format!("{envelope}\n").as_bytes())
                    .await
                    .unwrap();
                requests.push(kind);
            }
            requests
        });

        let driver = HostDriver::new(state_dir);
        let (confirmed, state) = driver.confirmed(&spec).await.unwrap().unwrap();
        assert!(!state.restart);
        driver.end_for_restart(&confirmed, &state).await.unwrap();
        assert_eq!(runner.await.unwrap(), ["ping", "ping", "stop"]);
    }

    #[tokio::test]
    async fn corrupt_runner_leaf_does_not_hide_a_valid_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let filesystem = ClientFilesystemState::under_root(&temp.path().join("client"));
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
            program: None,
        };
        std::fs::write(
            valid_dir.join("runner.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        std::fs::write(invalid_dir.join("runner.json"), b"{broken").unwrap();

        let candidates = owned(&filesystem).await.unwrap();
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
