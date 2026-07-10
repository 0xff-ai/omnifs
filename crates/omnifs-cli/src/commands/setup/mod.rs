//! `omnifs setup`: guided onboarding walkthrough.
//!
//! A single ledger drives the whole wizard: an environment summary, a mount
//! point question, a provider picker, a per-provider block for each
//! selection, and a launch. Every human line prints on stderr through the
//! `crate::ui` design system; stdout is reserved for machine output. The
//! daemon always runs host-native, so there is no runtime-backend stage: on
//! macOS `omnifs up` additionally auto-starts the optional Docker-hosted FUSE
//! frontend, which is why this wizard still surfaces Docker reachability
//! there.

pub mod host_os;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::{Provider, ProviderManifest};

use crate::commands::init;
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
    /// Mount point for the daemon's native mount.
    #[arg(long, value_name = "PATH")]
    pub mount_point: Option<PathBuf>,
    /// Preselect providers and skip the picker.
    #[arg(long, value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
}

/// How the shared configure tail titles its sections: the fresh wizard counts
/// its stages (`── 3/5 first mount ──`), while the review hub is not a
/// five-stage walk, so its actions print plain rules without counters.
#[derive(Clone, Copy)]
enum StageStyle {
    Wizard,
    Hub,
}

impl StageStyle {
    fn banner(self, n: usize, title: &str) -> String {
        match self {
            Self::Wizard => ui::stage_rule(n, 5, title),
            Self::Hub => ui::rule(title),
        }
    }
}

/// The outcome of configuring one provider during setup.
enum MountOutcome {
    Ready,
    Skipped,
    Failed(String),
}

struct InitResult {
    mount_name: String,
    outcome: MountOutcome,
}

