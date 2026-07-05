//! `omnifs setup`: guided onboarding walkthrough.
//!
//! Sequential, npx-style: each step prints inline and stays in scrollback.
//! Detects host OS, explains the runtime model, prepares the selected runtime,
//! walks the user through selecting providers, confirms capabilities per
//! provider, runs `init`, and (unless `--no-up`) launches the daemon.

pub mod host_os;
pub mod summary;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use clap::Args;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::{Provider, ProviderManifest};

use crate::commands::init;
use crate::config::ConfiguredBackend;
use crate::launch_backend::GUEST_MOUNT;
use crate::stages::PromptMode;
use crate::workspace::Workspace;

use self::host_os::HostOs;
use self::summary::InitResult;

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
    /// Runtime to persist as the default.
    #[arg(long, value_enum)]
    pub runtime: Option<ConfiguredBackend>,
    /// Mount point to preview for host-native runs. To persist it for launch,
    /// export `OMNIFS_MOUNT_POINT` with the same value.
    #[arg(long, value_name = "PATH")]
    pub mount_point: Option<PathBuf>,
    /// Preselect providers and skip the picker.
    #[arg(long, value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
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
        print_orientation(os);

        let workspace = Workspace::resolve()?;
        let paths = workspace.layout();
        let config = workspace.config()?;
        let environment = crate::stages::environment_check(os, &workspace)?;
        print_environment(&environment);
        crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;
        fs::create_dir_all(&paths.mounts_dir)
            .with_context(|| format!("create {}", paths.mounts_dir.display()))?;

        let mounts = workspace.mounts()?;
        if environment.configured && self.providers.is_empty() && !self.yes {
            match review_mode(&workspace, &mounts, mode)? {
                ReviewAction::Exit => return Ok(()),
                ReviewAction::AddProvider => {},
            }
        }

        let backend =
            crate::stages::runtime_selection(os, config.system.runtime, self.runtime, mode)?;
        crate::stages::persist_runtime(paths, backend)?;
        let host_native = backend == ConfiguredBackend::Native;
        let mount_point = crate::stages::mount_point_resolution(self.mount_point.clone(), mode)?;

        print_runtime(backend, os);
        print_mount_point(&mount_point, host_native);

        let catalog = workspace.catalog();
        let installed = crate::catalog::installed_providers(catalog)?;
        if installed.is_empty() {
            anyhow::bail!("no built-in or plugin providers are available");
        }
        let configured = crate::catalog::configured_mounts(catalog, &mounts)?;

        let selected = resolve_selection(&self, &installed, &configured, mode)?;
        let results = run_init_loop(&selected, &self, &installed, &workspace).await;

        let (mount_label, mount_root, browse_hint) = if host_native {
            let mount_root = omnifs_workspace::layout::WorkspaceLayout::display(&mount_point);
            (
                "Host mount",
                mount_root.clone(),
                format!("`ls {mount_root}`"),
            )
        } else {
            (
                "Container FUSE mount",
                GUEST_MOUNT.to_string(),
                format!("`omnifs shell` then `ls {GUEST_MOUNT}`"),
            )
        };
        let report = summary::SetupSummary::new(
            paths,
            mount_label,
            &mount_root,
            &browse_hint,
            &configured,
            &results,
        );
        anstream::print!("{report}");

        let any_ready = report.any_ready();
        if self.no_up {
            anstream::println!("\nSkipping daemon launch (--no-up).");
        } else if !any_ready {
            anstream::println!(
                "\nNo mounts to launch. Add one with `omnifs init <provider>`, then run `omnifs up`."
            );
        } else {
            let outcome = launch_via_up().await?;
            if let Some(mount) = results
                .iter()
                .find(|result| result.outcome.is_ok())
                .map(|result| result.mount_name.as_str())
            {
                match crate::stages::verify_first_read(&outcome, mount) {
                    Ok(read) => print_first_read(&read),
                    Err(error) => anstream::eprintln!(
                        "First read check failed; run `omnifs doctor` for details: {error:#}"
                    ),
                }
            }
        }
        print_graduation(&results);
        Ok(())
    }
}

