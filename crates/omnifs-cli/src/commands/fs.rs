//! Named filesystem configuration and lifecycle.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Args, Subcommand};
use omnifs_core::fs;
use omnifs_workspace::Workspace;
use serde::Serialize;

use crate::docker::{DockerClient, DockerTarget};
use crate::error::ExitCode;
use crate::fs_container::{
    FILESYSTEM_DEV_IMAGE, filesystem_container_name, resolve_filesystem_image,
};
use crate::libkrun_runner::{LibkrunLaunchRequest, LibkrunRunner};
use crate::ui::output::{Output, ResultVerdict};

const DOCKER_TIMEOUT: Duration = Duration::from_secs(5);
const LIBKRUN_TIMEOUT: Duration = Duration::from_secs(90);
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(200);

#[derive(Args, Debug)]
pub struct FsArgs {
    #[command(subcommand)]
    pub command: FsCommand,
}

#[derive(Subcommand, Debug)]
pub enum FsCommand {
    /// Create a named filesystem configuration
    Create(CreateArgs),
    /// Remove a detached filesystem configuration
    Rm(NameArgs),
    /// Start and mount a configured filesystem
    Attach(NameArgs),
    /// Unmount and stop a configured filesystem
    Detach(NameArgs),
    /// Detach and attach a configured filesystem
    Restart(NameArgs),
    /// Enter or locate a running filesystem
    Shell(ShellArgs),
    /// List configured filesystems and daemon attachment state
    Ls,
}

#[derive(Args, Debug, Clone)]
#[command(
    after_help = "Examples:\n  omnifs fs create --name work\n  omnifs fs create --name native --protocol nfs --runtime host --location /mnt/omnifs"
)]
pub struct CreateArgs {
    #[arg(long)]
    pub name: fs::Id,
    #[arg(long)]
    pub protocol: Option<fs::Protocol>,
    #[arg(long)]
    pub runtime: Option<fs::Runtime>,
    #[arg(long)]
    pub location: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct NameArgs {
    #[arg(long)]
    pub name: fs::Id,
}

#[derive(Args, Debug, Clone)]
#[command(
    after_help = "Examples:\n  omnifs fs shell --name work\n  omnifs fs shell --name work -- ls -la"
)]
pub struct ShellArgs {
    #[arg(long)]
    pub name: fs::Id,
    /// Shell to launch in a guest filesystem.
    #[arg(long)]
    pub shell: Option<String>,
    /// Command and arguments to run in the projected tree.
    #[arg(
        num_args = 0..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        conflicts_with = "shell"
    )]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ActionResult {
    filesystem: fs::Spec,
    state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct ListResult {
    filesystems: Vec<ListRow>,
}

#[derive(Debug, Clone, Serialize)]
struct ListRow {
    #[serde(flatten)]
    spec: fs::Spec,
    state: &'static str,
}

impl FsArgs {
    pub async fn run(self, output: Output) -> Result<ExitCode> {
        match self.command {
            FsCommand::Create(args) => args.run(&output),
            FsCommand::Rm(args) => rm(args, output).await,
            FsCommand::Attach(args) => attach(args, output).await,
            FsCommand::Detach(args) => detach(args, output).await,
            FsCommand::Restart(args) => restart(args, output).await,
            FsCommand::Shell(args) => shell(args, output).await,
            FsCommand::Ls => list(output).await,
        }
    }
}

impl CreateArgs {
    fn run(self, output: &Output) -> Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let spec = resolve_spec(&workspace, self)?;
        workspace.filesystems().claim(spec.id())?.create(&spec)?;
        finish_action(output, spec, "configured")
    }
}

fn resolve_spec(workspace: &Workspace, args: CreateArgs) -> Result<fs::Spec> {
    let (protocol, runtime) = resolve_pair(args.protocol, args.runtime)?;
    ensure!(
        supports(protocol, runtime),
        "{protocol}/{runtime} is not supported on {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let location = match runtime {
        fs::Runtime::Host => args.location.unwrap_or_else(|| {
            workspace
                .filesystem_state()
                .default_host_location(&args.name)
        }),
        fs::Runtime::Docker | fs::Runtime::Libkrun => {
            ensure!(
                args.location.is_none(),
                "the {runtime} runtime owns its location; --location is not allowed"
            );
            PathBuf::from(fs::GUEST_LOCATION)
        },
    };
    fs::Spec::new(args.name, protocol, runtime, location).map_err(Into::into)
}

