//! CLI type definitions: top-level parser and command enum.

use clap::{Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::fmt::Write as _;

use crate::commands;
use crate::commands::doctor::DoctorVerdict;
use crate::error::ExitCode;
use crate::ui::output::{Output, OutputMode};
use omnifs_workspace::Workspace;

#[derive(Parser)]
#[command(
    name = "omnifs",
    version,
    about = "omnifs: a virtual filesystem for everything",
    after_help = "Exit codes:\n  0  success\n  1  generic failure\n  2  usage error\n  3  daemon unreachable\n  4  auth or consent required\n  5  degraded health"
)]
pub struct Cli {
    /// Increase tracing verbosity. -v = info, -vv = debug with span events.
    /// Overridden by `RUST_LOG`.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Output contract for this invocation.
    #[arg(long, global = true, value_enum, default_value_t = OutputMode::Human)]
    pub output: OutputMode,

    /// Suppress conversational narration on stderr. Receipts, progress settle
    /// lines, and errors are preserved.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Reject prompts and browser handoffs.
    #[arg(long, global = true)]
    pub no_input: bool,

    /// Approve confirmation-only decisions.
    #[arg(long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Show daemon, mount, and auth status
    Status(commands::status::StatusArgs),

    /// Start the daemon and serve configured mounts
    ///
    /// Spawns or replaces the host-native daemon over configured mounts.
    /// Frontends are managed separately with `omnifs frontend enable`.
    #[command(visible_alias = "apply")]
    Up(commands::up::UpArgs),
    /// Stop the daemon and clean up
    ///
    /// Stops only the daemon; independent frontend runners stay alive and
    /// reconnect when a daemon is started again.
    Down(commands::down::DownArgs),
    /// Tail the daemon log
    Logs(commands::logs::LogsArgs),
    /// Stream FUSE, provider, and callout events
    Inspect(commands::inspect::InspectArgs),
    /// Add, list, reauthenticate, revoke, or remove mounts
    Mount(commands::mount::MountArgs),

    /// Configure providers, start the daemon, and enable platform frontends
    Setup(commands::setup::SetupArgs),

    /// Install omnifs usage skills for agent harnesses
    Skill(commands::skill::SkillArgs),

    /// Diagnose environment, auth, and daemon health
    Doctor(commands::doctor::DoctorArgs),

    /// Print shell completions
    Completions(commands::completions::CompletionsArgs),

    /// Print version information
    ///
    /// Prints the one-line build identity.
    Version(commands::version::VersionArgs),

    /// Run the runtime daemon. Internal: launched by the host-native lifecycle
    /// command, not invoked directly. The daemon still runs as its own process
    /// over the local control socket; this is the same binary, not a separate entrypoint.
    #[command(hide = true)]
    Daemon(crate::daemon::DaemonArgs),

    /// Warm retained providers. Internal: launched as detached cache work.
    #[command(hide = true)]
    WarmProviders(crate::provider_warmup::WarmProvidersArgs),

    /// Manage filesystem frontends attached to the host-native daemon
    ///
    /// Enable, disable, restart, or list FUSE and NFS frontends. Every
    /// frontend renders the daemon's same shared namespace and carries no
    /// provider credentials.
    Frontend(commands::frontend::FrontendArgs),
}

impl Cli {
    pub(crate) fn runs_daemon(&self) -> bool {
        if matches!(&self.command, Some(Commands::Daemon(_))) {
            return true;
        }
        false
    }

    pub(crate) fn usage_label(&self) -> Option<&'static str> {
        self.command
            .as_ref()
            .map_or(Some("bare"), Commands::usage_label)
    }

    pub(crate) fn command_path(&self) -> &'static str {
        self.command
            .as_ref()
            .map_or("status", Commands::command_path)
    }

    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            Some(command) => command.run(output).await,
            None => run_bare(output).await,
        }
    }
}