fn print_orientation(os: HostOs) {
    anstream::println!();
    anstream::println!("{} ({})", crate::style::bold("omnifs setup"), os.name());
    anstream::println!();
    anstream::println!("omnifs projects services into your filesystem as regular files.");
    anstream::println!("A local daemon serves them at one mount point.");
    anstream::println!("The CLI manages setup, auth, launch, and troubleshooting.");
    anstream::println!();
    anstream::println!("{}", os.explain_runtime());
    anstream::println!();
}

fn print_environment(report: &crate::stages::EnvironmentReport) {
    anstream::println!("1/6  environment    {} ✓", report.os.name());
    if report.configured {
        anstream::println!("                   existing workspace found");
    }
}

fn print_runtime(backend: ConfiguredBackend, os: HostOs) {
    let consequence = match (backend, os) {
        (ConfiguredBackend::Docker, HostOs::MacOs) => {
            "Docker (recommended) - Linux FUSE in a container"
        },
        (ConfiguredBackend::Docker, _) => "Docker - Linux FUSE in a container",
        (ConfiguredBackend::Native, HostOs::MacOs) => "native NFS (experimental)",
        (ConfiguredBackend::Native, _) => "native FUSE",
    };
    anstream::println!("2/6  runtime        {consequence}");
}

fn print_mount_point(path: &std::path::Path, host_native: bool) {
    let display = WorkspaceLayout::display(path);
    if host_native {
        anstream::println!("3/6  mount point    {display}");
    } else {
        anstream::println!("3/6  mount point    {GUEST_MOUNT} inside the runtime container");
    }
}

fn print_first_read(read: &crate::stages::FirstRead) {
    anstream::println!();
    anstream::println!("6/6  launch         ran `{}`", read.command);
    anstream::print!("{}", read.output);
}

fn print_graduation(results: &[InitResult]) {
    let ready: Vec<&InitResult> = results
        .iter()
        .filter(|result| result.outcome.is_ok())
        .collect();
    if ready.is_empty() {
        return;
    }
    anstream::println!();
    anstream::println!("{}", crate::style::bold("You're set. Daily commands:"));
    anstream::println!("  omnifs");
    anstream::println!("  omnifs doctor");
    anstream::println!("  omnifs init <provider>");
    anstream::println!();
    anstream::println!("Shell completions are available with `omnifs completions <shell>`.");
}

enum ReviewAction {
    AddProvider,
    Exit,
}

#[derive(Debug, Clone, Copy)]
enum ReviewChoice {
    AddProvider,
    ChangeRuntime,
    RecheckEnvironment,
    Exit,
}

impl fmt::Display for ReviewChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AddProvider => "add provider",
            Self::ChangeRuntime => "change runtime",
            Self::RecheckEnvironment => "re-check environment",
            Self::Exit => "leave unchanged",
        })
    }
}

fn review_mode(
    workspace: &Workspace,
    mounts: &[crate::mount_config::MountConfig],
    mode: PromptMode,
) -> anyhow::Result<ReviewAction> {
    anstream::println!();
    anstream::println!("{}", crate::style::bold("Setup review"));
    if let Ok(config) = workspace.config() {
        anstream::println!(
            "  runtime  {}",
            config
                .system
                .runtime
                .map_or("unset".to_string(), |runtime| format!("{runtime:?}")
                    .to_lowercase())
        );
    }
    if mounts.is_empty() {
        anstream::println!("  mounts   none");
    } else {
        for mount in mounts {
            anstream::println!(
                "  mount    {} ({})",
                mount.name,
                mount.config.provider_name()
            );
        }
    }
    if mode.no_input {
        anyhow::bail!(
            "`omnifs setup --no-input` is in review mode; pass --providers <provider> to add one, --runtime <docker|native> to change runtime, or --yes to accept defaults"
        );
    }
    if !mode.interactive {
        anyhow::bail!(
            "`omnifs setup` is in review mode and needs a terminal; pass --providers <provider>, --runtime <docker|native>, or --yes"
        );
    }
    let choice = inquire::Select::new(
        "What do you want to change?",
        vec![
            ReviewChoice::AddProvider,
            ReviewChoice::ChangeRuntime,
            ReviewChoice::RecheckEnvironment,
            ReviewChoice::Exit,
        ],
    )
    .prompt()
    .map_err(|error| anyhow!("review prompt: {error}"))?;
    match choice {
        ReviewChoice::AddProvider => Ok(ReviewAction::AddProvider),
        ReviewChoice::ChangeRuntime => {
            let os = HostOs::detect();
            let current = workspace.config()?.system.runtime;
            let runtime = crate::stages::runtime_selection(os, current, None, mode)?;
            crate::stages::persist_runtime(workspace.layout(), runtime)?;
            anstream::println!("Runtime updated.");
            Ok(ReviewAction::Exit)
        },
        ReviewChoice::RecheckEnvironment => {
            let report = crate::stages::environment_check(HostOs::detect(), workspace)?;
            print_environment(&report);
            Ok(ReviewAction::Exit)
        },
        ReviewChoice::Exit => Ok(ReviewAction::Exit),
    }
}