fn resolve_pair(
    protocol: Option<fs::Protocol>,
    runtime: Option<fs::Runtime>,
) -> Result<(fs::Protocol, fs::Runtime)> {
    match (protocol, runtime) {
        (Some(fs::Protocol::Nfs), None) => Ok((fs::Protocol::Nfs, fs::Runtime::Host)),
        (None, Some(fs::Runtime::Docker)) => Ok((fs::Protocol::Fuse, fs::Runtime::Docker)),
        (None, Some(fs::Runtime::Libkrun)) => Ok((fs::Protocol::Fuse, fs::Runtime::Libkrun)),
        (Some(fs::Protocol::Fuse), None) => default_fuse_pair()
            .with_context(|| "FUSE has no default runtime on this platform; provide --runtime"),
        (None, Some(fs::Runtime::Host)) => {
            let (protocol, _) = platform_default().with_context(
                || "host has no default protocol on this platform; provide --protocol",
            )?;
            Ok((protocol, fs::Runtime::Host))
        },
        (None, None) => platform_default()
            .with_context(|| "this platform has no default filesystem protocol/runtime pair"),
        (Some(protocol), Some(runtime)) => Ok((protocol, runtime)),
    }
}

fn platform_default() -> Option<(fs::Protocol, fs::Runtime)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", _) => Some((fs::Protocol::Fuse, fs::Runtime::Host)),
        ("macos", "aarch64") => Some((fs::Protocol::Fuse, fs::Runtime::Libkrun)),
        ("macos", _) => Some((fs::Protocol::Nfs, fs::Runtime::Host)),
        _ => None,
    }
}

fn default_fuse_pair() -> Option<(fs::Protocol, fs::Runtime)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", _) => Some((fs::Protocol::Fuse, fs::Runtime::Host)),
        ("macos", "aarch64") => Some((fs::Protocol::Fuse, fs::Runtime::Libkrun)),
        _ => None,
    }
}

pub(crate) fn supports(protocol: fs::Protocol, runtime: fs::Runtime) -> bool {
    matches!(
        (
            std::env::consts::OS,
            std::env::consts::ARCH,
            protocol,
            runtime,
        ),
        (
            "linux",
            _,
            fs::Protocol::Fuse,
            fs::Runtime::Host | fs::Runtime::Docker
        ) | ("linux" | "macos", _, fs::Protocol::Nfs, fs::Runtime::Host)
            | ("macos", _, fs::Protocol::Fuse, fs::Runtime::Docker)
            | ("macos", "aarch64", fs::Protocol::Fuse, fs::Runtime::Libkrun)
    )
}

pub(crate) fn available_filesystems() -> Vec<(fs::Protocol, fs::Runtime)> {
    [fs::Protocol::Fuse, fs::Protocol::Nfs]
        .into_iter()
        .flat_map(|protocol| {
            [fs::Runtime::Host, fs::Runtime::Docker, fs::Runtime::Libkrun]
                .into_iter()
                .filter(move |runtime| supports(protocol, *runtime))
                .map(move |runtime| (protocol, runtime))
        })
        .collect()
}

pub(crate) fn default_runtime(protocol: fs::Protocol) -> Option<fs::Runtime> {
    match protocol {
        fs::Protocol::Fuse => default_fuse_pair().map(|(_, runtime)| runtime),
        fs::Protocol::Nfs if supports(fs::Protocol::Nfs, fs::Runtime::Host) => {
            Some(fs::Runtime::Host)
        },
        fs::Protocol::Nfs => None,
    }
}

