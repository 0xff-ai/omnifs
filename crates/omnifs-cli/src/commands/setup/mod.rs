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

use anyhow::{Context, anyhow};
use clap::Args;
use omnifs_provider::ProviderManifest;

use crate::catalog::{ProviderTemplate, ProviderTemplates};
use crate::commands::{init, up};
use crate::config::ConfiguredBackend;
use crate::error::WithHint;
use crate::launch_backend::DockerTarget;
use crate::runtime::Runtime;
use crate::session::GUEST_FUSE_MOUNT;
use crate::workspace::Workspace;

use self::host_os::HostOs;
use self::summary::InitResult;

#[derive(Args, Debug, Clone, Default)]
pub struct SetupArgs {
    /// Skip the final daemon launch.
    #[arg(long)]
    pub no_up: bool,
    /// Skip confirmations; auto-accept detected ambient credentials.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Preselect providers and skip the picker.
    #[arg(long, value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
}

impl SetupArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        require_tty()?;

        let os = host_os::detect();
        print_banner(os);
        print_explainer(os);
        if os == HostOs::Unsupported {
            anyhow::bail!("omnifs does not yet run on this platform");
        }

        let workspace = Workspace::resolve()?;
        let paths = workspace.layout();
        let config = workspace.config()?;
        fs::create_dir_all(&paths.mounts_dir)
            .with_context(|| format!("create {}", paths.mounts_dir.display()))?;

        // Record the launch backend so `omnifs up`/`down` read it. Docker is
        // optional; native mode does not require a Docker daemon or image pull.
        let backend = config.backend();
        if config.system.runtime.is_none() {
            let mut file = crate::config::ConfigFile::load(&paths.config_file)?;
            file.set_system_backend(backend)?;
            file.save()?;
        }
        let host_native = backend == ConfiguredBackend::Native;

        if !host_native {
            let docker_target = DockerTarget::resolve(None, None, &config)?;
            let runtime = connect_runtime(os, &docker_target).await?;
            runtime
                .pull_image_with_progress(docker_target.image().as_str())
                .await?;
        }

        let catalog = workspace.catalog();
        let mounts = workspace.mounts()?;
        let templates = catalog.provider_templates()?;
        if templates.is_empty() {
            anyhow::bail!("no built-in or plugin providers are available");
        }
        let configured = templates.configured_mounts(catalog, &mounts);

        let selected = resolve_selection(&self, &templates, &configured)?;
        let results = run_init_loop(&selected, &self, &templates, &workspace).await;

        let (mount_label, mount_root, browse_hint) = if host_native {
            // The daemon resolves its own mount point; we preview the expected
            // default here for the setup summary (HOME/omnifs, same logic as
            // the daemon's `resolve_mount_point` default).
            let home = std::env::var_os("HOME")
                .ok_or_else(|| anyhow::anyhow!("cannot resolve host mount point: set HOME"))?;
            let mount_point = std::path::PathBuf::from(home).join("omnifs");
            let mount_root = omnifs_home::WorkspaceLayout::display(&mount_point);
            (
                "Host mount",
                mount_root.clone(),
                format!("`ls {mount_root}`"),
            )
        } else {
            (
                "Container FUSE mount",
                GUEST_FUSE_MOUNT.to_string(),
                format!("`omnifs shell` then `ls {GUEST_FUSE_MOUNT}`"),
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
                "\nNo mounts to launch. Add one with `omnifs mounts add <provider>`, then run `omnifs up`."
            );
        } else {
            launch_via_up().await?;
        }
        Ok(())
    }
}

fn require_tty() -> anyhow::Result<()> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return Ok(());
    }
    Err(anyhow!(
        "`omnifs setup` requires an interactive terminal. For non-interactive use, run `omnifs mounts add <provider>` per provider and then `omnifs up`."
    ))
}

fn print_banner(os: HostOs) {
    anstream::println!();
    anstream::println!(
        "{} ({})",
        crate::style::bold("omnifs setup"),
        host_os::name(os)
    );
    anstream::println!();
}

fn print_explainer(os: HostOs) {
    anstream::println!("{}", host_os::explain_runtime(os));
    anstream::println!();
}

