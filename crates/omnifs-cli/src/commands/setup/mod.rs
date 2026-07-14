//! `omnifs setup`: the small onboarding composition over the normal commands.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::anyhow;
use clap::Args;
use omnifs_workspace::provider::{Provider, ProviderAuthManifest, ProviderManifest};

use crate::commands::frontend::{
    FrontendEnableArgs, FrontendEnvironment, FrontendFilesystem, FrontendResult, RuntimeState,
};
use crate::commands::{mount, up::UpArgs};
use crate::stages::PromptMode;
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

pub mod host_os;

use self::host_os::HostOs;

#[derive(Args, Debug, Clone, Default)]
pub struct SetupArgs {
    /// Skip the final daemon launch.
    #[arg(long)]
    pub no_up: bool,
    /// Preselect providers and skip the picker.
    #[arg(long, value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
}

impl SetupArgs {
    #[allow(clippy::too_many_lines)] // setup's onboarding phases stay linear
    pub async fn run(self, output: Output) -> anyhow::Result<()> {
        let mode =
            PromptMode::from_flags(output.yes(), output.no_input() || output.is_structured());
        let mut session = crate::ui::session::Session::intro_with_output("omnifs setup", output)?;
        let os = HostOs::detect();
        if os == HostOs::Unsupported {
            anyhow::bail!("omnifs does not yet run on this platform");
        }

        let workspace = Workspace::resolve()?;
        let config = workspace.config()?;
        let frontend_defaults = platform_frontend_defaults(os);
        session.note(format!(
            "omnifs mounts services as regular files on {}.",
            os.name()
        ));
        if frontend_defaults
            .iter()
            .any(|entry| matches!(entry.environment, FrontendEnvironment::Docker))
        {
            render_docker_reachability(&config, &mut session).await;
        }

        let inventory = crate::inventory::Inventory::collect(&workspace).await?;
        let mounts = &inventory.desired_mounts;
        if !mounts.is_empty() && self.providers.is_empty() && !mode.yes {
            render_review(&workspace, output).await?;
            if mode.no_input {
                anyhow::bail!(
                    "`omnifs setup --no-input` is in review mode; pass --providers <provider> or --yes"
                );
            }
            if !mode.interactive {
                anyhow::bail!(
                    "`omnifs setup` is in review mode and needs a terminal; pass --providers <provider> or --yes"
                );
            }
            return Ok(());
        }

        let installed = crate::catalog::installed_providers(workspace.catalog())?;
        if installed.is_empty() {
            anyhow::bail!("no built-in or plugin providers are available");
        }
        let configured = mounts
            .iter()
            .map(|mount| {
                (
                    mount.config.provider_name().to_string(),
                    mount.config.mount.clone(),
                )
            })
            .collect();

        session.phase("1/4 environment");
        session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Done,
            "environment",
            format!("{}; {} providers installed", os.name(), installed.len()),
        ));

        session.phase("2/4 what should omnifs mount?");
        let selected = self.resolve_selection(&installed, &configured, mode, &mut session)?;
        for provider_name in selected {
            let Some((_, manifest)) = crate::catalog::find_installed(&installed, &provider_name)
            else {
                anyhow::bail!("provider `{provider_name}` not found");
            };
            let verb = if manifest.auth.is_some() {
                "sign in"
            } else {
                "mount"
            };
            session.phase(format!("3/4 {provider_name} {verb}"));
            crate::stages::configure_mount(
                mount::AddArgs {
                    provider: Some(provider_name),
                    as_name: None,
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
                },
                &workspace,
                false,
                &mut session,
                mode,
            )
            .await?;
        }

        let has_mounts = !workspace.mounts()?.is_empty();
        if has_mounts {
            workspace.commit_mounts()?;
        }
        if self.no_up {
            session.outro("You're set. Run `omnifs up` when ready.");
            return emit_inventory_if_structured(&workspace, output).await;
        }
        if !has_mounts {
            session.outro("No mounts yet. Add one with `omnifs mount add <provider>`.");
            return emit_inventory_if_structured(&workspace, output).await;
        }

        UpArgs::default()
            .start_in_workspace(&workspace, output)
            .await?;
        for frontend in frontend_defaults {
            let result = frontend.enable(&workspace, output).await?;
            render_frontend_result(&mut session, result);
        }
        session.outro("You're set. Try `omnifs shell`.");
        emit_inventory_if_structured(&workspace, output).await
    }

    fn resolve_selection(
        &self,
        installed: &[(Provider, ProviderManifest)],
        configured: &BTreeMap<String, String>,
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

    fn yes_auto_select(
        installed: &[(Provider, ProviderManifest)],
        configured: &BTreeMap<String, String>,
        session: &mut crate::ui::session::Session,
    ) -> Vec<String> {
        let mut selected = Vec::new();
        let mut skipped = Vec::new();
        for (provider, manifest) in installed {
            let name = provider.meta.name.to_string();
            if configured.contains_key(&name) {
                continue;
            }
            if manifest.requires_mount_input() {
                skipped.push(format!("{name} (needs configuration)"));
                continue;
            }
            let auth_manifest = manifest
                .auth
                .as_ref()
                .map(ProviderAuthManifest::wasm_auth_manifest);
            let ambient =
                !crate::commands::mount::detect::detect(auth_manifest.as_ref()).is_empty();
            if manifest.auth.is_none() || ambient {
                selected.push(name);
            } else {
                skipped.push(format!("{name} (needs credentials)"));
            }
        }
        if !selected.is_empty() {
            session.note(format!("auto-selected {}", selected.join(", ")));
        }
        for provider in skipped {
            session.note(format!("skipped {provider}"));
        }
        selected
    }
}