impl Commands {
    fn labels(&self) -> (Option<&'static str>, &'static str) {
        match self {
            Self::Status(_) => (Some("status"), "status"),
            Self::Up(_) => (Some("up"), "up"),
            Self::Down(_) => (Some("down"), "down"),
            Self::Logs(_) => (Some("logs"), "logs"),
            Self::Inspect(_) => (Some("inspect"), "inspect"),
            Self::Mount(args) => (
                Some("mount"),
                match &args.command {
                    commands::mount::MountCommand::Add(_) => "mount.add",
                    commands::mount::MountCommand::Ls(_) => "mount.ls",
                    commands::mount::MountCommand::Show(_) => "mount.show",
                    commands::mount::MountCommand::Reauth(_) => "mount.reauth",
                    commands::mount::MountCommand::Revoke(_) => "mount.revoke",
                    commands::mount::MountCommand::Rm { .. } => "mount.rm",
                },
            ),
            Self::Setup(_) => (Some("setup"), "setup"),
            Self::Skill(_) => (Some("skill"), "skill"),
            Self::Doctor(_) => (Some("doctor"), "doctor"),
            Self::Completions(_) => (Some("completions"), "completions"),
            Self::Version(_) => (Some("version"), "version"),
            Self::Daemon(_) => (None, "daemon"),
            Self::WarmProviders(_) => (None, "warm-providers"),
            // Every `frontend` subcommand shares one usage label; there is
            // no longer a hidden internal one (like `daemon`'s) to exclude.
            Self::Frontend(args) => (
                Some("frontend"),
                match &args.command {
                    commands::frontend::FrontendCommand::Enable(_) => "frontend.enable",
                    commands::frontend::FrontendCommand::Disable(_) => "frontend.disable",
                    commands::frontend::FrontendCommand::Restart(_) => "frontend.restart",
                    commands::frontend::FrontendCommand::Ls(_) => "frontend.ls",
                    commands::frontend::FrontendCommand::Shell(_) => "frontend.shell",
                },
            ),
        }
    }

    pub(crate) fn command_path(&self) -> &'static str {
        self.labels().1
    }

    /// Top-level subcommand label for `cli.jsonl` usage metrics, or `None` for the
    /// internal `daemon` subcommand (which records `daemon.jsonl` instead of
    /// counting as CLI usage).
    pub(crate) fn usage_label(&self) -> Option<&'static str> {
        self.labels().0
    }

    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self {
            Self::Doctor(args) => {
                let verdict = args.run(output).await?;
                Ok(exit_for_verdict(verdict))
            },
            Self::Status(args) => args.run(output).await,
            Self::Up(args) => args.run(output).await,
            Self::Down(args) => args.run(output).await,
            Self::Logs(args) => args.run(&output).map(|()| ExitCode::Success),
            Self::Inspect(args) => args.run(output).await.map(|()| ExitCode::Success),
            Self::Mount(args) => args.run(output).await,
            Self::Setup(args) => args.run(output).await,
            Self::Skill(args) => args.run(&output).map(|()| ExitCode::Success),
            Self::Completions(args) => args.run(&output).map(|()| ExitCode::Success),
            Self::Version(args) => args.run(output).await,
            Self::Daemon(args) => crate::daemon::run(&args).await.map(|()| ExitCode::Success),
            Self::WarmProviders(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Frontend(args) => args.run(output).await,
        }
    }
}

/// Inspect raw argv just far enough to choose a structured error contract for
/// a later Clap usage failure. Invalid, missing, duplicate, and post-`--`
/// occurrences deliberately return `None`, leaving Clap's human error path in
/// charge.
pub(crate) fn raw_output_mode<I>(args: I) -> Option<OutputMode>
where
    I: IntoIterator<Item = OsString>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    let mut selected = None;
    let mut occurrences = 0_u8;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].to_string_lossy();
        if arg == "--" {
            break;
        }
        let value = if let Some(value) = arg.strip_prefix("--output=") {
            occurrences = occurrences.saturating_add(1);
            Some(value.to_owned())
        } else if arg == "--output" {
            occurrences = occurrences.saturating_add(1);
            index = index.saturating_add(1);
            args.get(index)
                .map(|value| value.to_string_lossy().into_owned())
        } else {
            index = index.saturating_add(1);
            continue;
        };
        if let Some(value) = value
            && let Ok(mode) = OutputMode::from_str(&value, true)
        {
            selected = Some(mode);
        } else {
            selected = None;
        }
        index = index.saturating_add(1);
    }
    (occurrences == 1).then_some(selected).flatten()
}