impl SetupArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        let mode = PromptMode {
            interactive: terminal && !self.no_input,
            yes: self.yes,
            no_input: self.no_input,
        };

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
            return self.review_mode(&workspace, mode).await;
        }

        // Fresh mode: orientation + environment ledger.
        print_header();
        let installed = crate::catalog::installed_providers(workspace.catalog())?;
        if installed.is_empty() {
            anyhow::bail!("no built-in or plugin providers are available");
        }
        anstream::eprintln!("{}", ui::stage_rule(1, 5, "environment"));
        anstream::eprintln!("{}", ui::ok("environment", os.name()));
        anstream::eprintln!(
            "{}",
            ui::ok("providers", format!("{} installed", installed.len()))
        );
        // Docker reachability matters only on macOS, where `omnifs up`
        // auto-starts the Docker-hosted FUSE frontend; Linux's native FUSE
        // mount needs no container.
        if os == HostOs::MacOs {
            Self::render_docker_row(&config).await;
        }

        anstream::eprintln!();
        anstream::eprintln!("{}", ui::stage_rule(2, 5, "mount point"));
        let mount_point = self.resolve_mount_point(mode)?;

        self.configure_and_launch(&workspace, mount_point, mode, StageStyle::Wizard)
            .await
    }

    /// The informational Docker reachability row for the environment stage. It
    /// never fails setup; an unreachable daemon just notes the retry hint.
    async fn render_docker_row(config: &crate::config::Config) {
        let mut live = ui::LiveRow::start("docker", "checking");
        live.update("connecting");
        match crate::stages::probe_docker_reachability(config).await {
            crate::stages::DockerReachability::Running { version } => {
                live.settle_ok(format!("{version} running"));
            },
            crate::stages::DockerReachability::Unreachable => {
                live.settle_warn("not reachable");
                anstream::eprintln!(
                    "{}",
                    ui::note(
                        "start Docker Desktop so `omnifs up` can start the FUSE frontend; native NFS keeps working without it"
                    )
                );
            },
        }
    }

    /// Resolve the mount point and print the post-answer note. The daemon
    /// always runs host-native, so this question always applies.
    fn resolve_mount_point(&self, mode: PromptMode) -> anyhow::Result<PathBuf> {
        let mount_point = crate::stages::mount_point_resolution(self.mount_point.clone(), mode)?;
        let display = WorkspaceLayout::display(&mount_point);
        // State the fact when the prompt was skipped (--mount-point or --yes).
        if self.mount_point.is_some() || mode.yes {
            anstream::eprintln!("{}", ui::ok("mount point", &display));
        }
        anstream::eprintln!("{}", ui::note(format!("files appear at {display}")));
        // Launch reads OMNIFS_MOUNT_POINT; a typed/flagged value only previews
        // unless it is exported.
        let already = crate::config::env_string("OMNIFS_MOUNT_POINT")
            .is_some_and(|env| WorkspaceLayout::display(&PathBuf::from(env)) == display);
        if !already {
            anstream::eprintln!(
                "{}",
                ui::note(format!(
                    "`export OMNIFS_MOUNT_POINT={display}` to persist it"
                ))
            );
        }
        Ok(mount_point)
    }

    /// Shared tail: pick providers, configure each, launch, and close.
    async fn configure_and_launch(
        &self,
        workspace: &Workspace,
        mount_point: PathBuf,
        mode: PromptMode,
        style: StageStyle,
    ) -> anyhow::Result<()> {
        let installed = crate::catalog::installed_providers(workspace.catalog())?;
        let mounts = workspace.mounts()?;
        let configured = crate::catalog::configured_mounts(workspace.catalog(), &mounts)?;

        anstream::eprintln!();
        anstream::eprintln!("{}", style.banner(3, "first mount"));
        let selected = self.resolve_selection(&installed, &configured, mode)?;

        // Nothing new to configure (all providers already configured, or the
        // picker was confirmed empty): from the hub, return to it without the
        // launch narration. The fresh wizard falls through to its own
        // "no mounts yet" handling below.
        if selected.is_empty() && matches!(style, StageStyle::Hub) {
            return Ok(());
        }

        anstream::eprintln!();
        anstream::eprintln!("{}", style.banner(4, "auth + grants"));
        let results = self.run_init_loop(&selected, &installed, workspace).await;

        let any_ready = results
            .iter()
            .any(|r| matches!(r.outcome, MountOutcome::Ready))
            || !configured.is_empty();

        if self.no_up {
            anstream::eprintln!(
                "{}",
                ui::note("daemon launch skipped (--no-up); run `omnifs up` when ready")
            );
            Self::print_closer(&results, None);
            return Ok(());
        }
        if !any_ready {
            anstream::eprintln!(
                "{}",
                ui::note("no mounts yet; add one with `omnifs init <provider>`")
            );
            return Ok(());
        }

        let outcome = self
            .launch_and_report(workspace, &mount_point, &results, style)
            .await?;
        Self::print_closer(&results, Some(&outcome));
        Ok(())
    }

    async fn launch_and_report(
        &self,
        workspace: &Workspace,
        mount_point: &std::path::Path,
        results: &[InitResult],
        style: StageStyle,
    ) -> anyhow::Result<LaunchOutcome> {
        anstream::eprintln!();
        anstream::eprintln!("{}", style.banner(5, "launch"));
        // `Launcher::launch` writes its own stderr progress lines; a spinner
        // here would be overwritten mid-line by them. Print a plain note before
        // and settle into a static row after, as the pre-LiveRow design did.
        anstream::eprintln!("{}", ui::note("starting the daemon"));
        let outcome = match Launcher::new(workspace, "omnifs setup").launch().await {
            Ok(outcome) => outcome,
            Err(error) => {
                anstream::eprintln!("{}", ui::fail("daemon", one_line(&error)));
                return Err(error);
            },
        };

        let mp = outcome
            .mount_point
            .clone()
            .unwrap_or_else(|| mount_point.to_path_buf());
        let daemon = format!("running natively at {}", WorkspaceLayout::display(&mp));
        anstream::eprintln!("{}", ui::ok("daemon", daemon));

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
                        anstream::eprintln!("{}", ui::note(line));
                    }
                    anstream::eprintln!(
                        "{}",
                        ui::ok(
                            "first read",
                            format!("{} ({entries} entries)", read.command)
                        )
                    );
                },
                Err(error) => {
                    anstream::eprintln!(
                        "{}",
                        ui::warn_row("first read", "failed; run omnifs doctor")
                    );
                    anstream::eprintln!("{}", ui::note(one_line(&error)));
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
                !crate::commands::init::detect::detect(manifest.wasm_auth_manifest().as_ref())
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
            anstream::eprintln!(
                "{}",
                ui::note(format!("auto-selected {}", selected.join(", ")))
            );
        }
        for entry in &skipped {
            anstream::eprintln!("{}", ui::note(format!("skipped {entry}")));
        }
        selected
    }

    /// The `You're set.` graduation card: any failures/skips first, then one
    /// row per Ready mount naming where its files live, then the daily-command
    /// hints. `outcome` is `None` when the daemon was not launched (`--no-up`),
    /// which also swaps the first hint to `omnifs up`.
    fn print_closer(results: &[InitResult], outcome: Option<&LaunchOutcome>) {
        // Surface any failures/skips before the closer.
        let mut had_failure = false;
        for result in results {
            match &result.outcome {
                MountOutcome::Failed(reason) => {
                    had_failure = true;
                    anstream::eprintln!("{}", ui::fail(&result.mount_name, reason));
                },
                MountOutcome::Skipped => {
                    anstream::eprintln!("{}", ui::skip(&result.mount_name, "skipped"));
                },
                MountOutcome::Ready => {},
            }
        }
        if had_failure {
            anstream::eprintln!("{}", ui::note("retry with `omnifs init <provider>`"));
        }

        anstream::eprintln!();
        anstream::eprintln!("{}", ui::heading("You're set."));
        // One row per Ready mount, naming where its files live. Only shown when
        // the daemon is up; without it there is no live path to point at.
        if let Some(outcome) = outcome {
            for result in results {
                if matches!(result.outcome, MountOutcome::Ready) {
                    let where_to = ready_mount_location(outcome, &result.mount_name);
                    anstream::eprintln!("{}", ui::ok(&result.mount_name, where_to));
                }
            }
        }
        if outcome.is_none() {
            anstream::eprintln!("{}", ui::hint("omnifs up", "start the daemon"));
        } else {
            anstream::eprintln!("{}", ui::hint("omnifs shell", "browse your files"));
        }
        anstream::eprintln!("{}", ui::hint("omnifs status", "check the daemon"));
        anstream::eprintln!("{}", ui::hint("omnifs init", "add another provider"));
        anstream::eprintln!(
            "{}",
            ui::hint("omnifs completions", "tab completion for your shell")
        );
    }

    /// Resolve which provider names to configure.
    fn resolve_selection(
        &self,
        installed: &[(Provider, ProviderManifest)],
        configured: &std::collections::BTreeMap<String, String>,
        mode: PromptMode,
    ) -> anyhow::Result<Vec<String>> {
        if !self.providers.is_empty() {
            return validate_preselected(&self.providers, installed, configured);
        }
        if mode.yes {
            return Ok(Self::yes_auto_select(installed, configured));
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
            anstream::eprintln!("{}", ui::note("all providers already configured"));
            return Ok(Vec::new());
        }
        crate::ui::picker::multiselect("What should omnifs mount?", rows)
    }

    async fn run_init_loop(
        &self,
        selected: &[String],
        installed: &[(Provider, ProviderManifest)],
        workspace: &Workspace,
    ) -> Vec<InitResult> {
        let mut out = Vec::new();
        for provider_name in selected {
            let Some((_, manifest)) = crate::catalog::find_installed(installed, provider_name)
            else {
                out.push(InitResult {
                    mount_name: provider_name.clone(),
                    outcome: MountOutcome::Failed(format!("provider `{provider_name}` not found")),
                });
                continue;
            };
            let mount_name = manifest.default_mount.clone();

            anstream::eprintln!();
            anstream::eprintln!("{}", ui::rule(provider_name));
            init::render_consent_block(manifest);

            let init_args = init::InitArgs {
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
            };
            match crate::stages::configure_mount(init_args, workspace, false).await {
                Ok(outcome) => out.push(InitResult {
                    mount_name: outcome.mount_name,
                    outcome: MountOutcome::Ready,
                }),
                Err(error) => {
                    // A cancel (Esc/Ctrl-C at any prompt) or an auth-required
                    // bail is a skip, not a provider failure.
                    let skipped = crate::ui::picker::is_canceled(&error)
                        || crate::error::exit_code(&error) == crate::error::ExitCode::AuthRequired;
                    out.push(InitResult {
                        mount_name,
                        outcome: if skipped {
                            MountOutcome::Skipped
                        } else {
                            MountOutcome::Failed(one_line(&error))
                        },
                    });
                },
            }
        }
        out
    }
}

