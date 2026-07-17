//! `omnifs setup`: thin first-run composition over existing operations.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use clap::Args;

use crate::commands::frontend::{
    FrontendEnableArgs, FrontendFilesystem, FrontendResult, FrontendResultState, FrontendRuntime,
};
use crate::commands::mount::AddArgs;
use crate::commands::up::UpArgs;
use crate::error::ExitCode;
use crate::inventory::{Inventory, Verdict};
use crate::provider_bundle::EmbeddedProviders;
use crate::provider_resolver::{provider_options, safe_for_setup};
use crate::stages::PromptMode;
use crate::status::InventoryReport;
use crate::ui::output::Output;
use crate::ui::report::Row;
use crate::ui::style::Glyph;
use omnifs_workspace::Workspace;

const FINISH: &str = "__omnifs_setup_finish__";

#[derive(Args, Debug, Clone, Default)]
pub struct SetupArgs {
    /// Configure these exact embedded provider names. Repeat or comma-separate.
    #[arg(long, value_name = "PROVIDER", value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Configure mounts without starting the daemon or enabling frontends.
    #[arg(long)]
    pub no_up: bool,
    /// Print OAuth URLs instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
}

impl SetupArgs {
    pub async fn run(self, output: Output) -> Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        self.run_in_workspace(&workspace, output).await
    }

    async fn run_in_workspace(self, workspace: &Workspace, output: Output) -> Result<ExitCode> {
        output.intro("omnifs setup")?;
        let prompt =
            PromptMode::from_flags(output.yes(), output.no_input() || output.is_structured());
        let selected = self.select_providers(workspace, &output, prompt)?;
        let configure_prompt = Self::configure_prompt(&output, prompt);

        for provider in selected {
            crate::stages::configure_mount(
                AddArgs {
                    provider: Some(provider),
                    as_name: None,
                    no_browser: self.no_browser,
                    token: None,
                    token_env: None,
                    no_validate: false,
                    scopes: Vec::new(),
                    scheme: None,
                    no_auth: false,
                    config_json: None,
                    limits_json: None,
                },
                workspace,
                &output,
                configure_prompt,
            )
            .await?;
        }

        if !self.no_up && !crate::mount_config::load_mounts(workspace)?.is_empty() {
            output.narrate("Starting the daemon.");
            UpArgs::default()
                .start_in_workspace(workspace, output.clone())
                .await?;
            for (filesystem, runtime) in Self::default_frontends()? {
                let result = FrontendEnableArgs {
                    filesystem,
                    runtime: Some(runtime),
                    location: None,
                }
                .enable(workspace, output.clone())
                .await?;
                Self::narrate_frontend(&output, &result);
            }
        }

        let inventory = Inventory::collect(workspace).await?;
        let exit_code = match inventory.verdict() {
            Verdict::Ok => ExitCode::Success,
            Verdict::Degraded => ExitCode::Degraded,
        };
        if output.is_structured() {
            output.emit_result(inventory.verdict(), inventory)?;
        } else {
            InventoryReport { inventory }.render().print();
            output.outro("Setup complete.");
        }
        Ok(exit_code)
    }

    fn select_providers(
        &self,
        workspace: &Workspace,
        output: &Output,
        prompt: PromptMode,
    ) -> Result<Vec<String>> {
        let embedded = EmbeddedProviders::load()?;
        let mounts = crate::mount_config::load_mounts(workspace)?;
        let mut configured = mounts
            .iter()
            .map(|mount| {
                (
                    mount.config.provider.meta.name.to_string(),
                    mount.name.to_string(),
                )
            })
            .collect::<BTreeMap<_, _>>();

        if !self.providers.is_empty() {
            let mut seen = BTreeSet::new();
            for provider in &self.providers {
                if seen.insert(provider) && embedded.by_name(provider).is_none() {
                    bail!(
                        "provider `{provider}` is not an exact embedded provider name; pass one of: {}",
                        embedded.names().collect::<Vec<_>>().join(", ")
                    );
                }
            }

            return Ok(self
                .providers
                .iter()
                .filter(|provider| {
                    if configured.contains_key(*provider) {
                        output.row(&Row::new(
                            Glyph::Skip,
                            "provider",
                            format!("{provider} already configured"),
                        ));
                        false
                    } else {
                        configured.insert((*provider).clone(), "setup".to_owned());
                        true
                    }
                })
                .cloned()
                .collect());
        }

        if output.yes() {
            let selected = provider_options(&embedded, &configured)
                .into_iter()
                .filter(|option| {
                    embedded
                        .by_name(&option.name)
                        .is_some_and(|provider| safe_for_setup(provider.manifest()))
                })
                .map(|option| option.name)
                .collect();
            return Ok(selected);
        }

        if prompt.no_input || !prompt.interactive {
            bail!("setup needs --providers <NAME>, or pass --yes to select safe providers");
        }

        let mut selected = Vec::new();
        loop {
            let options = provider_options(&embedded, &configured);
            if options.is_empty() {
                break;
            }
            let choices = options
                .into_iter()
                .map(|option| (option.name.clone(), option.name, option.hint))
                .chain(std::iter::once((
                    FINISH.to_owned(),
                    "finish setup".to_owned(),
                    String::new(),
                )));
            let provider = crate::ui::prompt::Select::new("Which provider should setup configure?")
                .options(choices)
                .ask_with_output(output)?;
            if provider == FINISH {
                break;
            }
            configured.insert(provider.clone(), "setup".to_owned());
            selected.push(provider);
        }
        Ok(selected)
    }

    fn configure_prompt(output: &Output, prompt: PromptMode) -> PromptMode {
        if output.yes() {
            PromptMode {
                interactive: false,
                yes: true,
                no_input: true,
            }
        } else {
            prompt
        }
    }

    fn default_frontends() -> Result<Vec<(FrontendFilesystem, FrontendRuntime)>> {
        if cfg!(target_os = "linux") {
            return Ok(vec![(FrontendFilesystem::Fuse, FrontendRuntime::Host)]);
        }
        if cfg!(target_os = "macos") {
            return Ok(vec![
                (FrontendFilesystem::Nfs, FrontendRuntime::Host),
                (FrontendFilesystem::Fuse, FrontendRuntime::Docker),
            ]);
        }
        bail!(
            "omnifs setup is unsupported on {}; configure mounts with `omnifs mount add`",
            std::env::consts::OS
        )
    }

    fn narrate_frontend(output: &Output, result: &FrontendResult) {
        let value = match result.state {
            FrontendResultState::Attached => format!("{} attached", result.id),
            FrontendResultState::Stopped => format!("{} stopped", result.id),
            FrontendResultState::Failed => result
                .detail
                .as_deref()
                .map_or_else(|| format!("{} failed", result.id), str::to_owned),
        };
        let glyph = match result.state {
            FrontendResultState::Attached => Glyph::Done,
            FrontendResultState::Stopped => Glyph::Skip,
            FrontendResultState::Failed => Glyph::Warn,
        };
        output.row(&Row::new(glyph, "frontend", value));
        if let Some(fix) = &result.fix {
            output.note(fix);
        }
    }
}
