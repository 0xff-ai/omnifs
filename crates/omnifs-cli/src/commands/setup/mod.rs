//! `omnifs setup`: guided onboarding walkthrough.
//!
//! A single ledger drives the whole wizard: an environment summary, a mount
//! point question, a provider picker, a per-provider block for each
//! selection, and a launch. Every human line prints on stderr through the
//! `crate::ui` design system; stdout is reserved for machine output. The
//! daemon always runs host-native, so there is no runtime-backend stage: the
//! wizard surfaces Docker reachability and asks for a host mount point only
//! when the effective `[[frontends]]` plan (explicit config, else the
//! platform default) actually launches those frontend kinds — a guest-only
//! config, for instance, needs neither.

pub mod host_os;

use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::{Provider, ProviderManifest};

use crate::commands::mount;
use crate::frontend_backend::Driver;
use crate::launch::{LaunchOutcome, Launcher};
use crate::stages::PromptMode;
use crate::ui;
use crate::ui::picker::PickerRow;
use crate::workspace::Workspace;

use self::host_os::HostOs;

#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // mirrors CLI flags 1:1
pub struct SetupArgs {
    /// Skip the final daemon launch.
    #[arg(long)]
    pub no_up: bool,
    /// Skip confirmations; auto-accept detected ambient credentials.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Fail instead of prompting. Use flags or --yes for every answer.
    #[arg(long)]
    pub no_input: bool,
    /// Mount point for the local frontend, when the effective `[[frontends]]`
    /// plan includes one.
    #[arg(long, value_name = "PATH")]
    pub mount_point: Option<PathBuf>,
    /// Preselect providers and skip the picker.
    #[arg(long, value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
}

/// How the shared configure tail titles its rail phases.
#[derive(Clone, Copy)]
enum StageStyle {
    Wizard,
    Hub,
}

impl StageStyle {
    fn phase(self, n: usize, title: &str) -> String {
        match self {
            Self::Wizard => format!("{n}/5 {title}"),
            Self::Hub => title.to_string(),
        }
    }
}

/// The outcome of configuring one provider during setup.
enum MountOutcome {
    Ready,
    Skipped,
}

impl MountOutcome {
    fn from_status(status: crate::stages::MountInitStatus) -> Self {
        match status {
            crate::stages::MountInitStatus::Ready => Self::Ready,
            crate::stages::MountInitStatus::SignInDeclined => Self::Skipped,
        }
    }
}

struct InitResult {
    mount_name: String,
    outcome: MountOutcome,
}