pub(crate) fn raw_command_path<I>(args: I) -> &'static str
where
    I: IntoIterator<Item = OsString>,
{
    let values = args
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut positional = Vec::new();
    let mut index = 0;
    while index < values.len() {
        let value = &values[index];
        if value == "--" {
            break;
        }
        if value == "--output" {
            index = index.saturating_add(2);
            continue;
        }
        if value.starts_with("--output=")
            || value == "--quiet"
            || value == "-q"
            || value == "--yes"
            || value == "--no-input"
            || value == "--verbose"
            || value.starts_with("-v")
        {
            index = index.saturating_add(1);
            continue;
        }
        if !value.starts_with('-') {
            positional.push(value.clone());
            if positional.len() == 2 {
                break;
            }
        }
        index = index.saturating_add(1);
    }
    match positional.as_slice() {
        [command, subcommand, ..] if command == "mount" && subcommand == "add" => "mount.add",
        [command, subcommand, ..] if command == "mount" && subcommand == "ls" => "mount.ls",
        [command, subcommand, ..] if command == "mount" && subcommand == "show" => "mount.show",
        [command, subcommand, ..] if command == "mount" && subcommand == "reauth" => "mount.reauth",
        [command, subcommand, ..] if command == "mount" && subcommand == "revoke" => "mount.revoke",
        [command, subcommand, ..] if command == "mount" && subcommand == "rm" => "mount.rm",
        [command, subcommand, ..] if command == "frontend" && subcommand == "enable" => {
            "frontend.enable"
        },
        [command, subcommand, ..] if command == "frontend" && subcommand == "disable" => {
            "frontend.disable"
        },
        [command, subcommand, ..] if command == "frontend" && subcommand == "restart" => {
            "frontend.restart"
        },
        [command, subcommand, ..] if command == "frontend" && subcommand == "ls" => "frontend.ls",
        [command, subcommand, ..] if command == "frontend" && subcommand == "shell" => {
            "frontend.shell"
        },
        [command, ..] if command == "apply" => "up",
        [command, ..] if command == "status" => "status",
        [command, ..] if command == "up" => "up",
        [command, ..] if command == "down" => "down",
        [command, ..] if command == "logs" => "logs",
        [command, ..] if command == "inspect" => "inspect",
        [command, ..] if command == "setup" => "setup",
        [command, ..] if command == "skill" => "skill",
        [command, ..] if command == "doctor" => "doctor",
        [command, ..] if command == "completions" => "completions",
        [command, ..] if command == "version" => "version",
        [command, ..] if command == "daemon" => "daemon",
        _ => "status",
    }
}

/// Bare `omnifs` adapts to the workspace: a fresh workspace with
/// no mounts at all shows a dedicated short screen instead of an empty
/// status report; a configured workspace shows the shared status report
/// (`InventoryReport`, so this never drifts from `omnifs status`) closed by
/// the single next actionable step, `Start serving:  omnifs up` when
/// stopped or the derived browse action when running.
async fn run_bare(output: Output) -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let inventory = crate::inventory::Inventory::collect(&workspace).await?;
    let exit_code = match inventory.verdict() {
        crate::inventory::Verdict::Ok => ExitCode::Success,
        crate::inventory::Verdict::Degraded => ExitCode::Degraded,
    };
    if output.is_structured() {
        output.emit_result(inventory.verdict(), inventory)?;
        return Ok(exit_code);
    }

    if inventory.mounts.is_empty() {
        crate::ui::print_raw(&format!(
            "{}\n",
            fresh_workspace_screen(&inventory, crate::ui::render::stdout_capabilities())
        ));
        return Ok(exit_code);
    }

    let running = inventory.daemon_state() == crate::inventory::DaemonState::Running;
    let report = crate::status::InventoryReport { inventory };
    report.render().print();
    output.narrate("");
    if running {
        output.narrate(format!(
            "Browse:  `{}`",
            crate::ui::access::browse_command(&report.inventory)
        ));
    } else {
        output.narrate("Start serving:  `omnifs up`");
    }
    Ok(exit_code)
}

/// A label column width fitting both "Get started:" (12) and "or piecewise:"
/// (13), the two rows `fresh_workspace_block` prints.
const FRESH_LABEL_WIDTH: usize = 14;
/// A command column width fitting both "omnifs setup" (12) and "omnifs mount
/// add" (16) with a 4-column gap before the description.
const FRESH_CMD_WIDTH: usize = 20;