fn platform_frontend_defaults(os: HostOs) -> Vec<FrontendEnableArgs> {
    match os {
        HostOs::MacOs => vec![
            FrontendEnableArgs {
                filesystem: FrontendFilesystem::Nfs,
                environment: FrontendEnvironment::Host,
                location: None,
            },
            FrontendEnableArgs {
                filesystem: FrontendFilesystem::Fuse,
                environment: FrontendEnvironment::Docker,
                location: None,
            },
        ],
        HostOs::LinuxNative | HostOs::LinuxWsl => vec![FrontendEnableArgs {
            filesystem: FrontendFilesystem::Fuse,
            environment: FrontendEnvironment::Host,
            location: None,
        }],
        HostOs::Unsupported => Vec::new(),
    }
}

async fn render_docker_reachability(
    config: &omnifs_workspace::config::Config,
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
                "not reachable; `omnifs up` may need Docker Desktop",
            ));
        },
    }
}

fn render_frontend_result(session: &mut crate::ui::session::Session, result: FrontendResult) {
    let glyph = match result.state {
        RuntimeState::Attached => crate::ui::style::Glyph::Done,
        RuntimeState::Stopped | RuntimeState::Failed => crate::ui::style::Glyph::Warn,
    };
    let state = match result.state {
        RuntimeState::Attached => "attached",
        RuntimeState::Stopped => "daemon stopped",
        RuntimeState::Failed => "failed",
    };
    let action = if result.changed { "started" } else { "ready" };
    let mut detail = format!("{} {state} ({action})", result.id);
    if let Some(message) = result.detail {
        let _ = write!(&mut detail, ": {message}");
    }
    if let Some(fix) = result.fix {
        let _ = write!(&mut detail, "; fix: {fix}");
    }
    session.row(crate::ui::report::Row::new(glyph, "frontend", detail));
}

async fn render_review(workspace: &Workspace, output: Output) -> anyhow::Result<()> {
    let inventory = crate::inventory::Inventory::collect(workspace).await?;
    if output.is_structured() {
        output.emit_result(ResultVerdict::from(inventory.verdict()), inventory)?;
    } else {
        crate::status::InventoryReport { inventory }
            .render(false)
            .print();
    }
    Ok(())
}

async fn emit_inventory_if_structured(workspace: &Workspace, output: Output) -> anyhow::Result<()> {
    if output.is_structured() {
        let inventory = crate::inventory::Inventory::collect(workspace).await?;
        output.emit_result(ResultVerdict::from(inventory.verdict()), inventory)?;
    }
    Ok(())
}

fn validate_preselected(
    requested: &[String],
    installed: &[(Provider, ProviderManifest)],
    configured: &BTreeMap<String, String>,
    session: &mut crate::ui::session::Session,
) -> anyhow::Result<Vec<String>> {
    let known = installed
        .iter()
        .map(|(provider, _)| provider.meta.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    requested
        .iter()
        .map(|id| {
            if crate::catalog::find_installed(installed, id).is_none() {
                return Err(anyhow!("provider `{id}` is not available; known: {known}"));
            }
            if let Some(mount) = configured.get(id) {
                session.row(crate::ui::report::Row::new(
                    crate::ui::style::Glyph::Skip,
                    id,
                    format!("already configured as {mount}"),
                ));
                return Ok(None);
            }
            Ok(Some(id.clone()))
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|ids| ids.into_iter().flatten().collect())
}