fn print_header() {
    anstream::eprintln!();
    anstream::eprintln!("{}", ui::heading("omnifs setup"));
    anstream::eprintln!();
    anstream::eprintln!("  omnifs mounts your services as regular files.");
    anstream::eprintln!("  One daemon, one mount point, your standard tools.");
    anstream::eprintln!();
}

/// Mount point for the review-mode add-a-provider path, derived without any
/// prompt: the resolved default (`OMNIFS_MOUNT_POINT` or the home-derived
/// path), since the daemon always runs host-native.
fn add_provider_mount_point() -> anyhow::Result<PathBuf> {
    omnifs_workspace::layout::resolve_mount_point().ok_or_else(|| {
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
            anstream::eprintln!(
                "{}",
                ui::skip(id, format!("already configured as {}", configured[id]))
            );
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
    async fn review_mode(&self, workspace: &Workspace, mode: PromptMode) -> anyhow::Result<()> {
        anstream::eprintln!();
        anstream::eprintln!("{}", ui::heading("omnifs setup"));

        loop {
            anstream::eprintln!();
            let summaries = render_review_ledger(workspace).await?;

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
                    Err(error) if crate::ui::picker::is_canceled(&error) => return Ok(()),
                    Err(error) => return Err(error),
                };

            match choice.as_str() {
                "add a provider" => {
                    // Jump straight to the shared configure tail. Esc at the
                    // provider picker returns to the hub, not out of setup.
                    let mount_point = add_provider_mount_point()?;
                    match self
                        .configure_and_launch(workspace, mount_point, mode, StageStyle::Hub)
                        .await
                    {
                        Ok(()) => {},
                        Err(error) if crate::ui::picker::is_canceled(&error) => {},
                        Err(error) => return Err(error),
                    }
                },
                "run checks" => {
                    anstream::eprintln!("{}", ui::note("running `omnifs doctor`"));
                    crate::commands::doctor::DoctorArgs::default().run().await?;
                },
                "exit" => return Ok(()),
                _ => self.reauth_from_hub(workspace, &candidates).await,
            }
            // The blank at the top of the next iteration separates this action's
            // output from the re-rendered ledger.
        }
    }

    /// Re-authenticate one mount from the hub. When several mounts need
    /// attention, a second picker chooses which. A cancel or a reauth failure
    /// leaves a note and returns to the hub rather than aborting setup.
    async fn reauth_from_hub(&self, workspace: &Workspace, candidates: &[String]) {
        let target = if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            match crate::ui::picker::select("Which mount?", reauth_target_rows(candidates)) {
                Ok(id) => id,
                // Cancel is a silent return to the hub; anything else is worth
                // a breadcrumb before returning.
                Err(error) if crate::ui::picker::is_canceled(&error) => return,
                Err(error) => {
                    anstream::eprintln!("{}", ui::note(one_line(&error)));
                    return;
                },
            }
        };
        let reauth = crate::commands::mounts::ReauthArgs {
            name: target,
            no_input: false,
            no_browser: self.no_browser,
            token: None,
            token_env: None,
            no_validate: false,
            scopes: Vec::new(),
        };
        if let Err(error) = reauth.run_in_workspace(workspace).await {
            anstream::eprintln!("{}", ui::note(one_line(&error)));
        }
    }
}