async fn connect_runtime(os: HostOs, target: &DockerTarget) -> anyhow::Result<Runtime> {
    let runtime = Runtime::connect_for(target).context("connect to Docker daemon")?;
    runtime
        .ping()
        .await
        .context("Docker daemon did not respond")
        .with_hint(host_os::docker_install_hint(os))?;
    Ok(runtime)
}

/// Resolve which provider IDs to configure: explicit `--providers` wins,
/// otherwise an interactive `inquire::MultiSelect` over unconfigured providers.
fn resolve_selection(
    args: &SetupArgs,
    templates: &ProviderTemplates,
    configured: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<String>> {
    if !args.providers.is_empty() {
        return validate_preselected(&args.providers, templates, configured);
    }

    if !configured.is_empty() {
        anstream::println!("{}", crate::style::bold("Already configured"));
        for (id, mount) in configured {
            anstream::println!("  {id}  (mount: {mount})");
        }
        anstream::println!();
    }

    let mut selectable: Vec<&ProviderTemplate> = templates
        .iter()
        .map(|(_, tmpl)| tmpl)
        .filter(|tmpl| !configured.contains_key(&tmpl.manifest.id))
        .collect();
    if selectable.is_empty() {
        anstream::println!("All providers already configured. Nothing to add.");
        return Ok(Vec::new());
    }
    // Demote providers that require user-supplied state (PAT, fixture file) to
    // the bottom of the picker and uncheck them by default so the smoke path
    // for a fresh setup only enables providers that work with ambient or
    // browser-based auth.
    selectable.sort_by_key(|tmpl| (default_off(&tmpl.manifest.id), tmpl.manifest.id.clone()));

    let options: Vec<ProviderOption> = selectable
        .iter()
        .map(|tmpl| ProviderOption {
            id: tmpl.manifest.id.clone(),
            line: option_line(&tmpl.manifest),
        })
        .collect();
    let default_indices: Vec<usize> = options
        .iter()
        .enumerate()
        .filter_map(|(idx, opt)| (!default_off(&opt.id)).then_some(idx))
        .collect();

    let chosen = inquire::MultiSelect::new("Which providers do you want to configure?", options)
        .with_default(&default_indices)
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
    format!("{:<14} {}", manifest.id, summary)
}

/// A compact one-line capability summary for the multi-select row.
fn capability_summary(manifest: &ProviderManifest) -> Option<String> {
    if manifest.capabilities.is_empty() {
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
    if manifest.capabilities.len() > 3 {
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

fn validate_preselected(
    requested: &[String],
    templates: &ProviderTemplates,
    configured: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for id in requested {
        if templates.by_id(id).is_none() {
            anyhow::bail!(
                "provider `{id}` is not available; known: {}",
                templates.ids().collect::<Vec<_>>().join(", ")
            );
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
    templates: &ProviderTemplates,
    workspace: &Workspace,
) -> Vec<InitResult> {
    let mut out = Vec::new();
    for provider_name in selected {
        let Some(template) = templates.by_id(provider_name) else {
            out.push(InitResult {
                provider_name: provider_name.clone(),
                mount_name: provider_name.clone(),
                outcome: Err(format!("provider `{provider_name}` not found")),
            });
            continue;
        };
        let mount_name = template.manifest.default_mount.clone();

        anstream::println!();
        anstream::println!("{}", crate::style::bold(format!("--- {provider_name} ---")));

        if !args.yes {
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
        }

        let init_args = init::InitArgs {
            provider: Some(provider_name.clone()),
            as_name: None,
            no_input: false,
            yes: args.yes,
            no_browser: args.no_browser,
            token: None,
            token_env: None,
            scopes: Vec::new(),
        };
        let outcome = init_args
            .run_in_workspace(workspace)
            .await
            .map_err(|e| e.to_string());
        out.push(InitResult {
            provider_name: provider_name.clone(),
            mount_name,
            outcome,
        });
    }
    out
}

async fn launch_via_up() -> anyhow::Result<()> {
    anstream::println!();
    anstream::println!("Launching omnifs ...");
    up::UpArgs::default().run().await
}