/// One `<label> <accent(cmd)> <dim(desc)>` row of `fresh_workspace_block`,
/// column-aligned against its sibling row rather than against the general
/// ledger primitives.
fn fresh_workspace_row(
    label: &str,
    cmd: &str,
    desc: &str,
    caps: crate::ui::render::Capabilities,
) -> String {
    let label_pad = FRESH_LABEL_WIDTH.saturating_sub(label.chars().count());
    let cmd_pad = FRESH_CMD_WIDTH.saturating_sub(cmd.chars().count());
    format!(
        "{label}{}{}{}{}",
        " ".repeat(label_pad),
        crate::ui::style::accent(cmd, caps.color),
        " ".repeat(cmd_pad),
        crate::ui::style::dim(desc, caps.color)
    )
}

/// Bare `omnifs` on a workspace with no mounts at all: no status
/// probe, no empty report, just the two ways to get started.
fn fresh_workspace_block(caps: crate::ui::render::Capabilities) -> String {
    let intro = crate::ui::render::sentence(
        "No mounts yet. omnifs projects external services as files.",
        caps,
    );
    let get_started = fresh_workspace_row(
        "Get started:",
        "omnifs setup",
        "pick services, sign in, mount",
        caps,
    );
    let piecewise = fresh_workspace_row(
        "or piecewise:",
        "omnifs mount add",
        "configure one mount",
        caps,
    );
    format!("{intro}\n\n{get_started}\n{piecewise}")
}

/// The full bare-`omnifs` screen for a mount-less workspace: the get-started
/// block, plus (when the inventory verdict is degraded) the one fact behind
/// exit 5, so the exit code is never unexplained even though this screen
/// skips the status report entirely.
fn fresh_workspace_screen(
    inventory: &crate::inventory::Inventory,
    caps: crate::ui::render::Capabilities,
) -> String {
    let mut screen = fresh_workspace_block(caps);
    if let Some((what, fix)) = fresh_workspace_degradation(inventory) {
        let _ = write!(screen, "\n\n{what}:  `{fix}`");
    }
    screen
}

/// The one actionable fact behind a `Degraded` verdict on a mount-less
/// workspace, if any: `Inventory::verdict` (inventory.rs) has two disjuncts
/// that can still fire when `mounts` is empty (a daemon that failed or went
/// unreachable, or a frontend severe enough to flip the verdict while the
/// daemon is otherwise up), and the mount-related disjuncts are moot on an
/// empty mount list. Returns the label to show and the fix command to run;
/// the fix command is always `DaemonState::context_fix` or the frontend's
/// own `fix` field, never re-derived here.
fn fresh_workspace_degradation(
    inventory: &crate::inventory::Inventory,
) -> Option<(String, String)> {
    let daemon_state = inventory.daemon_state();
    let daemon_label = match daemon_state {
        crate::inventory::DaemonState::Failed => Some("Daemon is unhealthy"),
        crate::inventory::DaemonState::Unreachable => Some("Daemon is unreachable"),
        _ => None,
    };
    if let Some(label) = daemon_label
        && let Some(fix) = daemon_state.context_fix()
    {
        return Some((label.to_owned(), fix.to_owned()));
    }

    let daemon_up = matches!(
        daemon_state,
        crate::inventory::DaemonState::Running
            | crate::inventory::DaemonState::Starting
            | crate::inventory::DaemonState::Degraded
    );
    if !daemon_up {
        return None;
    }
    inventory.frontends.iter().find_map(|frontend| {
        if frontend.state.severity() < crate::inventory::Severity::Attention {
            return None;
        }
        let fix = frontend.fix.clone()?;
        Some((
            format!(
                "{} ({}) frontend is {}",
                frontend.filesystem.label(),
                frontend.runtime.label(),
                frontend.state.label()
            ),
            fix,
        ))
    })
}