/// Resolve which provider IDs to configure: explicit `--providers` wins,
/// otherwise an interactive `inquire::MultiSelect` over unconfigured providers.
fn resolve_selection(
    args: &SetupArgs,
    installed: &[(Provider, ProviderManifest)],
    configured: &BTreeMap<String, String>,
    mode: PromptMode,
) -> anyhow::Result<Vec<String>> {
    anstream::println!("4/6  first mount");
    if !args.providers.is_empty() {
        return validate_preselected(&args.providers, installed, configured);
    }
    if mode.yes {
        return Ok(Vec::new());
    }
    if mode.no_input {
        anyhow::bail!(
            "`omnifs setup --no-input` needs --providers <provider>[,<provider>...] for the first mount, or use --yes to configure only setup defaults"
        );
    }
    if !mode.interactive {
        anyhow::bail!(
            "`omnifs setup` needs an interactive terminal for provider selection; pass --providers <provider>[,<provider>...] or --yes"
        );
    }

    if !configured.is_empty() {
        anstream::println!("{}", crate::style::bold("Already configured"));
        for (id, mount) in configured {
            anstream::println!("  {id}  (mount: {mount})");
        }
        anstream::println!();
    }

    let mut selectable: Vec<&ProviderManifest> = installed
        .iter()
        .map(|(_, manifest)| manifest)
        .filter(|manifest| !configured.contains_key(&manifest.id))
        .collect();
    if selectable.is_empty() {
        anstream::println!("All providers already configured. Nothing to add.");
        return Ok(Vec::new());
    }
    // Demote providers that require user-supplied state (PAT, fixture file) to
    // the bottom of the picker and uncheck them by default so the smoke path
    // for a fresh setup only enables providers that work with ambient or
    // browser-based auth.
    selectable.sort_by_key(|manifest| (default_off(&manifest.id), manifest.id.clone()));

    let options: Vec<ProviderOption> = selectable
        .iter()
        .map(|manifest| ProviderOption {
            id: manifest.id.clone(),
            line: option_line(manifest),
        })
        .collect();
    let default_indices: Vec<usize> = options
        .iter()
        .enumerate()
        .filter_map(|(idx, opt)| (!default_off(&opt.id)).then_some(idx))
        .collect();

    let chosen = inquire::MultiSelect::new("Which providers do you want to configure?", options)
        .with_default(&default_indices)
        .with_formatter(&format_selected_providers)
        .with_help_message("space to toggle, a all, n none, enter confirm, esc cancel")
        .prompt()
        .map_err(|e| anyhow!("selection prompt: {e}"))?;

    Ok(chosen.into_iter().map(|opt| opt.id).collect())
}

/// Providers that don't work end-to-end without an explicit user step (PAT,
/// fixture file). Listed at the bottom of the setup picker and unchecked by
/// default so a fresh `omnifs setup` keeps moving without hitting prompts the
/// user can't satisfy from ambient context.
fn default_off(provider_name: &str) -> bool {
    matches!(provider_name, "db" | "linear")
}

/// One-row summary shown inside the multi-select.
fn option_line(manifest: &ProviderManifest) -> String {
    let summary = capability_summary(manifest).unwrap_or_else(|| "no extra capabilities".into());
    let auth = if manifest.auth.is_none() {
        "no credentials needed"
    } else {
        "auth required"
    };
    format!(
        "{:<14} {:<23} {} ({summary})",
        manifest.id, auth, manifest.display_name
    )
}