pub(crate) async fn ensure_setup_filesystem(
    workspace: &Workspace,
    protocol: fs::Protocol,
    runtime: fs::Runtime,
    output: Output,
) -> Result<()> {
    let id = fs::Id::new(format!("{protocol}-{runtime}"))?;
    let claim = workspace.filesystems().claim(&id)?;
    let spec = if let Some(spec) = claim.get()? {
        ensure!(
            spec.protocol() == protocol && spec.runtime() == runtime,
            "configured filesystem `{id}` does not match setup's {protocol}/{runtime} choice"
        );
        spec
    } else {
        let location = match runtime {
            fs::Runtime::Host => workspace.filesystem_state().default_host_location(&id),
            fs::Runtime::Docker | fs::Runtime::Libkrun => PathBuf::from(fs::GUEST_LOCATION),
        };
        let spec = fs::Spec::new(id, protocol, runtime, location)?;
        claim.create(&spec)?;
        spec
    };
    let attached = crate::client::DaemonClient::for_workspace(workspace)
        .status_optional_checked()
        .await?
        .is_some_and(|status| status.filesystems.contains(&spec));
    if attached {
        return Ok(());
    }
    if runtime_running(workspace, &spec, output.clone()).await? {
        ensure!(
            wait_for_attachment(workspace, &spec).await,
            "filesystem `{}` is running but did not attach to the daemon",
            spec.id()
        );
        return Ok(());
    }
    launch_and_confirm(workspace, &spec, output).await
}

async fn rm(args: NameArgs, output: Output) -> Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let claim = workspace.filesystems().claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    ensure_not_attached(&workspace, &spec).await?;
    ensure!(
        !runtime_running(&workspace, &spec, output.clone()).await?,
        "filesystem `{}` is still running; detach it first",
        spec.id()
    );
    claim.remove()?;
    finish_action(&output, spec, "removed")
}

async fn attach(args: NameArgs, output: Output) -> Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let claim = workspace.filesystems().claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    ensure!(
        supports(spec.protocol(), spec.runtime()),
        "{}/{} is not supported on this platform",
        spec.protocol(),
        spec.runtime()
    );
    let status = crate::client::DaemonClient::for_workspace(&workspace)
        .status_optional()
        .await?
        .context("the daemon is stopped; run `omnifs up` before attaching a filesystem")?;
    ensure!(
        !status.filesystems.iter().any(|row| row.id() == spec.id()),
        "filesystem `{}` is already attached",
        spec.id()
    );
    ensure!(
        !runtime_running(&workspace, &spec, output.clone()).await?,
        "filesystem `{}` already has a running {} instance",
        spec.id(),
        spec.runtime()
    );
    launch_and_confirm(&workspace, &spec, output.clone()).await?;
    finish_action(&output, spec, "attached")
}

async fn detach(args: NameArgs, output: Output) -> Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let claim = workspace.filesystems().claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    stop_runtime(&workspace, &spec, output.clone()).await?;
    ensure!(
        !runtime_running(&workspace, &spec, output.clone()).await?,
        "filesystem `{}` still has a running {} instance",
        spec.id(),
        spec.runtime()
    );
    finish_action(&output, spec, "detached")
}

async fn restart(args: NameArgs, output: Output) -> Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let claim = workspace.filesystems().claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    ensure!(
        supports(spec.protocol(), spec.runtime()),
        "{}/{} is not supported on this platform",
        spec.protocol(),
        spec.runtime()
    );
    crate::client::DaemonClient::for_workspace(&workspace)
        .status_optional()
        .await?
        .context("the daemon is stopped; run `omnifs up` before restarting a filesystem")?;
    stop_runtime(&workspace, &spec, output.clone()).await?;
    launch_and_confirm(&workspace, &spec, output.clone()).await?;
    finish_action(&output, spec, "attached")
}

fn required_spec(
    claim: &omnifs_workspace::filesystems::Claim<'_>,
    id: &fs::Id,
) -> Result<fs::Spec> {
    claim
        .get()?
        .with_context(|| format!("filesystem `{id}` is not configured"))
}

async fn ensure_not_attached(workspace: &Workspace, spec: &fs::Spec) -> Result<()> {
    let status = crate::client::DaemonClient::for_workspace(workspace)
        .status_optional_checked()
        .await
        .context("cannot prove the daemon attachment state; run `omnifs doctor`")?;
    if let Some(status) = status {
        ensure!(
            !status.filesystems.iter().any(|row| row.id() == spec.id()),
            "filesystem `{}` is attached; detach it first",
            spec.id()
        );
    }
    Ok(())
}

