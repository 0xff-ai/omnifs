//! `FilesystemDriver`: the closed three-variant dispatch over the host,
//! Docker, and libkrun filesystem lifecycle drivers.
//!
//! `fs::Runtime` is a closed, persisted 3-variant enum with no plugin or
//! extension pressure, so this stays a plain enum with methods rather than a
//! trait object. [`Self::for_spec`] is the one remaining match on
//! `fs::Runtime`; every other command dispatches by matching this enum's own
//! variants instead.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use omnifs_core::{ClientOwnerId, fs};

use crate::client_fs_state::ClientFilesystemState;
use crate::docker::{DockerClient, DockerContainerIdentity, OwnedFilesystemContainer};
use crate::host_fs::HostDriver;
use crate::libkrun_runner::LibkrunRunner;
use crate::ui::output::Output;

/// Everything a filesystem driver's `launch` needs, resolved once by
/// [`FilesystemDriver::launch`] instead of three divergent positional
/// argument lists (host and Docker took raw tuples; libkrun had its own
/// bespoke request type). Each driver reads only the attach address its own
/// transport uses: host and libkrun dial the daemon's Unix listener, Docker
/// dials the TCP one.
pub(crate) struct LaunchContext<'a> {
    pub(crate) client_state: &'a ClientFilesystemState,
    pub(crate) client_owner: ClientOwnerId,
    pub(crate) spec: &'a fs::Spec,
    pub(crate) output: Output,
    attach_unix: Option<PathBuf>,
    attach_tcp: Option<SocketAddr>,
}

impl<'a> LaunchContext<'a> {
    /// `info` is the caller's already-fetched `GetInventory` response's
    /// `info` field: every launch caller already probes the daemon's
    /// inventory before deciding to launch at all (to check attachment or
    /// readiness), so this never re-fetches it.
    fn resolve(
        client_state: &'a ClientFilesystemState,
        client_owner: ClientOwnerId,
        spec: &'a fs::Spec,
        output: Output,
        info: omnifs_api::DaemonInfo,
    ) -> Self {
        Self {
            client_state,
            client_owner,
            spec,
            output,
            attach_unix: info.attach_unix,
            attach_tcp: info.attach_tcp,
        }
    }

    pub(crate) fn attach_unix(&self) -> Result<&Path> {
        self.attach_unix
            .as_deref()
            .context("daemon has no Unix filesystem attach listener")
    }

    pub(crate) fn attach_tcp(&self) -> Result<SocketAddr> {
        self.attach_tcp
            .context("daemon has no TCP filesystem attach listener")
    }
}

/// The one spec-vs-record identity check the host and libkrun drivers'
/// `confirmed` both need: a durable record only proves a live instance when
/// its spec still matches the caller's configured one. Docker's `confirmed`
/// keeps its own check (a launch-command argv comparison, since a Docker
/// container carries no persisted `fs::Spec` to compare against directly).
pub(crate) fn ensure_record_matches(record_spec: &fs::Spec, expected: &fs::Spec) -> Result<()> {
    ensure!(
        record_spec == expected,
        "runner record does not match configured filesystem `{}`",
        expected.id()
    );
    Ok(())
}

/// The "durable state survives but its record is gone" fail-shape every
/// backend's `confirmed` opens with: prove nothing is left to reconcile
/// before returning `None`, rather than silently reporting the filesystem as
/// absent while a mount or helper is still live. `what` names the backend's
/// state, `record_noun` the record it expected to find alongside it. Callers
/// attach the `omnifs doctor` remediation as a hint, since this shared
/// helper has no `Output` to reach through `with_hint`'s call-site pattern.
pub(crate) fn ensure_no_orphaned_state(
    state_exists: bool,
    what: &str,
    record_noun: &str,
    location: impl fmt::Display,
) -> Result<()> {
    ensure!(
        !state_exists,
        "{what} state exists at {location} without a {record_noun}"
    );
    Ok(())
}