/// Render the review status ledger (mounts, daemon) and return the mount
/// summaries the menu needs. Re-run each hub iteration for fresh facts.
async fn render_review_ledger(
    workspace: &Workspace,
) -> anyhow::Result<Vec<crate::mount_report::UserMountStatus>> {
    let store = omnifs_workspace::creds::FileStore::new(&workspace.layout().credentials_file);
    let mounts = workspace.mounts()?;
    let summaries =
        crate::mount_report::scan_user_mount_configs(workspace.catalog(), &mounts, &store);
    if summaries.is_empty() {
        anstream::eprintln!("{}", ui::warn_row("mounts", "none configured"));
    } else {
        let entries: Vec<(String, &'static str)> = summaries.iter().map(mount_summary).collect();
        let joined = entries
            .iter()
            .map(|(name, state)| format!("{name} ({state})"))
            .collect::<Vec<_>>()
            .join(", ");
        // A long roster overflows the ledger row; wrap it into per-mount notes.
        if entries.len() > 3 || joined.chars().count() > MOUNTS_ROW_WIDTH {
            anstream::eprintln!(
                "{}",
                ui::ok("mounts", format!("{} configured", entries.len()))
            );
            for (name, state) in &entries {
                anstream::eprintln!("{}", ui::note(format!("{name}: {state}")));
            }
        } else {
            anstream::eprintln!("{}", ui::ok("mounts", joined));
        }
    }

    if workspace.daemon().ready().await {
        anstream::eprintln!("{}", ui::ok("daemon", "running"));
    } else {
        anstream::eprintln!("{}", ui::warn_row("daemon", "not running"));
        anstream::eprintln!("{}", ui::note("start it with `omnifs up`"));
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
    /// point: it derives the resolved host default without asking.
    #[test]
    fn add_provider_mount_point_is_prompt_free() {
        assert_eq!(
            add_provider_mount_point().unwrap(),
            omnifs_workspace::layout::resolve_mount_point().unwrap()
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
}