impl SetupArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let mode = PromptMode::from_flags(self.yes, self.no_input);
        let mut session = crate::ui::session::Session::intro("omnifs setup")?;

        let os = HostOs::detect();
        let workspace = Workspace::resolve()?;
        let paths = workspace.layout();
        let config = workspace.config()?;
        let environment = crate::stages::environment_check(os, &workspace)?;
        crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;
        std::fs::create_dir_all(&paths.mounts_dir)
            .with_context(|| format!("create {}", paths.mounts_dir.display()))?;

        // Review mode: a configured workspace, no explicit providers, no --yes.
        // A looping hub that owns its own actions.
        if environment.configured && self.providers.is_empty() && !self.yes {
            return self.review_mode(&workspace, mode, &mut session).await;
        }

        // Fresh mode: orientation + environment ledger.
        session.note("omnifs mounts your services as regular files.");
        session.note("One daemon, one mount point, your standard tools.");
        let installed = crate::catalog::installed_providers(workspace.catalog())?;
        if installed.is_empty() {
            anyhow::bail!("no built-in or plugin providers are available");
        }
        session.phase("1/5 environment");
        session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Done,
            "environment",
            os.name(),
        ));
        session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Done,
            "providers",
            format!("{} installed", installed.len()),
        ));
        let frontend_plan = resolve_setup_frontend_plan(&config, os)?;
        // Docker reachability matters only when the effective `[[frontends]]`
        // plan actually launches a Docker frontend; a Linux host or a
        // guest-only config needs no container.
        if frontend_plan
            .iter()
            .any(|entry| entry.driver == Driver::Docker)
        {
            Self::render_docker_row(&config, &mut session).await;
        }

        session.phase("2/5 mount point");
        let mount_point = if frontend_plan
            .iter()
            .any(|entry| entry.driver == Driver::Local)
        {
            Some(self.resolve_mount_point(mode, &mut session)?)
        } else {
            session.note(
                "no local frontend is configured; skipping the host mount point (see `omnifs frontend up`)",
            );
            None
        };

        self.configure_and_launch(
            &workspace,
            mount_point,
            mode,
            StageStyle::Wizard,
            &mut session,
        )
        .await
    }

    /// The informational Docker reachability row for the environment stage. It
    /// never fails setup; an unreachable daemon just notes the retry hint.
    async fn render_docker_row(
        config: &crate::config::Config,
        session: &mut crate::ui::session::Session,
    ) {
        match crate::stages::probe_docker_reachability(config).await {
            crate::stages::DockerReachability::Running { version } => {
                session.row(crate::ui::report::Row::new(
                    crate::ui::style::Glyph::Done,
                    "docker",
                    format!("{version} running"),
                ));
            },
            crate::stages::DockerReachability::Unreachable => {
                session.row(crate::ui::report::Row::new(
                    crate::ui::style::Glyph::Warn,
                    "docker",
                    "not reachable",
                ));
                session.note(
                    "start Docker Desktop so `omnifs up` can start the FUSE frontend; native NFS keeps working without it",
                );
            },
        }
    }

    /// Resolve the mount point and print the post-answer note. The daemon
    /// always runs host-native, so this question always applies.
    fn resolve_mount_point(
        &self,
        mode: PromptMode,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<PathBuf> {
        let mount_point = crate::stages::mount_point_resolution(self.mount_point.clone(), mode)?;
        let display = WorkspaceLayout::display(&mount_point);
        // State the fact when the prompt was skipped (--mount-point or --yes).
        if self.mount_point.is_some() || mode.yes {
            session.row(crate::ui::report::Row::new(
                crate::ui::style::Glyph::Done,
                "mount point",
                &display,
            ));
        }
        session.note(format!("files appear at {display}"));
        // Launch reads OMNIFS_MOUNT_POINT; a typed/flagged value only previews
        // unless it is exported.
        let already = crate::config::env_string("OMNIFS_MOUNT_POINT")
            .is_some_and(|env| WorkspaceLayout::display(&PathBuf::from(env)) == display);
        if !already {
            session.note(format!(
                "`export OMNIFS_MOUNT_POINT={display}` to persist it"
            ));
        }
        Ok(mount_point)
    }

    /// Shared tail: pick providers, configure each, launch, and close.
    /// `mount_point` is `None` for a guest-only `[[frontends]]` plan (no
    /// local frontend to report a host path for).
    async fn configure_and_launch(
        &self,
        workspace: &Workspace,
        mount_point: Option<PathBuf>,
        mode: PromptMode,
        style: StageStyle,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<()> {
        let installed = crate::catalog::installed_providers(workspace.catalog())?;
        let mounts = workspace.mounts()?;
        let configured = crate::catalog::configured_mounts(workspace.catalog(), &mounts)?;

        session.phase(style.phase(3, "what should omnifs mount?"));
        let selected = self.resolve_selection(&installed, &configured, mode, session)?;

        // Nothing new to configure (all providers already configured, or the
        // picker was confirmed empty): from the hub, return to it without the
        // launch narration. The fresh wizard falls through to its own
        // "no mounts yet" handling below.
        if selected.is_empty() && matches!(style, StageStyle::Hub) {
            return Ok(());
        }

        // Time-to-first-file: in the fresh wizard, when at least one selected
        // provider needs no auth, bring those mounts up and prove a real read
        // before any browser sign-in begins. The rail stays a single linear
        // spine: the fast mounts' whole section (their configuration, the
        // daemon launch, and the first-read proof) finishes, and only then do
        // the auth-requiring providers sign in. They attach to the
        // already-running daemon, so no relaunch is needed.
        if matches!(style, StageStyle::Wizard) && !self.no_up {
            let (fast, rest) = Self::split_fast_lane(&selected, &installed);
            if !fast.is_empty() {
                return self
                    .fast_lane_launch(&fast, &rest, &installed, workspace, mount_point, session)
                    .await;
            }
        }

        // A provider configuration or credential failure is a command failure,
        // not a soft skip. Returning the original error preserves its exit
        // code and context for scripts and for the top-level renderer. Only an
        // explicit sign-in decline becomes `MountOutcome::Skipped`.
        let results = self
            .run_init_loop(&selected, &installed, workspace, style, 4, session)
            .await?;

        let any_ready = results
            .iter()
            .any(|r| matches!(r.outcome, MountOutcome::Ready))
            || !configured.is_empty();

        if self.no_up {
            session.note("daemon launch skipped (--no-up); run `omnifs up` when ready");
            Self::print_closer(&results, None, session);
            return Ok(());
        }
        if !any_ready {
            session.outro("No mounts yet. Add one with `omnifs mount add <provider>`.");
            return Ok(());
        }

        let outcome = self
            .launch_and_report(workspace, mount_point.as_deref(), &results, style, session)
            .await?;
        Self::print_closer(&results, Some(&outcome), session);
        Ok(())
    }

    /// Partition selected provider names into the no-auth fast lane and the
    /// auth-requiring rest, preserving each group's original order so the
    /// transcript is deterministic. A provider is fast when its manifest
    /// declares no auth (the same predicate `--yes` auto-selection uses).
    fn split_fast_lane(
        selected: &[String],
        installed: &[(Provider, ProviderManifest)],
    ) -> (Vec<String>, Vec<String>) {
        selected.iter().cloned().partition(|name| {
            crate::catalog::find_installed(installed, name)
                .is_some_and(|(_, manifest)| manifest.auth.is_none())
        })
    }

    /// The time-to-first-file spine: configure the no-auth mounts (phase 4),
    /// launch the daemon and prove a read on the first of them (phase 5), then
    /// sign the auth-requiring providers in afterward (phase 5, applied live to
    /// the running daemon). Only the fresh wizard reaches this path, and only
    /// when a launch is wanted (`!--no-up`) and `fast` is non-empty.
    async fn fast_lane_launch(
        &self,
        fast: &[String],
        rest: &[String],
        installed: &[(Provider, ProviderManifest)],
        workspace: &Workspace,
        mount_point: Option<PathBuf>,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<()> {
        let mut results = self
            .run_init_loop(fast, installed, workspace, StageStyle::Wizard, 4, session)
            .await?;

        // Launch on the fast mounts alone. The daemon reads the specs just
        // written and serves them; the first-read proof runs against the first
        // Ready mount here, before any browser round trip.
        let outcome = self
            .launch_and_report(
                workspace,
                mount_point.as_deref(),
                &results,
                StageStyle::Wizard,
                session,
            )
            .await?;

        // The auth-requiring providers sign in below. `configure_mount`'s
        // persist step applies each spec to the now-running daemon (its
        // create-if-ready path), so these mounts go live without a relaunch.
        let rest_results = self
            .run_init_loop(rest, installed, workspace, StageStyle::Wizard, 5, session)
            .await?;
        results.extend(rest_results);

        Self::print_closer(&results, Some(&outcome), session);
        Ok(())
    }

    async fn launch_and_report(
        &self,
        workspace: &Workspace,
        mount_point: Option<&std::path::Path>,
        results: &[InitResult],
        style: StageStyle,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<LaunchOutcome> {
        session.phase(style.phase(5, "launch"));
        // `Launcher::launch` writes its own stderr progress lines; a spinner
        // here would be overwritten mid-line by them. Print a plain note before
        // and settle into a static row after.
        session.note("starting the daemon");
        let outcome = match Launcher::new(workspace, "omnifs setup").launch().await {
            Ok(outcome) => outcome,
            Err(error) => {
                session.row(crate::ui::report::Row::new(
                    crate::ui::style::Glyph::Fail,
                    "daemon",
                    one_line(&error),
                ));
                return Err(error);
            },
        };

        let mp = outcome
            .mount_point
            .clone()
            .or_else(|| mount_point.map(std::path::Path::to_path_buf));
        let daemon = match &mp {
            Some(mp) => format!("running; local mount at {}", WorkspaceLayout::display(mp)),
            None => "running (no local frontend attached; see `omnifs frontend up`)".to_string(),
        };
        session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Done,
            "daemon",
            daemon,
        ));

        if let Some(mount) = results
            .iter()
            .find(|r| matches!(r.outcome, MountOutcome::Ready))
            .map(|r| r.mount_name.as_str())
        {
            match crate::stages::verify_first_read(&outcome, mount) {
                Ok(read) => {
                    let entries = read.output.lines().count();
                    // Show the actual listing (bounded) so the read is visibly
                    // real, then the summary row with the count.
                    for line in read.output.lines().take(5) {
                        session.note(line);
                    }
                    session.row(crate::ui::report::Row::new(
                        crate::ui::style::Glyph::Done,
                        "first read",
                        format!("{} ({entries} entries)", read.command),
                    ));
                },
                Err(error) => {
                    session.row(crate::ui::report::Row::new(
                        crate::ui::style::Glyph::Warn,
                        "first read",
                        "failed; run omnifs doctor",
                    ));
                    session.note(one_line(&error));
                },
            }
        }
        Ok(outcome)
    }

    /// The `--yes` auto-selection: every installed, unconfigured provider that
    /// can complete without user interaction. Prints why the rest were left out.
    fn yes_auto_select(
        installed: &[(Provider, ProviderManifest)],
        configured: &std::collections::BTreeMap<String, String>,
        session: &mut crate::ui::session::Session,
    ) -> Vec<String> {
        let mut selected = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for (provider, manifest) in installed {
            let name = provider.meta.name.to_string();
            if configured.contains_key(&name) {
                continue;
            }
            let requires_prompt = manifest
                .config
                .as_ref()
                .is_some_and(omnifs_workspace::provider::ConfigMetadata::requires_prompt);
            let ambient =
                !crate::commands::mount::detect::detect(manifest.wasm_auth_manifest().as_ref())
                    .is_empty();
            if requires_prompt {
                skipped.push(format!("{name} (needs configuration)"));
            } else if manifest.auth.is_none() || ambient {
                selected.push(name);
            } else {
                let reason = if matches!(
                    manifest.default_scheme(),
                    Some((_, omnifs_workspace::authn::AuthScheme::Oauth(_)))
                ) {
                    "needs browser sign-in"
                } else {
                    "needs an API key"
                };
                skipped.push(format!("{name} ({reason})"));
            }
        }
        if !selected.is_empty() {
            session.note(format!("auto-selected {}", selected.join(", ")));
        }
        for entry in &skipped {
            session.note(format!("skipped {entry}"));
        }
        selected
    }

    /// The `You're set.` graduation card: any skipped providers first, then one
    /// row per Ready mount naming where its files live, then the daily-command
    /// hints. `outcome` is `None` when the daemon was not launched (`--no-up`),
    /// which also swaps the first hint to `omnifs up`.
    fn print_closer(
        results: &[InitResult],
        outcome: Option<&LaunchOutcome>,
        session: &mut crate::ui::session::Session,
    ) {
        // Surface explicit sign-in declines before the closer. Failures return
        // from the configure tail and are rendered by the top-level error
        // handler, preserving their original exit code.
        for result in results {
            match &result.outcome {
                MountOutcome::Skipped => {
                    session.row(crate::ui::report::Row::new(
                        crate::ui::style::Glyph::Skip,
                        &result.mount_name,
                        "skipped",
                    ));
                },
                MountOutcome::Ready => {},
            }
        }
        // One row per Ready mount, naming where its files live. Only shown when
        // the daemon is up; without it there is no live path to point at.
        if let Some(outcome) = outcome {
            for result in results {
                if matches!(result.outcome, MountOutcome::Ready) {
                    let where_to = ready_mount_location(outcome, &result.mount_name);
                    session.row(crate::ui::report::Row::new(
                        crate::ui::style::Glyph::Done,
                        &result.mount_name,
                        where_to,
                    ));
                }
            }
        }
        if outcome.is_none() {
            session.note(ui::hint("omnifs up", "start the daemon"));
        } else {
            session.note(ui::hint("omnifs shell", "browse your files"));
        }
        session.note(ui::hint("omnifs status", "check the daemon"));
        session.note(ui::hint("omnifs mount add", "add another provider"));
        session.note(ui::hint(
            "omnifs completions",
            "tab completion for your shell",
        ));
        let next = if outcome.is_none() {
            "You're set. Run `omnifs up` when ready."
        } else {
            "You're set. Try `omnifs shell`."
        };
        session.outro(next);
    }

    /// Resolve which provider names to configure.
    fn resolve_selection(
        &self,
        installed: &[(Provider, ProviderManifest)],
        configured: &std::collections::BTreeMap<String, String>,
        mode: PromptMode,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<Vec<String>> {
        if !self.providers.is_empty() {
            return validate_preselected(&self.providers, installed, configured, session);
        }
        if mode.yes {
            return Ok(Self::yes_auto_select(installed, configured, session));
        }
        if mode.no_input {
            anyhow::bail!(
                "`--no-input` needs --providers <provider>[,<provider>...], or pass --yes to configure the auto-selectable providers"
            );
        }
        if !mode.interactive {
            anyhow::bail!(
                "provider selection needs a terminal; pass --providers <provider>[,<provider>...] or --yes"
            );
        }
        let rows = crate::ui::picker::build_rows(installed, configured);
        if rows.is_empty() {
            session.note("all providers already configured");
            return Ok(Vec::new());
        }
        crate::ui::picker::multiselect("What should omnifs mount?", rows)
    }

    async fn run_init_loop(
        &self,
        selected: &[String],
        installed: &[(Provider, ProviderManifest)],
        workspace: &Workspace,
        style: StageStyle,
        phase_num: usize,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<Vec<InitResult>> {
        let mut out = Vec::new();
        for provider_name in selected {
            let Some((_, manifest)) = crate::catalog::find_installed(installed, provider_name)
            else {
                anyhow::bail!("provider `{provider_name}` not found");
            };
            let mount_name = manifest.default_mount.clone();
            // A no-auth provider has nothing to sign into; naming its phase
            // "sign in" would misdescribe a mount that just comes up.
            let verb = if manifest.auth.is_some() {
                "sign in"
            } else {
                "mount"
            };
            session.phase(style.phase(phase_num, &format!("{provider_name} {verb}")));

            let init_args = mount::AddArgs {
                provider: Some(provider_name.clone()),
                as_name: None,
                no_input: self.no_input || self.yes,
                yes: self.yes,
                no_browser: self.no_browser,
                token: None,
                token_env: None,
                no_validate: false,
                scopes: Vec::new(),
                scheme: None,
                no_auth: false,
                config_json: None,
                capabilities_json: None,
                limits_json: None,
                json: false,
            };
            match crate::stages::configure_mount(init_args, workspace, false, session).await {
                Ok(outcome) => out.push(InitResult {
                    mount_name: outcome.mount_name,
                    outcome: MountOutcome::from_status(outcome.status),
                }),
                Err(error) => {
                    session.row(crate::ui::report::Row::new(
                        crate::ui::style::Glyph::Fail,
                        mount_name,
                        one_line(&error),
                    ));
                    return Err(error);
                },
            }
        }
        Ok(out)
    }
}

/// Map the setup wizard's OS detection onto the frontend resolver's coarser
/// OS axis. WSL counts as Linux: it hosts a real Linux kernel FUSE stack, the
/// same distinction the resolver cares about.
fn to_resolver_os(os: HostOs) -> crate::config::HostOs {
    match os {
        HostOs::MacOs => crate::config::HostOs::MacOs,
        HostOs::LinuxNative | HostOs::LinuxWsl => crate::config::HostOs::Linux,
        HostOs::Unsupported => crate::config::HostOs::Other,
    }
}

/// The effective `[[frontends]]` plan, for wizard stage gating only (whether
/// to show the Docker reachability row, whether to ask for a host mount
/// point). Falls back to `/` when the platform mount-point default itself is
/// unresolvable (e.g. `HOME` unset): a local entry's driver/kind presence in
/// the plan does not depend on which path it ultimately resolves to, only
/// gating decisions read this plan, and the real mount-point question (when
/// asked) resolves and validates the default itself.
fn resolve_setup_frontend_plan(
    config: &crate::config::Config,
    os: HostOs,
) -> anyhow::Result<Vec<crate::config::EffectiveFrontend>> {
    let default_mount_point =
        omnifs_workspace::layout::resolve_mount_point().unwrap_or_else(|| PathBuf::from("/"));
    crate::config::resolve_frontends(&config.frontends, to_resolver_os(os), &default_mount_point)
}

/// Mount point for the review-mode add-a-provider path, derived without any
/// prompt: the resolved default (`OMNIFS_MOUNT_POINT` or the home-derived
/// path), when the effective `[[frontends]]` plan includes a local frontend.
/// A guest-only plan has no host mount path to derive.
fn add_provider_mount_point(
    config: &crate::config::Config,
    os: HostOs,
) -> anyhow::Result<Option<PathBuf>> {
    let plan = resolve_setup_frontend_plan(config, os)?;
    if !plan.iter().any(|entry| entry.driver == Driver::Local) {
        return Ok(None);
    }
    omnifs_workspace::layout::resolve_mount_point()
        .map(Some)
        .ok_or_else(|| {
            anyhow::anyhow!("cannot resolve host mount point: set HOME or OMNIFS_MOUNT_POINT")
        })
}

fn one_line(error: &anyhow::Error) -> String {
    error.to_string().lines().next().unwrap_or("").to_string()
}

/// Where a Ready mount's files live for the graduation card.
fn ready_mount_location(outcome: &LaunchOutcome, mount: &str) -> String {
    let base = outcome
        .mount_point
        .clone()
        .or_else(omnifs_workspace::layout::resolve_mount_point)
        .unwrap_or_else(|| PathBuf::from("/"));
    WorkspaceLayout::display(&base.join(mount))
}

fn validate_preselected(
    requested: &[String],
    installed: &[(Provider, ProviderManifest)],
    configured: &std::collections::BTreeMap<String, String>,
    session: &mut crate::ui::session::Session,
) -> anyhow::Result<Vec<String>> {
    let known = || {
        installed
            .iter()
            .map(|(provider, _)| provider.meta.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = Vec::new();
    for id in requested {
        if crate::catalog::find_installed(installed, id).is_none() {
            anyhow::bail!("provider `{id}` is not available; known: {}", known());
        }
        if configured.contains_key(id) {
            session.row(crate::ui::report::Row::new(
                crate::ui::style::Glyph::Skip,
                id,
                format!("already configured as {}", configured[id]),
            ));
            continue;
        }
        out.push(id.clone());
    }
    Ok(out)
}

// ── Review mode ────────────────────────────────────────────────────────────

/// Ledger width budget for the joined review-mode mounts value.
const MOUNTS_ROW_WIDTH: usize = 60;

impl SetupArgs {
    /// The review hub: a loop over a status ledger and an action menu for an
    /// already-configured workspace. Each action runs, then control returns to
    /// the hub; the loop exits on the exit item, Esc, or Ctrl-C.
    async fn review_mode(
        &self,
        workspace: &Workspace,
        mode: PromptMode,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<()> {
        loop {
            let summaries = render_review_ledger(workspace, session).await?;

            // Non-interactive review keeps its verbatim bail messages.
            if mode.no_input {
                anyhow::bail!(
                    "`omnifs setup --no-input` is in review mode; pass --providers <provider> to add one, or --yes"
                );
            }
            if !mode.interactive {
                anyhow::bail!(
                    "`omnifs setup` is in review mode and needs a terminal; pass --providers <provider> or --yes"
                );
            }

            let candidates = reauth_candidates(&summaries);
            let choice =
                match crate::ui::picker::select("What next?", review_menu_rows(&candidates)) {
                    Ok(id) => id,
                    // A rail cancellation is a command cancellation. Do not turn
                    // Esc/Ctrl-C into a skipped provider or silently fall through
                    // to a successful setup exit.
                    Err(error) if crate::ui::picker::is_canceled(&error) => return Err(error),
                    Err(error) => return Err(error),
                };

            match choice.as_str() {
                "add a provider" => {
                    // Jump straight to the shared configure tail. Esc at the
                    // provider picker returns to the hub, not out of setup.
                    let mount_point =
                        add_provider_mount_point(&workspace.config()?, HostOs::detect())?;
                    match self
                        .configure_and_launch(
                            workspace,
                            mount_point,
                            mode,
                            StageStyle::Hub,
                            session,
                        )
                        .await
                    {
                        Ok(()) => {},
                        Err(error) if crate::ui::picker::is_canceled(&error) => {},
                        Err(error) => return Err(error),
                    }
                },
                "run checks" => {
                    session.note("running `omnifs doctor`");
                    crate::commands::doctor::DoctorArgs::default().run().await?;
                },
                "exit" => {
                    session.outro("Leaving setup.");
                    return Ok(());
                },
                _ => {
                    self.reauth_from_hub(workspace, &candidates, session).await;
                },
            }
            // The blank at the top of the next iteration separates this action's
            // output from the re-rendered ledger.
        }
    }

    /// Re-authenticate one mount from the hub. When several mounts need
    /// attention, a second picker chooses which. A cancel or a reauth failure
    /// leaves a note and returns to the hub rather than aborting setup.
    async fn reauth_from_hub(
        &self,
        workspace: &Workspace,
        candidates: &[String],
        session: &mut crate::ui::session::Session,
    ) {
        let target = if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            match crate::ui::picker::select("Which mount?", reauth_target_rows(candidates)) {
                Ok(id) => id,
                // Cancel is a silent return to the hub; anything else is worth
                // a breadcrumb before returning.
                Err(error) if crate::ui::picker::is_canceled(&error) => return,
                Err(error) => {
                    session.note(one_line(&error));
                    return;
                },
            }
        };
        let reauth = crate::commands::mount::ReauthArgs {
            name: target,
            no_input: false,
            no_browser: self.no_browser,
            token: None,
            token_env: None,
            no_validate: false,
            scopes: Vec::new(),
        };
        if let Err(error) = reauth.run_in_session(workspace, session).await {
            session.note(one_line(&error));
        }
    }
}

/// Render the review status ledger (mounts, daemon) and return the mount
/// summaries the menu needs. Re-run each hub iteration for fresh facts.
async fn render_review_ledger(
    workspace: &Workspace,
    session: &mut crate::ui::session::Session,
) -> anyhow::Result<Vec<crate::mount_report::UserMountStatus>> {
    let store = omnifs_workspace::creds::FileStore::new(&workspace.layout().credentials_file);
    let mounts = workspace.mounts()?;
    let summaries =
        crate::mount_report::scan_user_mount_configs(workspace.catalog(), &mounts, &store);
    if summaries.is_empty() {
        session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Warn,
            "mounts",
            "none configured",
        ));
    } else {
        let entries: Vec<(String, &'static str)> = summaries.iter().map(mount_summary).collect();
        let joined = entries
            .iter()
            .map(|(name, state)| format!("{name} ({state})"))
            .collect::<Vec<_>>()
            .join(", ");
        // A long roster overflows the ledger row; wrap it into per-mount notes.
        if entries.len() > 3 || joined.chars().count() > MOUNTS_ROW_WIDTH {
            session.row(crate::ui::report::Row::new(
                crate::ui::style::Glyph::Done,
                "mounts",
                format!("{} configured", entries.len()),
            ));
            for (name, state) in &entries {
                session.note(format!("{name}: {state}"));
            }
        } else {
            session.row(crate::ui::report::Row::new(
                crate::ui::style::Glyph::Done,
                "mounts",
                joined,
            ));
        }
    }

    if workspace.daemon().ready().await {
        session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Done,
            "daemon",
            "running",
        ));
    } else {
        session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Warn,
            "daemon",
            "not running",
        ));
        session.note("start it with `omnifs up`");
    }

    Ok(summaries)
}