fn exit_for_verdict(verdict: DoctorVerdict) -> ExitCode {
    match verdict {
        DoctorVerdict::Clean => ExitCode::Success,
        DoctorVerdict::Failures | DoctorVerdict::Warnings => ExitCode::Degraded,
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use std::ffi::OsString;

    use super::{Cli, Commands, fresh_workspace_block, raw_command_path, raw_output_mode};

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn caps(color: bool) -> crate::ui::render::Capabilities {
        crate::ui::render::Capabilities {
            width: 120,
            is_tty: color,
            color,
            quiet: false,
        }
    }

    /// The fresh-workspace screen:
    /// ```text
    /// No mounts yet. omnifs projects external services as files.
    ///
    /// Get started:  omnifs setup        pick services, sign in, mount
    /// or piecewise: omnifs mount add    configure one mount
    /// ```
    #[test]
    fn fresh_workspace_block_matches_the_documented_shape() {
        assert_eq!(
            fresh_workspace_block(caps(false)),
            "No mounts yet. omnifs projects external services as files.\n\
             \n\
             Get started:  omnifs setup        pick services, sign in, mount\n\
             or piecewise: omnifs mount add    configure one mount"
        );
    }

    #[test]
    fn fresh_workspace_block_accents_only_the_commands() {
        let rendered = fresh_workspace_block(caps(true));
        let plain = crate::ui::strip_ansi(&rendered);
        assert_eq!(plain, fresh_workspace_block(caps(false)));
        assert!(rendered.contains(&crate::ui::style::accent("omnifs setup", true)));
        assert!(rendered.contains(&crate::ui::style::accent("omnifs mount add", true)));
    }

    /// A genuinely clean fresh workspace (no mounts, nothing degraded) keeps
    /// the plain get-started screen and exits 0 (the bug this guards against:
    /// a mount-less workspace with a degraded inventory used to exit 5 while
    /// showing this same clean screen with no fact explaining the code).
    #[test]
    fn fresh_workspace_screen_stays_plain_and_ok_when_nothing_is_degraded() {
        let inventory = crate::inventory::Inventory::test(
            crate::inventory::DaemonState::Stopped,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(inventory.verdict(), crate::inventory::Verdict::Ok);
        assert_eq!(super::fresh_workspace_degradation(&inventory), None);
        assert_eq!(
            super::fresh_workspace_screen(&inventory, caps(false)),
            fresh_workspace_block(caps(false))
        );
    }

    /// An unreachable daemon flips the verdict to `Degraded` (exit 5) even
    /// with zero mounts; the screen must name it and reuse
    /// `DaemonState::context_fix` verbatim rather than re-deriving the fix.
    #[test]
    fn fresh_workspace_screen_names_an_unreachable_daemon() {
        let inventory = crate::inventory::Inventory::test(
            crate::inventory::DaemonState::Unreachable,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(inventory.verdict(), crate::inventory::Verdict::Degraded);
        assert_eq!(
            super::fresh_workspace_degradation(&inventory),
            Some((
                "Daemon is unreachable".to_owned(),
                crate::inventory::DaemonState::Unreachable
                    .context_fix()
                    .unwrap()
                    .to_owned()
            ))
        );
        let screen = super::fresh_workspace_screen(&inventory, caps(false));
        assert!(screen.starts_with(&fresh_workspace_block(caps(false))));
        assert!(
            screen.contains("Daemon is unreachable:  `omnifs logs`"),
            "{screen}"
        );
    }

    /// A leftover failed frontend observation (e.g. a stale entry under
    /// `cache/frontends`) can flip the verdict to `Degraded` while the daemon
    /// is otherwise running and there are still zero mounts; the screen must
    /// name that frontend and reuse its own `fix` field verbatim.
    #[test]
    fn fresh_workspace_screen_names_a_failed_frontend_while_daemon_is_up() {
        let frontend = crate::inventory::FrontendStatus {
            filesystem: crate::commands::frontend::FrontendFilesystem::Fuse,
            runtime: crate::commands::frontend::FrontendRuntime::Docker,
            location: None,
            state: crate::inventory::FrontendState::Failed,
            scope: "all",
            mount_count: 0,
            fix: Some("omnifs logs (container exited)".to_owned()),
        };
        let inventory = crate::inventory::Inventory::test(
            crate::inventory::DaemonState::Running,
            vec![frontend],
            Vec::new(),
        );
        assert_eq!(inventory.verdict(), crate::inventory::Verdict::Degraded);
        assert_eq!(
            super::fresh_workspace_degradation(&inventory),
            Some((
                "fuse (docker) frontend is failed".to_owned(),
                "omnifs logs (container exited)".to_owned()
            ))
        );
        let screen = super::fresh_workspace_screen(&inventory, caps(false));
        assert!(
            screen.contains("fuse (docker) frontend is failed:  `omnifs logs (container exited)`"),
            "{screen}"
        );
    }

    #[test]
    fn raw_output_mode_accepts_global_flag_anywhere_before_double_dash() {
        assert_eq!(
            raw_output_mode(argv(&["omnifs", "status", "--output", "json"])),
            Some(crate::ui::output::OutputMode::Json)
        );
        assert_eq!(
            raw_output_mode(argv(&["omnifs", "--output=jsonl", "mount", "ls"])),
            Some(crate::ui::output::OutputMode::Jsonl)
        );
        assert_eq!(
            raw_output_mode(argv(&["omnifs", "status", "--", "--output", "json"])),
            None
        );
    }

    #[test]
    fn raw_output_mode_rejects_ambiguous_or_invalid_occurrences() {
        assert_eq!(
            raw_output_mode(argv(&["omnifs", "status", "--output"])),
            None
        );
        assert_eq!(
            raw_output_mode(argv(&["omnifs", "status", "--output", "yaml"])),
            None
        );
        assert_eq!(
            raw_output_mode(argv(&[
                "omnifs",
                "status",
                "--output",
                "json",
                "--output=jsonl"
            ])),
            None
        );
    }

    #[test]
    fn raw_command_path_skips_global_options_and_names_nested_commands() {
        assert_eq!(
            raw_command_path(argv(&["--output", "json", "mount", "show", "x"])),
            "mount.show"
        );
        assert_eq!(
            raw_command_path(argv(&["--yes", "--no-input", "status"])),
            "status"
        );
        assert_eq!(
            raw_command_path(argv(&["apply", "--output", "json", "--unknown"])),
            "up"
        );
        let sentinel = "CLI_SECRET_SENTINEL_RAW_COMMAND";
        assert_eq!(
            raw_command_path(argv(&["--output", "json", "--token", sentinel])),
            "status"
        );
    }

    #[test]
    fn prompt_sites_have_non_interactive_flags() {
        let command = Cli::command();
        let table = [
            ("mount add provider selection", "mount add", "provider"),
            ("mount add mount name collision", "mount add", "as"),
            ("mount add auth scheme", "mount add", "scheme"),
            ("mount add OAuth browser", "mount add", "no-browser"),
            ("mount add static token", "mount add", "token-env"),
            ("mount add auth suppression", "mount add", "no-auth"),
            ("mount add provider config", "mount add", "config-json"),
            ("mount add resource limits", "mount add", "limits-json"),
            ("up readiness wait", "up", "wait"),
        ];

        for (prompt, subcommand, arg) in table {
            assert!(
                has_arg(&command, subcommand, arg),
                "prompt site `{prompt}` must be covered by `{subcommand}` arg `{arg}`"
            );
        }
    }

    #[test]
    fn apply_alias_parses_identically_to_up() {
        let up = Cli::try_parse_from(["omnifs", "up", "--wait", "3s", "--offline"]).unwrap();
        let apply = Cli::try_parse_from(["omnifs", "apply", "--wait", "3s", "--offline"]).unwrap();
        let (Commands::Up(up), Commands::Up(apply)) = (up.command.unwrap(), apply.command.unwrap())
        else {
            panic!("up and apply must parse to Commands::Up");
        };
        assert_eq!(up.wait, apply.wait);
        assert_eq!(up.offline, apply.offline);
    }

    #[test]
    fn help_wraps_at_requested_terminal_width() {
        let help = Cli::command().term_width(35).render_help().to_string();
        assert!(
            help.lines().any(|line| line.contains("Increase tracing")),
            "expected the verbose option in help:\n{help}"
        );
        assert!(
            help.contains("Increase tracing\n") && help.contains("          verbosity."),
            "expected the verbose description to wrap at 35 columns:\n{help}"
        );
    }

    /// Resolve a whitespace-separated subcommand path (for example `mount add`)
    /// and check whether the leaf subcommand declares the argument.
    fn has_arg(command: &clap::Command, subcommand: &str, arg: &str) -> bool {
        let global = command
            .get_arguments()
            .any(|candidate| candidate.get_id() == arg || candidate.get_long() == Some(arg));
        let mut current = command;
        for segment in subcommand.split_whitespace() {
            let Some(next) = current.find_subcommand(segment) else {
                return global;
            };
            current = next;
        }
        global
            || current
                .get_arguments()
                .any(|candidate| candidate.get_id() == arg || candidate.get_long() == Some(arg))
    }
}
