//! `FilesystemDriver`: the closed three-variant dispatch over the host,
//! Docker, and libkrun filesystem lifecycle drivers.
//!
//! `fs::Runtime` is a closed, persisted 3-variant enum with no plugin or
//! extension pressure, so this stays a plain enum with methods rather than a
//! trait object. [`Self::for_spec`] is the one remaining match on
//! `fs::Runtime`; every other command dispatches by matching this enum's own
//! variants instead.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use omnifs_core::{ClientOwnerId, fs};

use crate::client_fs_state::ClientFilesystemState;
use crate::docker::{DockerClient, DockerContainerIdentity};
use crate::host_fs::HostDriver;
use crate::libkrun_runner::LibkrunRunner;
use crate::rpc::RpcClient;
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
    async fn resolve(
        client_state: &'a ClientFilesystemState,
        client_owner: ClientOwnerId,
        spec: &'a fs::Spec,
        output: Output,
    ) -> Result<Self> {
        let info = RpcClient::resolve()?.inventory().await?.info;
        Ok(Self {
            client_state,
            client_owner,
            spec,
            output,
            attach_unix: info.attach_unix,
            attach_tcp: info.attach_tcp,
        })
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
                ensure!(
                    record.spec == *spec,
                    "libkrun helper spec `{}` does not match configured filesystem `{}`",
                    record.spec,
                    spec.id()
                );
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
    pub(crate) async fn launch(
        &self,
        client_state: &ClientFilesystemState,
        client_owner: ClientOwnerId,
        spec: &fs::Spec,
        output: Output,
    ) -> Result<()> {
        let ctx = LaunchContext::resolve(client_state, client_owner, spec, output).await?;
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
/// across the three backends, or a whole backend's listing failure. Doctor's
/// stray-filesystem check is the only consumer.
pub(crate) enum Candidate {
    Host(crate::host_fs::RunnerProbe),
    Docker(crate::docker::OwnedFilesystemCandidate),
    Libkrun(crate::libkrun_runner::LibkrunCandidate),
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
        Ok(probes) => candidates.extend(probes.into_iter().map(Candidate::Host)),
        Err(error) => candidates.push(Candidate::ListingFailed {
            backend: "host",
            error: format!("{error:#}"),
        }),
    }
    if let Some(docker) = docker {
        match docker.owned(client_state.profile_root()).await {
            Ok(owned) => candidates.extend(owned.into_iter().map(Candidate::Docker)),
            Err(error) => candidates.push(Candidate::ListingFailed {
                backend: "docker",
                error: format!("{error:#}"),
            }),
        }
    }
    candidates.extend(
        LibkrunRunner::owned(&client_state.runtime_root())
            .into_iter()
            .map(Candidate::Libkrun),
    );
    candidates
}
