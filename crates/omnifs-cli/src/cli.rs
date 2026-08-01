//! CLI type definitions: top-level parser and command enum.

use clap::{Args, Parser, Subcommand};
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::commands;
use crate::commands::doctor::DoctorVerdict;
use crate::error::ExitCode;
use crate::ui::output::{Output, OutputMode};

#[derive(Parser)]
#[command(
    name = "omnifs",
    version,
    about = "Project external services as files",
    after_help = "Exit codes:\n  0  success\n  1  generic failure\n  2  usage error\n  3  daemon unreachable\n  4  auth or consent required\n  5  degraded health\n  130  canceled"
)]
pub struct Cli {
    /// Increase tracing verbosity. -v = info, -vv = debug with span events.
    /// Overridden by `RUST_LOG`.
    #[arg(
        short = 'v',
        long = "verbose",
        action = clap::ArgAction::Count,
        global = true,
        help_heading = "Global options"
    )]
    pub verbose: u8,

    /// Output contract for this invocation.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = OutputMode::Human,
        help_heading = "Global options"
    )]
    pub output: OutputMode,

    /// Suppress conversational narration on stderr. Receipts, progress settle
    /// lines, and errors are preserved.
    #[arg(short = 'q', long, global = true, help_heading = "Global options")]
    pub quiet: bool,

    /// Reject prompts and browser handoffs.
    #[arg(long, global = true, help_heading = "Global options")]
    pub no_input: bool,

    /// Approve confirmation-only decisions.
    #[arg(long, global = true, help_heading = "Global options")]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Show resources and follow typed daemon work
    Status(StatusArgs),

    /// Preview the complete desired resource set from KCL
    Plan {
        /// KCL source file. Defaults to ./omnifs.k when it exists.
        path: Option<PathBuf>,
    },

    /// Apply the complete desired resource set from KCL
    Apply {
        /// KCL source file. Defaults to ./omnifs.k when it exists.
        path: Option<PathBuf>,
    },

    /// Stop the daemon and clean up
    ///
    /// Asks Filesystems to stop, drains them for a bounded time, then
    /// stops the daemon. Busy stragglers are reported for `omnifs doctor`.
    Down,
    /// Tail the daemon log
    Logs(commands::logs::LogsArgs),
    /// Stream FUSE, provider, and callout events
    Inspect(commands::inspect::InspectArgs),
    /// Add, list, show, or remove Provider resources
    Provider(commands::provider::ProviderArgs),

    /// Add, list, update, authenticate, or remove Mount resources
    Mount(commands::mount::MountArgs),

    /// Manage declared credential slots and their secret material
    Credential(commands::credential::CredentialArgs),

    /// Add, list, inspect, remove, restart, or enter Filesystems
    #[command(name = "fs")]
    Filesystem(commands::filesystem::FilesystemArgs),

    /// Start the daemon and offer a resource-based quick start
    #[command(after_help = "Examples:\n  omnifs setup")]
    Setup,

    /// Install omnifs usage skills for agent harnesses
    Skill(commands::skill::SkillArgs),

    /// Diagnose environment, auth, and daemon health
    Doctor,

    /// Print shell completions
    Completions(commands::completions::CompletionsArgs),

    /// Print version information
    ///
    /// Prints the one-line build identity.
    Version,

    /// Run the runtime daemon. Internal: launched by the host-native lifecycle
    /// command, not invoked directly. The daemon still runs as its own process
    /// over the local control socket; this is the same binary, not a separate entrypoint.
    #[command(hide = true)]
    Daemon,

    /// Run a host filesystem. Internal: launched by filesystem lifecycle commands.
    #[command(hide = true)]
    RunFs(omnifs_thin::RunFsArgs),
}

#[derive(Args, Debug, Clone)]
pub struct StatusArgs {
    /// Follow typed daemon progress instead of printing one status snapshot.
    #[arg(long)]
    pub follow: bool,
    /// Follow one desired revision to its terminal result.
    #[arg(long, requires = "follow", conflicts_with = "action")]
    pub revision: Option<omnifs_core::ResourceRevision>,
    /// Follow one durable action to its terminal result.
    #[arg(long, requires = "follow", conflicts_with = "revision")]
    pub action: Option<omnifs_core::ActionId>,
}

impl Cli {
    pub(crate) fn runs_daemon(&self) -> bool {
        if matches!(&self.command, Some(Commands::Daemon)) {
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
            Some(command) => Box::pin(command.run(output)).await,
            None => run_bare(output).await,
        }
    }
}