/// Mounts whose credential is missing or errored, and so can be reauthed.
fn reauth_candidates(summaries: &[crate::mount_report::UserMountStatus]) -> Vec<String> {
    use crate::auth::AuthReadiness;
    use crate::mount_report::UserMountStatus;
    summaries
        .iter()
        .filter_map(|status| match status {
            UserMountStatus::Ready(ready) => match &ready.auth {
                AuthReadiness::Missing { .. } | AuthReadiness::Error { .. } => {
                    Some(ready.mount.clone())
                },
                _ => None,
            },
            UserMountStatus::Invalid { .. } => None,
        })
        .collect()
}

/// A menu row with a detail panel: `id` is both the visible left label and the
/// returned choice.
fn menu_row(id: &str, summary: &str, panel: Vec<crate::ui::picker::PanelLine>) -> PickerRow {
    PickerRow {
        id: id.to_string(),
        summary: summary.to_string(),
        cap_tags: Vec::new(),
        auth_tag: None,
        default_on: false,
        detail: crate::ui::picker::Detail { lines: panel },
    }
}

/// The review hub menu rows, in display order. The reauth row appears only when
/// a mount needs attention.
fn review_menu_rows(candidates: &[String]) -> Vec<PickerRow> {
    use crate::ui::picker::{PanelLine, PanelRole};
    let line = |text: &str, role: PanelRole| PanelLine {
        text: text.to_string(),
        role,
    };

    let mut rows = vec![menu_row(
        "add a provider",
        "configure another provider",
        vec![
            line("add a provider", PanelRole::Head),
            line(
                "pick a provider, grant its access, then sign in or paste a token",
                PanelRole::Plain,
            ),
            line(
                "applies to the running daemon when it is up",
                PanelRole::Dim,
            ),
        ],
    )];

    if !candidates.is_empty() {
        let label = if candidates.len() == 1 {
            format!("reauth {}", candidates[0])
        } else {
            "reauth a mount".to_string()
        };
        rows.push(menu_row(
            &label,
            "renew a mount's credential",
            vec![
                line("reauth", PanelRole::Head),
                line(
                    "re-run sign-in or paste a fresh token for a mount that lost auth",
                    PanelRole::Plain,
                ),
                line(
                    &format!("needs attention: {}", candidates.join(", ")),
                    PanelRole::Dim,
                ),
            ],
        ));
    }

    rows.push(menu_row(
        "run checks",
        "diagnose with omnifs doctor",
        vec![
            line("run checks", PanelRole::Head),
            line(
                "probe docker, fuse, providers, credentials, and live mounts",
                PanelRole::Plain,
            ),
        ],
    ));
    rows.push(menu_row(
        "exit",
        "leave setup",
        vec![line("exit", PanelRole::Head)],
    ));
    rows
}