/// The recheck every `stop_confirmed` performs before it touches anything:
/// a proven identity can go stale between confirmation and teardown, and
/// acting on a replacement process, container, or helper that reused the
/// same durable slot would be a correctness bug, not just a stale message.
/// `noun` names the backend for the one shared phrasing.
pub(crate) fn ensure_identity_unchanged<T: PartialEq>(
    current: Option<&T>,
    expected: &T,
    noun: &str,
) -> Result<()> {
    ensure!(
        current == Some(expected),
        "{noun} identity changed; refusing to touch its replacement"
    );
    Ok(())
}

/// Fold a rollback attempt's outcome into the error that triggered it: the
/// original failure always wins, but a cleanup that also failed is appended
/// as context rather than silently dropped. `what` names the thing rollback
/// was cleaning up. Shared by every driver whose `launch` rolls back a
/// partially started instance on failure; host deliberately never calls this
/// (it leaves a timed-out runner alive for safe cleanup instead).
pub(crate) fn err_after_rollback<T>(
    primary: anyhow::Error,
    cleanup: Result<()>,
    what: &str,
) -> Result<T> {
    Err(match cleanup {
        Ok(()) => primary,
        Err(cleanup_error) => primary.context(format!(
            "{what} also could not be cleaned up: {cleanup_error:#}"
        )),
    })
}

/// One filesystem's live driver, bound to a specific configured `spec` at
/// construction.
pub(crate) enum FilesystemDriver {
    Host(HostDriver),
    Docker(DockerClient),
    Libkrun(LibkrunRunner),
}

/// A live, identity-matched instance proven by [`FilesystemDriver::confirmed`],
/// fed back into [`FilesystemDriver::stop_confirmed`] for its one teardown
/// entry point. Docker keeps its running flag here (unlike host and
/// libkrun, an identity-matched Docker container can be confirmed while
/// stopped) so callers that care can distinguish it without a second probe.
pub(crate) enum Confirmed {
    Host(omnifs_mtab::RunnerRecord),
    Docker(DockerContainerIdentity, bool),
    Libkrun(omnifs_libkrun::HelperRecord),
}

impl FilesystemDriver {
    /// The one remaining match on `fs::Runtime`: every other command
    /// dispatches through this enum's own variants instead.
    pub(crate) fn for_spec(
        client_state: &ClientFilesystemState,
        spec: &fs::Spec,
        output: Output,
    ) -> Result<Self> {
        match spec.runtime() {
            fs::Runtime::Host => Ok(Self::Host(HostDriver::new(
                client_state.state_dir(spec.id()),
            ))),
            fs::Runtime::Docker => Ok(Self::Docker(DockerClient::for_filesystem(
                client_state,
                spec.id(),
                output,
            )?)),
            fs::Runtime::Libkrun => Ok(Self::Libkrun(LibkrunRunner::new(
                client_state.libkrun_root(spec.id()),
            ))),
        }
    }

    /// Prove a live, identity-matched instance, matching `spec`.
    pub(crate) async fn confirmed(
        &self,
        client_state: &ClientFilesystemState,
        client_owner: ClientOwnerId,
        spec: &fs::Spec,
    ) -> Result<Option<Confirmed>> {
        match self {
            Self::Host(runner) => Ok(runner
                .confirmed(spec)
                .await?
                .map(|(record, _phase)| Confirmed::Host(record))),
            Self::Docker(client) => Ok(client
                .confirmed(client_state.profile_root(), client_owner, spec)
                .await?
                .map(|(identity, running)| Confirmed::Docker(identity, running))),
            Self::Libkrun(runner) => {
                let Some(record) = runner.confirmed()? else {
                    return Ok(None);
                };
                ensure_record_matches(&record.spec, spec)?;
                Ok(Some(Confirmed::Libkrun(record)))
            },
        }
    }

