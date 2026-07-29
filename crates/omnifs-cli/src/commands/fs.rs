//! Named filesystem configuration and lifecycle.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Args, Subcommand};
use omnifs_core::{ClientOwnerId, fs};
use serde::Serialize;

use crate::client_fs_state::ClientFilesystemState;
use crate::error::ExitCode;
use crate::filesystem_driver::{Confirmed, FilesystemDriver};
use crate::rpc::RpcClient;
use crate::ui::output::{Output, ResultVerdict};

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
            FsCommand::Attach(args) => Box::pin(attach(args, output)).await,
            FsCommand::Detach(args) => detach(args, output).await,
            FsCommand::Restart(args) => Box::pin(restart(args, output)).await,
            FsCommand::Shell(args) => shell(args, output).await,
            FsCommand::Ls => list(output).await,
        }
    }
}

impl CreateArgs {
    fn run(self, output: &Output) -> Result<ExitCode> {
        let client_state = ClientFilesystemState::resolve()?;
        let spec = resolve_spec(&client_state, self)?;
        client_state.registry().claim(spec.id())?.create(&spec)?;
        finish_action(output, spec, "configured")
    }
}

fn resolve_spec(client_state: &ClientFilesystemState, args: CreateArgs) -> Result<fs::Spec> {
    let (protocol, runtime) = resolve_pair(args.protocol, args.runtime)?;
    ensure!(
        supports(protocol, runtime),
        "{protocol}/{runtime} is not supported on {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let location = match runtime {
        fs::Runtime::Host => args
            .location
            .unwrap_or_else(|| client_state.default_host_location(&args.name)),
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

/// Every filesystem this platform recommends by default. Shared by `omnifs
/// setup`'s filesystem quick-start offer and `mount add`'s adaptive closing
/// hint, so both point at the same recommendation.
pub(crate) fn recommended_filesystems() -> Vec<(fs::Protocol, fs::Runtime)> {
    available_filesystems()
        .into_iter()
        .filter(|&(protocol, runtime)| default_runtime(protocol) == Some(runtime))
        .collect()
}

/// The id `omnifs setup`'s filesystem quick-start would offer first, if this
/// platform recommends any filesystem at all. Shared with `mount add`'s
/// adaptive closing hint, which names the same id when no host filesystem is
/// attached yet.
pub(crate) fn recommended_filesystem_id() -> Result<Option<fs::Id>> {
    recommended_filesystems()
        .into_iter()
        .next()
        .map(|(protocol, runtime)| fs::Id::new(format!("{protocol}-{runtime}")))
        .transpose()
        .map_err(Into::into)
}

/// The location a filesystem would be attached at, whether or not it has
/// been configured yet: the existing configured spec's location if one is
/// already claimed under `id`, else the runtime's default. Shared by
/// [`ensure_setup_filesystem`] (which creates the spec at this same location
/// when none exists yet) and setup's quick-start prompt
/// ([`preview_filesystem_location`]), which previews it before the user
/// accepts.
fn default_location_for(
    client_state: &ClientFilesystemState,
    runtime: fs::Runtime,
    id: &fs::Id,
) -> PathBuf {
    match runtime {
        fs::Runtime::Host => client_state.default_host_location(id),
        fs::Runtime::Docker | fs::Runtime::Libkrun => PathBuf::from(fs::GUEST_LOCATION),
    }
}

/// The location `omnifs setup`'s quick-start filesystem prompt would attach
/// `protocol`/`runtime` at, without creating or attaching anything. Read-only
/// preview built from the same claim-or-default logic
/// [`ensure_setup_filesystem`] uses when it actually creates the spec, so the
/// question text and the settled fact never disagree with each other.
pub(crate) fn preview_filesystem_location(
    client_state: &ClientFilesystemState,
    protocol: fs::Protocol,
    runtime: fs::Runtime,
) -> Result<PathBuf> {
    let id = fs::Id::new(format!("{protocol}-{runtime}"))?;
    if let Some(spec) = client_state.registry().claim(&id)?.get()? {
        return Ok(spec.location().to_path_buf());
    }
    Ok(default_location_for(client_state, runtime, &id))
}

pub(crate) async fn ensure_setup_filesystem(
    protocol: fs::Protocol,
    runtime: fs::Runtime,
    output: Output,
) -> Result<()> {
    let client_state = ClientFilesystemState::resolve()?;
    let id = fs::Id::new(format!("{protocol}-{runtime}"))?;
    let registry = client_state.registry();
    let claim = registry.claim(&id)?;
    let spec = if let Some(spec) = claim.get()? {
        ensure!(
            spec.protocol() == protocol && spec.runtime() == runtime,
            "configured filesystem `{id}` does not match setup's {protocol}/{runtime} choice"
        );
        spec
    } else {
        let location = default_location_for(&client_state, runtime, &id);
        let spec = fs::Spec::new(id, protocol, runtime, location)?;
        claim.create(&spec)?;
        spec
    };
    let attached = RpcClient::resolve()?
        .inventory()
        .await?
        .attachments
        .contains(&spec);
    if attached {
        return Ok(());
    }
    if runtime_running(&client_state, &spec, output.clone()).await? {
        ensure!(
            wait_for_attachment(&spec).await,
            "filesystem `{}` is running but did not attach to the daemon",
            spec.id()
        );
        return Ok(());
    }
    Box::pin(launch_and_confirm(&client_state, &spec, output)).await
}

async fn rm(args: NameArgs, output: Output) -> Result<ExitCode> {
    let client_state = ClientFilesystemState::resolve()?;
    let registry = client_state.registry();
    let claim = registry.claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    ensure_not_attached(&spec).await?;
    ensure!(
        !runtime_running(&client_state, &spec, output.clone()).await?,
        "filesystem `{}` is still running; detach it first",
        spec.id()
    );
    claim.remove()?;
    finish_action(&output, spec, "removed")
}

async fn attach(args: NameArgs, output: Output) -> Result<ExitCode> {
    let client_state = ClientFilesystemState::resolve()?;
    let registry = client_state.registry();
    let claim = registry.claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    ensure!(
        supports(spec.protocol(), spec.runtime()),
        "{}/{} is not supported on this platform",
        spec.protocol(),
        spec.runtime()
    );
    crate::commands::daemon_start::start().await?;
    let inventory = RpcClient::resolve()?
        .inventory()
        .await
        .context("daemon did not become ready before attaching a filesystem")?;
    if let Some(attached) = inventory
        .attachments
        .iter()
        .find(|row| row.id() == spec.id())
    {
        ensure!(
            attached == &spec,
            "filesystem `{}` is attached with different settings; run `omnifs doctor`",
            spec.id()
        );
        return finish_action(&output, spec, "already_attached");
    }
    ensure!(
        !runtime_running(&client_state, &spec, output.clone()).await?,
        "filesystem `{}` already has a running {} instance",
        spec.id(),
        spec.runtime()
    );
    Box::pin(launch_and_confirm(&client_state, &spec, output.clone())).await?;
    finish_action(&output, spec, "attached")
}

async fn detach(args: NameArgs, output: Output) -> Result<ExitCode> {
    let client_state = ClientFilesystemState::resolve()?;
    let registry = client_state.registry();
    let claim = registry.claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    stop_runtime(&client_state, &spec, output.clone()).await?;
    ensure!(
        !runtime_running(&client_state, &spec, output.clone()).await?,
        "filesystem `{}` still has a running {} instance",
        spec.id(),
        spec.runtime()
    );
    finish_action(&output, spec, "detached")
}

async fn restart(args: NameArgs, output: Output) -> Result<ExitCode> {
    let client_state = ClientFilesystemState::resolve()?;
    let registry = client_state.registry();
    let claim = registry.claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    ensure!(
        supports(spec.protocol(), spec.runtime()),
        "{}/{} is not supported on this platform",
        spec.protocol(),
        spec.runtime()
    );
    crate::commands::daemon_start::start().await?;
    RpcClient::resolve()?
        .inventory()
        .await
        .context("daemon did not become ready before restarting a filesystem")?;
    stop_runtime(&client_state, &spec, output.clone()).await?;
    Box::pin(launch_and_confirm(&client_state, &spec, output.clone())).await?;
    finish_action(&output, spec, "attached")
}

fn required_spec(claim: &crate::client_fs_state::Claim<'_>, id: &fs::Id) -> Result<fs::Spec> {
    claim
        .get()?
        .with_context(|| format!("filesystem `{id}` is not configured"))
}

async fn ensure_not_attached(spec: &fs::Spec) -> Result<()> {
    let inventory = RpcClient::resolve()?
        .inventory()
        .await
        .context("cannot prove the daemon attachment state; run `omnifs doctor`")?;
    ensure!(
        !inventory
            .attachments
            .iter()
            .any(|row| row.id() == spec.id()),
        "filesystem `{}` is attached; detach it first",
        spec.id()
    );
    Ok(())
}

async fn runtime_running(
    client_state: &ClientFilesystemState,
    spec: &fs::Spec,
    output: Output,
) -> Result<bool> {
    let driver = FilesystemDriver::for_spec(client_state, spec, output)?;
    match driver
        .confirmed(client_state, client_owner_id()?, spec)
        .await?
    {
        None => Ok(false),
        Some(Confirmed::Docker(_, false)) => bail!(
            "filesystem `{}` has a stopped Docker container; run `omnifs doctor`",
            spec.id()
        ),
        Some(_) => Ok(true),
    }
}

async fn launch_and_confirm(
    client_state: &ClientFilesystemState,
    spec: &fs::Spec,
    output: Output,
) -> Result<()> {
    let client_owner = client_owner_id()?;
    let driver = FilesystemDriver::for_spec(client_state, spec, output.clone())?;
    driver
        .launch(client_state, client_owner, spec, output.clone())
        .await?;
    if wait_for_attachment(spec).await {
        return Ok(());
    }

    let attach_error = anyhow::anyhow!(
        "filesystem `{}` mounted but did not attach to the daemon",
        spec.id()
    );
    match stop_runtime(client_state, spec, output).await {
        Ok(()) => Err(attach_error),
        Err(cleanup_error) => Err(attach_error.context(format!(
            "the failed filesystem also could not be detached safely: {cleanup_error:#}"
        ))),
    }
}

async fn stop_runtime(
    client_state: &ClientFilesystemState,
    spec: &fs::Spec,
    output: Output,
) -> Result<()> {
    let client_owner = client_owner_id()?;
    let driver = FilesystemDriver::for_spec(client_state, spec, output)?;
    let Some(confirmed) = driver.confirmed(client_state, client_owner, spec).await? else {
        return Ok(());
    };
    driver
        .stop_confirmed(client_state, client_owner, spec, confirmed)
        .await
}

pub(crate) fn client_owner_id() -> Result<ClientOwnerId> {
    crate::client_state::ClientState::resolve()?.owner_id()
}

pub(crate) async fn wait_for_attachment(spec: &fs::Spec) -> bool {
    // Any resolve/inventory failure is transient and treated the same as
    // "not attached yet": keep polling rather than aborting, so `check`
    // never actually returns `Err`.
    crate::process::poll_until(ATTACH_TIMEOUT, POLL, || async {
        let attached = match RpcClient::resolve() {
            Ok(rpc) => match rpc.inventory().await {
                Ok(inventory) => inventory.attachments.contains(spec),
                Err(_) => false,
            },
            Err(_) => false,
        };
        Ok(attached.then_some(()))
    })
    .await
    .unwrap_or(None)
    .is_some()
}

async fn shell(args: ShellArgs, output: Output) -> Result<ExitCode> {
    ensure!(
        !output.is_structured(),
        "fs shell is a passthrough command and only supports human output"
    );
    let client_state = ClientFilesystemState::resolve()?;
    let registry = client_state.registry();
    let claim = registry.claim(&args.name)?;
    let spec = required_spec(&claim, &args.name)?;
    let client_owner = client_owner_id()?;
    let driver = FilesystemDriver::for_spec(&client_state, &spec, output)?;
    ensure!(
        driver
            .confirmed(&client_state, client_owner, &spec)
            .await?
            .is_some(),
        "filesystem `{}` is detached",
        spec.id()
    );
    ensure_attached(&spec).await?;

    // Entering a host filesystem is a `cd` into an already-visible mount, not
    // a remote exec, so it stays outside the driver's `shell_command`, which
    // returns `None` for host.
    if let FilesystemDriver::Host(runner) = &driver {
        let (_, phase) = runner
            .confirmed(&spec)
            .await?
            .context("host filesystem runner disappeared")?;
        ensure!(
            phase == omnifs_thin::host_control::RunnerPhase::Mounted,
            "filesystem `{}` is not mounted; runner phase is {phase:?}",
            spec.id()
        );
        return if let Some(program) = args.command.first() {
            let mut command = Command::new(program);
            command
                .args(&args.command[1..])
                .current_dir(spec.location());
            propagate(
                command,
                format!("run command in filesystem `{}`", spec.id()),
            )
        } else if let Some(shell) = args.shell.as_deref() {
            let mut command = Command::new(shell);
            command.current_dir(spec.location());
            propagate(command, format!("open shell in filesystem `{}`", spec.id()))
        } else {
            crate::ui::print_raw(&format!(
                "Filesystem `{}` is available at {}.\n",
                spec.id(),
                spec.location().display()
            ));
            Ok(ExitCode::Success)
        };
    }

    if matches!(driver, FilesystemDriver::Libkrun(_)) {
        crate::libkrun_runner::ensure_socat_available()?;
    }
    let label = match &driver {
        FilesystemDriver::Docker(client) => {
            format!(
                "open shell in filesystem container `{}`",
                client.container_name()
            )
        },
        FilesystemDriver::Host(_) | FilesystemDriver::Libkrun(_) => {
            format!("open shell in filesystem `{}`", spec.id())
        },
    };
    let command = driver
        .shell_command(args.shell.as_deref(), &args.command)
        .context("filesystem driver has no shell command")?;
    propagate(command, label)
}

async fn ensure_attached(spec: &fs::Spec) -> Result<()> {
    let inventory = RpcClient::resolve()?
        .inventory()
        .await
        .context("cannot verify filesystem attachment state")?;
    ensure!(
        inventory.attachments.contains(spec),
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
    let inventory = crate::inventory::Inventory::collect_rpc().await?;
    let filesystems = inventory.filesystems;
    let verdict = if filesystems
        .iter()
        .any(|filesystem| filesystem.state.severity() >= crate::inventory::Severity::Attention)
    {
        crate::ui::output::ResultVerdict::Degraded
    } else {
        crate::ui::output::ResultVerdict::Ok
    };
    let next_action = filesystems
        .iter()
        .find(|filesystem| filesystem.state.severity() >= crate::inventory::Severity::Attention)
        .map(|filesystem| crate::inventory::NextAction::Doctor {
            target: crate::inventory::ActionTarget::Filesystem(filesystem.spec.id().clone()),
        });
    let rows = filesystems
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
            crate::status::filesystem_table(&filesystems, next_action.as_ref()),
        ));
        crate::ui::print_raw(&report.render());
    }
    Ok(match verdict {
        crate::ui::output::ResultVerdict::Ok => ExitCode::Success,
        crate::ui::output::ResultVerdict::Degraded => ExitCode::Degraded,
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
        let rows = [
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "filesystem",
                result.filesystem.id().to_string(),
            ),
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "protocol",
                result.filesystem.protocol().to_string(),
            ),
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "runtime",
                result.filesystem.runtime().to_string(),
            ),
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "location",
                result.filesystem.location().display().to_string(),
            ),
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "state",
                state.replace('_', " "),
            ),
        ];
        let width = crate::ui::render::ledger_key_width(&rows);
        for row in &rows {
            output.ledger_row(row, width);
        }
        match state {
            "configured" => output.outro(format!(
                "Filesystem `{}` is configured but detached. Attach it: `omnifs fs attach --name {}`",
                result.filesystem.id(),
                result.filesystem.id()
            )),
            "attached" | "already_attached"
                if result.filesystem.runtime() == fs::Runtime::Host =>
            {
                output.outro(format!(
                    "Files are at {}.",
                    result.filesystem.location().display()
                ));
            },
            "attached" | "already_attached" => output.outro(format!(
                "Enter it: `omnifs fs shell --name {}`",
                result.filesystem.id()
            )),
            "detached" => output.outro(format!(
                "Filesystem `{}` is detached; its configuration remains.",
                result.filesystem.id()
            )),
            "removed" => output.outro(format!(
                "Filesystem `{}` configuration removed.",
                result.filesystem.id()
            )),
            _ => {},
        }
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