/// Picker rows for choosing which mount to reauth when several need it.
fn reauth_target_rows(candidates: &[String]) -> Vec<PickerRow> {
    candidates
        .iter()
        .map(|name| menu_row(name, "renew this mount's credential", Vec::new()))
        .collect()
}

/// A review-ledger entry for one mount: `(name, state word)`.
fn mount_summary(status: &crate::mount_report::UserMountStatus) -> (String, &'static str) {
    use crate::auth::AuthReadiness;
    use crate::mount_report::UserMountStatus;
    match status {
        UserMountStatus::Invalid { config_path, .. } => {
            let name = config_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("?")
                .to_string();
            (name, "invalid")
        },
        UserMountStatus::Ready(ready) => {
            let state = match &ready.auth {
                AuthReadiness::None => "no auth needed",
                AuthReadiness::Ready { .. } => "ready",
                AuthReadiness::Missing { .. } => "needs auth",
                AuthReadiness::Error { .. } => "auth error",
            };
            (ready.mount.clone(), state)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The review-mode add-a-provider path must never prompt for a mount
    /// point: it derives the resolved host default without asking, for the
    /// default (locally-mounted) `[[frontends]]` plan.
    #[test]
    fn add_provider_mount_point_is_prompt_free() {
        assert_eq!(
            add_provider_mount_point(&crate::config::Config::default(), HostOs::detect())
                .unwrap()
                .unwrap(),
            omnifs_workspace::layout::resolve_mount_point().unwrap()
        );
    }

    /// A guest-only `[[frontends]]` plan has no host mount path to derive:
    /// the review-mode add-a-provider path must not fabricate one.
    #[test]
    fn add_provider_mount_point_is_none_for_a_guest_only_plan() {
        let config = crate::config::Config {
            frontends: vec![crate::config::ConfigFrontendEntry {
                kind: omnifs_workspace::runtime_record::FrontendKind::Fuse,
                driver: crate::frontend_backend::Driver::Docker,
                mount_point: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            add_provider_mount_point(&config, HostOs::MacOs).unwrap(),
            None
        );
    }

    #[test]
    fn ready_mount_location_uses_host_path() {
        let outcome = LaunchOutcome {
            mount_point: Some(PathBuf::from("/mnt/omnifs")),
        };
        assert_eq!(
            ready_mount_location(&outcome, "github"),
            "/mnt/omnifs/github"
        );
    }

    #[test]
    fn declined_sign_in_is_a_skip() {
        assert!(matches!(
            MountOutcome::from_status(crate::stages::MountInitStatus::SignInDeclined),
            MountOutcome::Skipped
        ));
    }

    #[test]
    fn review_phases_do_not_claim_the_five_stage_wizard() {
        assert_eq!(StageStyle::Hub.phase(4, "github sign in"), "github sign in");
        assert_eq!(
            StageStyle::Wizard.phase(4, "github sign in"),
            "4/5 github sign in"
        );
    }
}