    /// The one teardown entry point for a proven identity.
    pub(crate) async fn stop_confirmed(
        &self,
        client_state: &ClientFilesystemState,
        client_owner: ClientOwnerId,
        spec: &fs::Spec,
        confirmed: Confirmed,
    ) -> Result<()> {
        match (self, confirmed) {
            (Self::Host(runner), Confirmed::Host(record)) => runner.stop_confirmed(&record).await,
            (Self::Docker(client), Confirmed::Docker(identity, _running)) => {
                client
                    .stop_confirmed(&identity, client_state.profile_root(), client_owner, spec)
                    .await
            },
            (Self::Libkrun(runner), Confirmed::Libkrun(record)) => {
                runner.stop_confirmed(record).await
            },
            _ => anyhow::bail!(
                "confirmed identity belongs to a different filesystem driver than `{}`",
                spec.id()
            ),
        }
    }

    /// Launch and block until ready, uniformly across all three drivers.
    /// `info` is the caller's already-fetched daemon inventory info; see
    /// [`LaunchContext::resolve`].
    pub(crate) async fn launch(
        &self,
        client_state: &ClientFilesystemState,
        client_owner: ClientOwnerId,
        spec: &fs::Spec,
        output: Output,
        info: omnifs_api::DaemonInfo,
    ) -> Result<()> {
        let ctx = LaunchContext::resolve(client_state, client_owner, spec, output, info);
        match self {
            Self::Host(runner) => runner.launch(&ctx).await,
            Self::Docker(client) => client.launch(&ctx).await,
            Self::Libkrun(runner) => runner.launch(&ctx).await,
        }
    }

    /// Construct the shell/exec command for a confirmed live instance, or
    /// `None` for host: entering a host filesystem is a `cd` into an
    /// already-visible mount, not a remote exec, so `commands/fs.rs` keeps
    /// that branch outside the driver.
    pub(crate) fn shell_command(&self, shell: Option<&str>, argv: &[String]) -> Option<Command> {
        match self {
            Self::Host(_) => None,
            Self::Docker(client) => Some(client.shell_command(shell, argv)),
            Self::Libkrun(runner) => Some(runner.shell_command(shell, argv)),
        }
    }
}

/// One filesystem instance found by [`owned_filesystems`]'s combined scan
/// across the three backends, or one entry that scan could not identify or
/// confirm. Doctor's stray-filesystem check is the only consumer. A single
/// flat enum rather than three backend-specific `Valid`/`Invalid` scan
/// results each wrapped again here: every backend yields this type directly,
/// so an unreadable or unconfirmable entry (`Invalid`) has one shape and one
/// finding path regardless of which backend found it.
pub(crate) enum Candidate {
    Host {
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
        confirmed: Result<omnifs_thin::host_control::RunnerPhase, String>,
    },
    Docker(OwnedFilesystemContainer),
    Libkrun {
        id: fs::Id,
        state_dir: PathBuf,
        confirmed: Result<Option<omnifs_libkrun::HelperRecord>, String>,
    },
    /// One scan entry that could not be identified or read, naming the
    /// owning backend so a caller need not re-derive it.
    Invalid {
        backend: &'static str,
        target: Option<String>,
        error: String,
    },
    /// A whole backend's listing call failed, naming which one so the
    /// caller can label its finding without re-deriving it.
    ListingFailed {
        backend: &'static str,
        error: String,
    },
}

/// List every filesystem instance any of the three backends owns. Host and
/// libkrun scan their own runtime roots directly; Docker's scan reuses an
/// already-connected client (or is skipped entirely when Docker itself is
/// unreachable, since a stray container cannot be confirmed without one).
/// Each backend's listing failure becomes its own candidate rather than
/// aborting the other two backends' scans.
pub(crate) async fn owned_filesystems(
    client_state: &ClientFilesystemState,
    docker: Option<&DockerClient>,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    match crate::host_fs::owned(client_state).await {
        Ok(mut owned) => candidates.append(&mut owned),
        Err(error) => candidates.push(Candidate::ListingFailed {
            backend: "host",
            error: format!("{error:#}"),
        }),
    }
    if let Some(docker) = docker {
        match docker.owned(client_state.profile_root()).await {
            Ok(mut owned) => candidates.append(&mut owned),
            Err(error) => candidates.push(Candidate::ListingFailed {
                backend: "docker",
                error: format!("{error:#}"),
            }),
        }
    }
    candidates.append(&mut LibkrunRunner::owned(&client_state.runtime_root()));
    candidates
}