impl Commands {
    fn labels(&self) -> (Option<&'static str>, &'static str) {
        match self {
            Self::Status(_) => (Some("status"), "status"),
            Self::Plan { .. } => (Some("plan"), "plan"),
            Self::Apply { .. } => (Some("apply"), "apply"),
            Self::Down => (Some("down"), "down"),
            Self::Logs(_) => (Some("logs"), "logs"),
            Self::Inspect(_) => (Some("inspect"), "inspect"),
            Self::Provider(args) => (
                Some("provider"),
                match &args.command {
                    commands::provider::ProviderCommand::Add => "provider.add",
                    commands::provider::ProviderCommand::Ls => "provider.ls",
                    commands::provider::ProviderCommand::Show { .. } => "provider.show",
                    commands::provider::ProviderCommand::Rm { .. } => "provider.rm",
                },
            ),
            Self::Mount(args) => (
                Some("mount"),
                match &args.command {
                    commands::mount::MountCommand::Add => "mount.add",
                    commands::mount::MountCommand::Ls => "mount.ls",
                    commands::mount::MountCommand::Show { .. } => "mount.show",
                    commands::mount::MountCommand::Update { .. } => "mount.update",
                    commands::mount::MountCommand::Reauth { .. } => "mount.reauth",
                    commands::mount::MountCommand::Revoke { .. } => "mount.revoke",
                    commands::mount::MountCommand::Rm { .. } => "mount.rm",
                },
            ),
            Self::Credential(args) => (
                Some("credential"),
                match &args.command {
                    commands::credential::CredentialCommand::Login => "credential.login",
                    commands::credential::CredentialCommand::Set(_) => "credential.set",
                    commands::credential::CredentialCommand::Ls => "credential.ls",
                    commands::credential::CredentialCommand::Show { .. } => "credential.show",
                    commands::credential::CredentialCommand::Rm { .. } => "credential.rm",
                    commands::credential::CredentialCommand::Revoke { .. } => "credential.revoke",
                },
            ),
            Self::Filesystem(args) => (
                Some("fs"),
                match &args.command {
                    commands::filesystem::FilesystemCommand::Add => "fs.add",
                    commands::filesystem::FilesystemCommand::Ls => "fs.ls",
                    commands::filesystem::FilesystemCommand::Show { .. } => "fs.show",
                    commands::filesystem::FilesystemCommand::Rm { .. } => "fs.rm",
                    commands::filesystem::FilesystemCommand::Restart { .. } => "fs.restart",
                    commands::filesystem::FilesystemCommand::Shell(_) => "fs.shell",
                },
            ),
            Self::Setup => (Some("setup"), "setup"),
            Self::Skill(_) => (Some("skill"), "skill"),
            Self::Doctor => (Some("doctor"), "doctor"),
            Self::Completions(_) => (Some("completions"), "completions"),
            Self::Version => (Some("version"), "version"),
            Self::Daemon => (None, "daemon"),
            Self::RunFs(_) => (None, "run-fs"),
        }
    }

    pub(crate) fn command_path(&self) -> &'static str {
        self.labels().1
    }

    /// Top-level subcommand label for `cli.jsonl` usage metrics, or `None` for the
    /// internal `daemon` subcommand (which records its own usage stream instead of
    /// counting as CLI usage).
    pub(crate) fn usage_label(&self) -> Option<&'static str> {
        self.labels().0
    }

    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self {
            Self::Doctor => {
                let verdict = commands::doctor::run(output).await?;
                Ok(exit_for_verdict(verdict))
            },
            Self::Status(args) => {
                if args.follow {
                    let target = match (args.revision, args.action) {
                        (Some(revision), None) => {
                            commands::status::FollowTarget::Revision(revision)
                        },
                        (None, Some(action)) => commands::status::FollowTarget::Action(action),
                        (None, None) => commands::status::FollowTarget::Current,
                        (Some(_), Some(_)) => unreachable!("clap rejects conflicting targets"),
                    };
                    commands::status::follow(target, output).await
                } else {
                    commands::status::run(output).await
                }
            },
            Self::Plan { path } => commands::plan::run(path, output).await,
            Self::Apply { path } => commands::apply::run(path, output).await,
            Self::Down => commands::down::run(output).await,
            Self::Logs(args) => args.run(&output).await.map(|()| ExitCode::Success),
            Self::Inspect(args) => args.run(output).await.map(|()| ExitCode::Success),
            Self::Provider(args) => args.run(output).await,
            Self::Mount(args) => args.run(output).await,
            Self::Credential(args) => args.run(output).await,
            Self::Filesystem(args) => args.run(output).await,
            Self::Setup => Box::pin(commands::setup::run(output)).await,
            Self::Skill(args) => args.run(&output).map(|()| ExitCode::Success),
            Self::Completions(args) => args.run(&output).map(|()| ExitCode::Success),
            Self::Version => commands::version::run(output).await,
            Self::Daemon => omnifs_daemon::run().await.map(|()| ExitCode::Success),
            Self::RunFs(_) => {
                anyhow::bail!("run-fs must be dispatched before the CLI runtime starts")
            },
        }
    }
}