async fn runtime_running(workspace: &Workspace, spec: &fs::Spec, output: Output) -> Result<bool> {
    match spec.runtime() {
        fs::Runtime::Host => Ok(crate::host_fs::phase(workspace.filesystem_state(), spec)
            .await?
            .is_some()),
        fs::Runtime::Docker => {
            let client = docker_client(workspace, spec, output)?;
            match client.is_running().await? {
                None => Ok(false),
                Some(false) => bail!(
                    "filesystem `{}` has a stopped Docker container; run `omnifs doctor`",
                    spec.id()
                ),
                Some(true) => client
                    .confirmed_filesystem(workspace.identity().container_label(), spec)
                    .await
                    .map(|identity| identity.is_some()),
            }
        },
        fs::Runtime::Libkrun => {
            let runner = LibkrunRunner::new(workspace.filesystem_state().libkrun_root(spec.id()));
            let record = runner.confirmed_record()?;
            if let Some(record) = &record {
                ensure!(
                    record.spec == *spec,
                    "libkrun helper spec `{}` does not match configured filesystem `{}`",
                    record.spec,
                    spec.id()
                );
            }
            Ok(record.is_some())
        },
    }
}

async fn launch_and_confirm(workspace: &Workspace, spec: &fs::Spec, output: Output) -> Result<()> {
    match spec.runtime() {
        fs::Runtime::Host => crate::host_fs::launch(workspace.filesystem_state(), spec).await?,
        fs::Runtime::Docker => launch_docker(workspace, spec, output.clone()).await?,
        fs::Runtime::Libkrun => launch_libkrun(workspace, spec, output.clone()).await?,
    }
    if wait_for_attachment(workspace, spec).await {
        return Ok(());
    }

    let attach_error = anyhow::anyhow!(
        "filesystem `{}` mounted but did not attach to the daemon",
        spec.id()
    );
    match stop_runtime(workspace, spec, output).await {
        Ok(()) => Err(attach_error),
        Err(cleanup_error) => Err(attach_error.context(format!(
            "the failed filesystem also could not be detached safely: {cleanup_error:#}"
        ))),
    }
}

async fn stop_runtime(workspace: &Workspace, spec: &fs::Spec, output: Output) -> Result<()> {
    match spec.runtime() {
        fs::Runtime::Host => crate::host_fs::stop(workspace.filesystem_state(), spec).await,
        fs::Runtime::Docker => {
            let client = docker_client(workspace, spec, output)?;
            let container = client
                .confirmed_container_for_spec(workspace.identity().container_label(), spec)
                .await?;
            if let Some((identity, _running)) = container {
                client
                    .remove_confirmed(&identity, workspace.identity().container_label(), spec)
                    .await?;
            } else if client.is_running().await?.is_some() {
                bail!(
                    "filesystem `{}` has Docker state that cannot be proved safe; run `omnifs doctor`",
                    spec.id()
                );
            }
            Ok(())
        },
        fs::Runtime::Libkrun => {
            let runner = LibkrunRunner::new(workspace.filesystem_state().libkrun_root(spec.id()));
            let Some(record) = runner.confirmed_record()? else {
                return Ok(());
            };
            ensure!(
                record.spec == *spec,
                "libkrun helper spec does not match filesystem `{}`",
                spec.id()
            );
            runner.tear_down_confirmed(record).await
        },
    }
}

fn docker_client(workspace: &Workspace, spec: &fs::Spec, output: Output) -> Result<DockerClient> {
    let config = workspace.config()?;
    let image = resolve_filesystem_image(None, &config)?;
    let name = filesystem_container_name(workspace.identity().container_label(), spec.id())?;
    let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
    DockerClient::connect_for(&target, output)
}

async fn launch_docker(workspace: &Workspace, spec: &fs::Spec, output: Output) -> Result<()> {
    let config = workspace.config()?;
    let image = resolve_filesystem_image(None, &config)?;
    let name = filesystem_container_name(workspace.identity().container_label(), spec.id())?;
    let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
    let runtime = DockerClient::connect_ready(&target, "omnifs fs attach", output).await?;
    #[cfg(target_os = "linux")]
    let expected_ip = runtime.filesystem_attach_bind_ip().await?;
    #[cfg(not(target_os = "linux"))]
    let expected_ip = std::net::Ipv4Addr::LOCALHOST;
    let status = crate::client::DaemonClient::for_workspace(workspace)
        .status()
        .await?;
    let addr = status
        .attach_tcp
        .context("daemon has no TCP filesystem attach listener")?;
    let expected = SocketAddr::new(IpAddr::V4(expected_ip), workspace.attach_port()?.get());
    ensure!(
        addr == expected,
        "daemon attach listener is bound to {addr}, expected {expected}; restart the daemon"
    );
    runtime
        .launch(workspace.identity().container_label(), spec, addr.port())
        .await?;
    let identity = runtime
        .confirmed_filesystem(workspace.identity().container_label(), spec)
        .await?
        .context("the launched filesystem container did not retain its exact identity")?;
    if let Err(mount_error) = wait_for_docker_mount(&runtime).await {
        return match runtime
            .remove_confirmed(&identity, workspace.identity().container_label(), spec)
            .await
        {
            Ok(()) => Err(mount_error),
            Err(cleanup_error) => Err(mount_error.context(format!(
                "the failed filesystem container also could not be removed: {cleanup_error:#}"
            ))),
        };
    }
    Ok(())
}