/// A compact one-line capability summary for the multi-select row.
fn capability_summary(manifest: &ProviderManifest) -> Option<String> {
    let limits = crate::capability::limit_lines(&manifest.limits);
    if manifest.capabilities.is_empty() && limits.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = manifest
        .capabilities
        .iter()
        .take(3)
        .map(|entry| {
            format!(
                "{}: {}",
                crate::capability::capability_label(entry).to_lowercase(),
                crate::capability::capability_value(entry)
            )
        })
        .collect();
    let limit_slots = 3usize.saturating_sub(parts.len());
    parts.extend(
        limits
            .into_iter()
            .take(limit_slots)
            .map(|line| format!("limit: {} {}", line.label.to_lowercase(), line.value)),
    );
    if manifest.capabilities.len() + crate::capability::limit_lines(&manifest.limits).len() > 3 {
        parts.push("…".into());
    }
    Some(parts.join("; "))
}

/// Provider option wrapper so `inquire::MultiSelect` can show a rich
/// row while we still recover the provider id post-selection.
#[derive(Debug, Clone)]
struct ProviderOption {
    id: String,
    line: String,
}

impl fmt::Display for ProviderOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.line)
    }
}

fn format_selected_providers(
    options: &[inquire::list_option::ListOption<&ProviderOption>],
) -> String {
    options
        .iter()
        .map(|option| option.value.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_preselected(
    requested: &[String],
    installed: &[(Provider, ProviderManifest)],
    configured: &BTreeMap<String, String>,
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
            anstream::println!(
                "Skipping `{id}` (already configured as `{}`)",
                configured[id]
            );
            continue;
        }
        out.push(id.clone());
    }
    Ok(out)
}

async fn run_init_loop(
    selected: &[String],
    args: &SetupArgs,
    installed: &[(Provider, ProviderManifest)],
    workspace: &Workspace,
) -> Vec<InitResult> {
    let mut out = Vec::new();
    if !selected.is_empty() {
        anstream::println!("5/6  auth + grants");
    }
    for provider_name in selected {
        let Some((_, manifest)) = crate::catalog::find_installed(installed, provider_name) else {
            out.push(InitResult {
                provider_name: provider_name.clone(),
                mount_name: provider_name.clone(),
                outcome: Err(format!("provider `{provider_name}` not found")),
            });
            continue;
        };
        let mount_name = manifest.default_mount.clone();

        anstream::println!();
        anstream::println!("{}", crate::style::bold(format!("--- {provider_name} ---")));

        if !args.yes && !args.no_input {
            let proceed = inquire::Confirm::new(&format!("Configure `{provider_name}`?"))
                .with_default(true)
                .prompt();
            match proceed {
                Ok(true) => {},
                Ok(false) => {
                    out.push(InitResult {
                        provider_name: provider_name.clone(),
                        mount_name,
                        outcome: Err("skipped by user".into()),
                    });
                    continue;
                },
                Err(error) => {
                    out.push(InitResult {
                        provider_name: provider_name.clone(),
                        mount_name,
                        outcome: Err(format!("confirm prompt: {error}")),
                    });
                    continue;
                },
            }
        } else if args.no_input && !args.yes {
            out.push(InitResult {
                provider_name: provider_name.clone(),
                mount_name,
                outcome: Err("missing --yes for provider confirmation".into()),
            });
            continue;
        }

        let init_args = init::InitArgs {
            provider: Some(provider_name.clone()),
            as_name: None,
            no_input: args.no_input || args.yes,
            yes: args.yes,
            no_browser: args.no_browser,
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
        let outcome = crate::stages::configure_mount(init_args, workspace).await;
        let (mount_name, outcome) = match outcome {
            Ok(outcome) => (outcome.mount_name, Ok(())),
            Err(error) => (mount_name, Err(error.to_string())),
        };
        out.push(InitResult {
            provider_name: provider_name.clone(),
            mount_name,
            outcome,
        });
    }
    out
}

async fn launch_via_up() -> anyhow::Result<crate::launch::LaunchOutcome> {
    anstream::println!();
    anstream::println!("Launching omnifs ...");
    let workspace = Workspace::resolve()?;
    crate::stages::launch(&workspace, None, "omnifs setup").await
}