/// Bare `omnifs` adapts to the profile: a fresh profile with
/// no mounts at all shows a dedicated short screen instead of an empty
/// status report; a configured profile shows the shared status report
/// (`InventoryReport`, so this never drifts from `omnifs status`) closed by
/// the single next actionable step when stopped or the derived browse action
/// when running.
async fn run_bare(output: Output) -> anyhow::Result<ExitCode> {
    let inventory = crate::inventory::Inventory::collect_rpc().await?;
    let exit_code = match inventory.verdict() {
        crate::ui::output::ResultVerdict::Ok => ExitCode::Success,
        crate::ui::output::ResultVerdict::Degraded => ExitCode::Degraded,
    };
    if output.is_structured() {
        output.emit_result(inventory.verdict(), inventory)?;
        return Ok(exit_code);
    }

    if inventory.mounts.is_empty() {
        output.report(format!(
            "{}\n",
            fresh_profile_screen(&inventory, crate::ui::render::stdout_capabilities())
        ));
        return Ok(exit_code);
    }

    let report = crate::status::InventoryReport { inventory };
    let closing_action = report.closing_action();
    output.report(report.render().render());
    if let Some(action) = closing_action {
        output.narrate("");
        output.narrate(crate::ui::access::ActionLine::from(&action).render());
    }
    Ok(exit_code)
}

/// A label column width fitting both "Get started:" (12) and "or piecewise:"
/// (13), the two rows `fresh_profile_block` prints.
const FRESH_LABEL_WIDTH: usize = 14;
/// A command column width fitting both "omnifs setup" (12) and "omnifs mount
/// add" (16) with a 4-column gap before the description.
const FRESH_CMD_WIDTH: usize = 20;