async fn wait_for_docker_mount(runtime: &DockerClient) -> Result<()> {
    let deadline = tokio::time::Instant::now() + DOCKER_TIMEOUT;
    loop {
        if runtime.mount_ready(fs::GUEST_LOCATION).await? {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "{} did not appear inside the filesystem container within {}s",
                fs::GUEST_LOCATION,
                DOCKER_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn launch_libkrun(workspace: &Workspace, spec: &fs::Spec, output: Output) -> Result<()> {
    let config = workspace.config()?;
    let state = workspace.filesystem_state();
    let attach_socket = state.attach_socket();
    let guest_image_cache = state.guest_image_cache();
    let runner = LibkrunRunner::new(state.libkrun_root(spec.id()));
    let attached = async {
        ensure!(
            wait_for_attachment(workspace, spec).await,
            "filesystem `{}` did not attach to the daemon",
            spec.id()
        );
        Ok(())
    };
    runner
        .launch(
            LibkrunLaunchRequest {
                spec,
                daemon_attach_socket: &attach_socket,
                config: &config,
                guest_image_cache: &guest_image_cache,
                output,
                mount: None,
                timeout: LIBKRUN_TIMEOUT,
            },
            attached,
        )
        .await
}

async fn wait_for_attachment(workspace: &Workspace, spec: &fs::Spec) -> bool {
    let deadline = tokio::time::Instant::now() + ATTACH_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(status)) = crate::client::DaemonClient::for_workspace(workspace)
            .status_optional_checked()
            .await
            && status.filesystems.contains(spec)
        {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    false
}

async fn shell(args: ShellArgs, output: Output) -> Result<ExitCode> {
    ensure!(
        !output.is_structured(),
        "fs shell is a passthrough command and only supports human output"
    );
    let workspace = Workspace::resolve()?;
    let claim = workspace.filesystems().claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    ensure!(
        runtime_running(&workspace, &spec, output.clone()).await?,
        "filesystem `{}` is detached",
        spec.id()
    );
    ensure_attached(&workspace, &spec).await?;
    match spec.runtime() {
        fs::Runtime::Host => {
            let phase = crate::host_fs::phase(workspace.filesystem_state(), &spec)
                .await?
                .context("host filesystem runner disappeared")?;
            ensure!(
                phase == omnifs_thin::host_control::RunnerPhase::Mounted,
                "filesystem `{}` is not mounted; runner phase is {phase:?}",
                spec.id()
            );
            crate::ui::print_raw(&format!(
                "Filesystem `{}` is already available in your normal shell at {}.\n",
                spec.id(),
                spec.location().display()
            ));
            Ok(ExitCode::Success)
        },
        fs::Runtime::Docker => {
            let name =
                filesystem_container_name(workspace.identity().container_label(), spec.id())?;
            let target =
                DockerTarget::new(name.as_str().to_owned(), FILESYSTEM_DEV_IMAGE.to_owned())?;
            let client = DockerClient::connect_for(&target, output)?;
            propagate(
                client.shell_command(args.shell.as_deref(), &args.command),
                format!("open shell in filesystem container `{name}`"),
            )
        },
        fs::Runtime::Libkrun => {
            crate::libkrun_runner::ensure_socat_available()?;
            let runner = LibkrunRunner::new(workspace.filesystem_state().libkrun_root(spec.id()));
            propagate(
                runner.shell_command(args.shell.as_deref(), &args.command),
                format!("open shell in filesystem `{}`", spec.id()),
            )
        },
    }
}

async fn ensure_attached(workspace: &Workspace, spec: &fs::Spec) -> Result<()> {
    let status = crate::client::DaemonClient::for_workspace(workspace)
        .status_optional_checked()
        .await
        .context("cannot verify filesystem attachment state")?
        .context("the daemon is stopped")?;
    ensure!(
        status.filesystems.contains(spec),
        "filesystem `{}` is detached",
        spec.id()
    );
    Ok(())
}

fn propagate(mut command: Command, context: String) -> Result<ExitCode> {
    let status = command.status().with_context(|| context)?;
    ensure!(
        status.success(),
        "filesystem shell command exited with {status}"
    );
    Ok(ExitCode::Success)
}

async fn list(output: Output) -> Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let inventory = crate::inventory::Inventory::collect(&workspace).await?;
    let verdict = inventory.verdict();
    let rows = inventory
        .filesystems
        .iter()
        .map(|filesystem| ListRow {
            spec: filesystem.spec.clone(),
            state: filesystem.state.label(),
        })
        .collect::<Vec<_>>();
    let result = ListResult { filesystems: rows };
    if output.is_structured() {
        output.emit_result(verdict, &result)?;
    } else if result.filesystems.is_empty() {
        crate::ui::print_raw("No filesystems configured.\n");
    } else {
        let mut report = crate::ui::table::Report::new();
        report.push(crate::ui::table::Block::Resources(
            crate::status::filesystem_table(
                &inventory.filesystems,
                inventory.next_action().as_ref(),
            ),
        ));
        crate::ui::print_raw(&report.render());
    }
    Ok(match verdict {
        crate::inventory::Verdict::Ok => ExitCode::Success,
        crate::inventory::Verdict::Degraded => ExitCode::Degraded,
    })
}

fn finish_action(output: &Output, spec: fs::Spec, state: &'static str) -> Result<ExitCode> {
    let result = ActionResult {
        filesystem: spec,
        state,
    };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, &result)?;
    } else {
        crate::ui::print_raw(&format!(
            "{} filesystem `{}` ({}/{})\n",
            state,
            result.filesystem.id(),
            result.filesystem.protocol(),
            result.filesystem.runtime()
        ));
    }
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn grammar_uses_named_selectors_only() {
        let cli = crate::cli::Cli::try_parse_from([
            "omnifs",
            "fs",
            "create",
            "--name",
            "work",
            "--protocol",
            "nfs",
        ])
        .unwrap();
        let Some(crate::cli::Commands::Fs(args)) = cli.command else {
            panic!("expected fs command");
        };
        let FsCommand::Create(args) = args.command else {
            panic!("expected create");
        };
        assert_eq!(args.name.as_str(), "work");
        assert_eq!(args.protocol, Some(fs::Protocol::Nfs));

        assert!(crate::cli::Cli::try_parse_from(["omnifs", "fs", "attach", "work"]).is_err());

        let cli = crate::cli::Cli::try_parse_from([
            "omnifs",
            "fs",
            "shell",
            "--name",
            "work",
            "--",
            "sh",
            "-lc",
            "printf '%s' 'two words'",
        ])
        .unwrap();
        let Some(crate::cli::Commands::Fs(args)) = cli.command else {
            panic!("expected fs command");
        };
        let FsCommand::Shell(args) = args.command else {
            panic!("expected shell");
        };
        assert_eq!(
            args.command,
            ["sh", "-lc", "printf '%s' 'two words'"],
            "the parser must preserve argv boundaries"
        );
        assert!(
            crate::cli::Cli::try_parse_from([
                "omnifs", "fs", "shell", "--name", "work", "--shell", "/bin/zsh", "--", "pwd"
            ])
            .is_err(),
            "--shell and a command have distinct meanings"
        );
    }

    #[test]
    fn defaults_match_the_platform_matrix() {
        let expected = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", _) => Some((fs::Protocol::Fuse, fs::Runtime::Host)),
            ("macos", "aarch64") => Some((fs::Protocol::Fuse, fs::Runtime::Libkrun)),
            ("macos", _) => Some((fs::Protocol::Nfs, fs::Runtime::Host)),
            _ => None,
        };
        assert_eq!(platform_default(), expected);
    }
}