/// One `<label> <accent(cmd)> <dim(desc)>` row of `fresh_profile_block`,
/// column-aligned against its sibling row rather than against the general
/// ledger primitives.
fn fresh_profile_row(
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

/// Bare `omnifs` on a profile with no mounts at all: no status
/// probe, no empty report, just the two ways to get started.
fn fresh_profile_block(caps: crate::ui::render::Capabilities) -> String {
    let intro = crate::ui::render::sentence(
        "No mounts yet. omnifs projects external services as files.",
        caps,
    );
    let get_started = fresh_profile_row(
        "Get started:",
        "omnifs setup",
        "boot the daemon and quick-start mounts",
        caps,
    );
    let piecewise = fresh_profile_row(
        "or piecewise:",
        "omnifs mount add",
        "configure one mount",
        caps,
    );
    format!("{intro}\n\n{get_started}\n{piecewise}")
}

/// The full bare-`omnifs` screen for a mount-less profile: the get-started
/// block, plus (when the inventory verdict is degraded) the one fact behind
/// exit 5, so the exit code is never unexplained even though this screen
/// skips the status report entirely.
fn fresh_profile_screen(
    inventory: &crate::inventory::Inventory,
    caps: crate::ui::render::Capabilities,
) -> String {
    let mut screen = fresh_profile_block(caps);
    if let Some((what, fix)) = fresh_profile_degradation(inventory) {
        let _ = write!(screen, "\n\n{what}:  `{fix}`");
    }
    screen
}

/// The one actionable fact behind a `Degraded` verdict on a mount-less
/// profile, if any: `Inventory::verdict` (inventory.rs) has two disjuncts
/// that can still fire when `mounts` is empty (a daemon that failed or went
/// unreachable, or a filesystem severe enough to flip the verdict while the
/// daemon is otherwise up), and the mount-related disjuncts are moot on an
/// empty mount list. Returns the label and the one action selected by
/// `Inventory::next_action`.
fn fresh_profile_degradation(inventory: &crate::inventory::Inventory) -> Option<(String, String)> {
    let action = inventory.next_action()?;
    let command = crate::ui::access::ActionLine::from(&action).command;
    match action {
        crate::inventory::NextAction::Doctor {
            target: crate::inventory::ActionTarget::Profile,
        } => Some((
            match inventory.daemon_health() {
                crate::inventory::DaemonHealth::Unreachable => "Daemon is unreachable",
                _ => "Profile needs attention",
            }
            .to_owned(),
            command,
        )),
        crate::inventory::NextAction::Doctor {
            target: crate::inventory::ActionTarget::Filesystem(id),
        } => inventory
            .filesystems
            .iter()
            .find(|filesystem| filesystem.name == id)
            .map(|filesystem| {
                (
                    format!(
                        "{} ({}) filesystem is {}",
                        filesystem.spec.protocol().as_str(),
                        filesystem.spec.runtime().as_str(),
                        filesystem.state.label()
                    ),
                    command,
                )
            }),
        _ => None,
    }
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

    use super::{Cli, fresh_profile_block};

    fn caps(color: bool) -> crate::ui::render::Capabilities {
        crate::ui::render::Capabilities { width: 120, color }
    }

    /// The fresh-profile screen:
    /// ```text
    /// No mounts yet. omnifs projects external services as files.
    ///
    /// Get started:  omnifs setup        boot the daemon and quick-start mounts
    /// or piecewise: omnifs mount add    configure one mount
    /// ```
    #[test]
    fn fresh_profile_block_matches_the_documented_shape() {
        assert_eq!(
            fresh_profile_block(caps(false)),
            "No mounts yet. omnifs projects external services as files.\n\
             \n\
             Get started:  omnifs setup        boot the daemon and quick-start mounts\n\
             or piecewise: omnifs mount add    configure one mount"
        );
    }

    #[test]
    fn fresh_profile_block_accents_only_the_commands() {
        let rendered = fresh_profile_block(caps(true));
        let plain = crate::ui::strip_ansi(&rendered);
        assert_eq!(plain, fresh_profile_block(caps(false)));
        assert!(rendered.contains(&crate::ui::style::accent("omnifs setup", true)));
        assert!(rendered.contains(&crate::ui::style::accent("omnifs mount add", true)));
    }

    /// A genuinely clean fresh profile (no mounts, nothing degraded) keeps
    /// the plain get-started screen and exits 0 (the bug this guards against:
    /// a mount-less profile with a degraded inventory used to exit 5 while
    /// showing this same clean screen with no fact explaining the code).
    #[test]
    fn fresh_profile_screen_stays_plain_and_ok_when_nothing_is_degraded() {
        let inventory = crate::inventory::Inventory::test(
            crate::inventory::DaemonHealth::Stopped,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(inventory.verdict(), crate::ui::output::ResultVerdict::Ok);
        assert_eq!(super::fresh_profile_degradation(&inventory), None);
        assert_eq!(
            super::fresh_profile_screen(&inventory, caps(false)),
            fresh_profile_block(caps(false))
        );
    }

    /// An unreachable daemon flips the verdict to `Degraded` (exit 5) even
    /// with zero mounts; the screen must name it and reuse
    /// the Inventory-selected doctor action.
    #[test]
    fn fresh_profile_screen_names_an_unreachable_daemon() {
        let inventory = crate::inventory::Inventory::test(
            crate::inventory::DaemonHealth::Unreachable,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            inventory.verdict(),
            crate::ui::output::ResultVerdict::Degraded
        );
        assert_eq!(
            super::fresh_profile_degradation(&inventory),
            Some((
                "Daemon is unreachable".to_owned(),
                "omnifs doctor".to_owned()
            ))
        );
        let screen = super::fresh_profile_screen(&inventory, caps(false));
        assert!(screen.starts_with(&fresh_profile_block(caps(false))));
        assert!(
            screen.contains("Daemon is unreachable:  `omnifs doctor`"),
            "{screen}"
        );
    }

    /// A failed Filesystem can flip the verdict to `Degraded` while the daemon
    /// is otherwise running and there are still zero mounts; the screen must
    /// name that Filesystem and reuse its own `fix` field verbatim.
    #[test]
    fn fresh_profile_screen_names_a_failed_filesystem_while_daemon_is_up() {
        let filesystem = crate::inventory::FilesystemAccessStatus {
            name: "test".parse().unwrap(),
            spec: omnifs_core::FilesystemSpec::new(
                omnifs_core::FilesystemProtocol::Fuse,
                omnifs_core::FilesystemRuntime::Docker,
                omnifs_core::FILESYSTEM_GUEST_LOCATION.into(),
                None,
                None,
            )
            .unwrap(),
            state: crate::inventory::FilesystemAccessState::Failed,
            mount_count: 0,
            fix: Some("omnifs logs (container exited)".to_owned()),
        };
        let inventory = crate::inventory::Inventory::test(
            crate::inventory::DaemonHealth::Running,
            vec![filesystem],
            Vec::new(),
        );
        assert_eq!(
            inventory.verdict(),
            crate::ui::output::ResultVerdict::Degraded
        );
        assert_eq!(
            super::fresh_profile_degradation(&inventory),
            Some((
                "fuse (docker) filesystem is failed".to_owned(),
                "omnifs doctor".to_owned()
            ))
        );
        let screen = super::fresh_profile_screen(&inventory, caps(false));
        assert!(
            screen.contains("fuse (docker) filesystem is failed:  `omnifs doctor`"),
            "{screen}"
        );
    }

    #[test]
    fn only_confirmation_flows_keep_non_interactive_approval() {
        let command = Cli::command();
        for subcommand in ["setup", "doctor"] {
            assert!(has_arg(&command, subcommand, "yes"));
        }
        for retired in [
            "provider",
            "name",
            "scheme",
            "no-browser",
            "token-env",
            "no-auth",
            "config-json",
            "limits-json",
            "token",
        ] {
            assert!(
                !has_arg(&command, "mount add", retired),
                "interactive mount authoring still accepts `{retired}`"
            );
        }
    }

    #[test]
    fn removed_lifecycle_commands_are_not_in_the_user_grammar() {
        for argv in [
            &["omnifs", "up"][..],
            &["omnifs", "up", "--offline"][..],
            &["omnifs", "fs", "attach", "main"][..],
            &["omnifs", "fs", "detach", "main"][..],
        ] {
            let Err(error) = Cli::try_parse_from(argv) else {
                panic!("obsolete command parsed: {argv:?}");
            };
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn plan_and_apply_are_in_the_user_grammar() {
        for argv in [
            &["omnifs", "plan"][..],
            &["omnifs", "plan", "resources.k"][..],
            &["omnifs", "apply", "--yes"][..],
            &["omnifs", "apply", "resources.k", "--yes"][..],
        ] {
            assert!(
                Cli::try_parse_from(argv).is_ok(),
                "declarative command did not parse: {argv:?}"
            );
        }
    }

    #[test]
    fn credential_commands_use_resource_names_and_env_only_secrets() {
        for argv in [
            &["omnifs", "credential", "login"][..],
            &["omnifs", "credential", "ls"][..],
            &["omnifs", "credential", "show", "work"][..],
            &["omnifs", "credential", "rm", "work"][..],
            &["omnifs", "credential", "revoke", "work"][..],
            &[
                "omnifs",
                "credential",
                "set",
                "work",
                "--from-env",
                "OMNIFS_TEST_TOKEN",
            ][..],
        ] {
            assert!(Cli::try_parse_from(argv).is_ok(), "{argv:?}");
        }
        assert!(Cli::try_parse_from(["omnifs", "credential", "rm"]).is_err());
        assert!(
            Cli::try_parse_from(["omnifs", "credential", "set", "work", "--token", "secret"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "omnifs",
                "credential",
                "rm",
                "--provider",
                "github",
                "--scheme",
                "oauth",
                "--account",
                "work",
            ])
            .is_err()
        );
    }

    #[test]
    fn help_groups_invocation_options_and_drops_mount_authoring_flags() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("Global options:"), "{help}");

        let command = Cli::command();
        let mount_add = command
            .find_subcommand("mount")
            .and_then(|mount| mount.find_subcommand("add"))
            .expect("mount add");
        for retired in [
            "name",
            "as",
            "provider",
            "config-json",
            "limits-json",
            "token",
            "token-env",
        ] {
            assert!(
                !mount_add
                    .get_arguments()
                    .any(|arg| { arg.get_id() == retired || arg.get_long() == Some(retired) }),
                "mount add still accepts `{retired}`"
            );
        }
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
    /// and check whether the leaf subcommand declares the argument. An
    /// unknown path segment panics rather than silently falling back to the
    /// global-only check: a coverage row naming a subcommand that was
    /// renamed or removed must fail loudly, not quietly pass on whatever
    /// global args happen to exist.
    fn has_arg(command: &clap::Command, subcommand: &str, arg: &str) -> bool {
        let global = command
            .get_arguments()
            .any(|candidate| candidate.get_id() == arg || candidate.get_long() == Some(arg));
        let mut current = command;
        for segment in subcommand.split_whitespace() {
            current = current.find_subcommand(segment).unwrap_or_else(|| {
                panic!("unknown subcommand path segment `{segment}` in `{subcommand}`")
            });
        }
        global
            || current
                .get_arguments()
                .any(|candidate| candidate.get_id() == arg || candidate.get_long() == Some(arg))
    }
}
