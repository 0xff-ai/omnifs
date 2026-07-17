//! `omnifs mount` — add, list, re-authenticate, revoke, or remove mounts.

pub(crate) mod add;
pub(crate) mod auth_import;
pub(crate) mod detect;
pub(crate) mod mount_file;
pub(crate) mod provider_selection;
pub(crate) mod revoke;
pub(crate) mod spec_creation;
mod token_validation;

pub(crate) use add::AddArgs;
pub(crate) use add::{render_consent_block, run_static_token_init};
pub(crate) use auth_import::AuthImportDecision;
pub(crate) use auth_import::ImportOutcome;
pub(crate) use revoke::RevokeArgs;

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};
use omnifs_workspace::mounts::Name as MountName;
use std::path::Path;

use crate::error::{ExitCode, WithExitCode};
use crate::stages::PromptMode;
use crate::token_source::TokenSource;
use crate::ui::consent::{Decision, Outcome, Plan, Row};
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct MountArgs {
    #[command(subcommand)]
    pub command: MountCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MountCommand {
    /// Add and authenticate a mount.
    Add(AddArgs),
    /// List configured mounts with their provider and auth state.
    Ls(LsArgs),
    /// Show one configured mount and every derived frontend access path.
    Show(ShowArgs),
    /// Re-authenticate an existing mount.
    Reauth(ReauthArgs),
    /// Revoke the configured credential for an existing mount.
    Revoke(RevokeArgs),
    /// Remove a mount config.
    Rm {
        name: String,
        /// Print the removal plan without changing the workspace.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Args, Debug, Clone, Default)]
pub struct LsArgs {}

#[derive(Args, Debug, Clone)]
pub struct ShowArgs {
    /// Existing mount name.
    pub name: String,
}

#[derive(Args, Debug, Clone)]
pub struct ReauthArgs {
    /// Existing mount name to re-authenticate.
    pub name: String,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
    /// Read the static token from this source. Use `-` for stdin.
    #[arg(long, conflicts_with = "token_env")]
    pub token: Option<String>,
    /// Read the static token from this environment variable.
    #[arg(long, value_name = "ENV_VAR", conflicts_with = "token")]
    pub token_env: Option<String>,
    /// Store the static token without the provider's upstream validation
    /// probe (for CI or restricted tokens that fail the probe endpoint but
    /// work for their intended scope).
    #[arg(long)]
    pub no_validate: bool,
    /// OAuth scope to request. Repeat for multiple scopes.
    #[arg(long = "scope")]
    pub scopes: Vec<String>,
}

impl MountArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            MountCommand::Add(args) => args.run(output).await,
            MountCommand::Ls(args) => ls(&args, output).await,
            MountCommand::Show(args) => show(&args, output).await,
            MountCommand::Reauth(args) => {
                let receipt = args.run(output.clone()).await?;
                if output.is_structured() {
                    output.emit_result(ResultVerdict::Ok, receipt)?;
                }
                Ok(ExitCode::Success)
            },
            MountCommand::Revoke(args) => {
                let receipt = args.run(output.clone()).await?;
                if output.is_structured() {
                    output.emit_result(ResultVerdict::Ok, receipt)?;
                }
                Ok(ExitCode::Success)
            },
            MountCommand::Rm { name, dry_run } => {
                let workspace = Workspace::resolve()?;
                let receipt = rm_with_options(&workspace, &name, output.yes(), dry_run, &output)?;
                if output.is_structured() {
                    output.emit_result(receipt.output_verdict(), &receipt)?;
                }
                Ok(ExitCode::Success)
            },
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct MountsResult {
    mounts: Vec<crate::inventory::MountStatus>,
    verdict: crate::inventory::Verdict,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct MountShowResult {
    mount: crate::inventory::MountStatus,
    frontends: Vec<crate::inventory::FrontendStatus>,
    access_paths: Vec<crate::inventory::AccessPath>,
    verdict: crate::inventory::Verdict,
}

async fn ls(_args: &LsArgs, output: Output) -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let result = list_with_output(&workspace).await?;
    let exit_code = match result.verdict {
        crate::inventory::Verdict::Ok => ExitCode::Success,
        crate::inventory::Verdict::Degraded => ExitCode::Degraded,
    };
    if output.is_structured() {
        output.emit_result(result.verdict, &result)?;
    } else {
        crate::ui::print_raw(&render_mounts(&result));
    }
    Ok(exit_code)
}

async fn show(args: &ShowArgs, output: Output) -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let result = show_with_output(&workspace, &args.name).await?;
    if output.is_structured() {
        output.emit_result(result.verdict, &result)?;
    } else {
        crate::ui::print_raw(&render_mount_show(&result));
    }
    Ok(match result.verdict {
        crate::inventory::Verdict::Ok => ExitCode::Success,
        crate::inventory::Verdict::Degraded => ExitCode::Degraded,
    })
}

pub(crate) async fn list_with_output(workspace: &Workspace) -> anyhow::Result<MountsResult> {
    let inventory = crate::inventory::Inventory::collect(workspace).await?;
    let verdict = inventory.verdict();
    Ok(MountsResult {
        mounts: inventory.mounts,
        verdict,
    })
}

pub(crate) async fn show_with_output(
    workspace: &Workspace,
    name: &str,
) -> anyhow::Result<MountShowResult> {
    let inventory = crate::inventory::Inventory::collect(workspace).await?;
    let mount = inventory
        .mounts
        .iter()
        .find(|mount| mount.name == name)
        .cloned()
        .ok_or_else(|| anyhow!("no mount named `{name}`"))?;
    let mount_name = MountName::new(name.to_owned())?;
    let access_paths = inventory.access_paths(&mount_name);
    let verdict = inventory.verdict();
    Ok(MountShowResult {
        mount,
        frontends: inventory.frontends,
        access_paths,
        verdict,
    })
}

fn render_mounts(result: &MountsResult) -> String {
    let mut report = crate::ui::table::Report::new();
    report.push(crate::ui::table::Block::Resources(
        crate::status::mount_table(&result.mounts),
    ));
    report.render()
}

fn render_mount_show(result: &MountShowResult) -> String {
    use crate::ui::table::{
        Cell, Column, CountLabel, Priority, Report, ResourceRow, ResourceTable, StateToken,
        WidthPolicy,
    };
    let mut table = ResourceTable::new(
        "Access paths",
        CountLabel::number(result.access_paths.len()),
        vec![
            Column::new("Filesystem", Priority::Identity, WidthPolicy::Auto),
            Column::new("Runtime", Priority::Essential, WidthPolicy::Auto),
            Column::new("Path", Priority::Essential, WidthPolicy::Path),
            Column::new("State", Priority::Secondary, WidthPolicy::Auto),
        ],
    );
    for path in &result.access_paths {
        let state = match path.state {
            crate::inventory::AccessState::Available => StateToken::positive(path.state.label()),
            crate::inventory::AccessState::Offline => StateToken::neutral(path.state.label()),
            crate::inventory::AccessState::Failed => StateToken::failure(path.state.label()),
        };
        let row_state = state.clone();
        table.push(ResourceRow::new(
            [
                Cell::new(path.filesystem.label()),
                Cell::new(path.runtime.label()),
                Cell::new(path.path.display().to_string()),
                Cell::state(state),
            ],
            row_state,
        ));
    }
    let mut report = Report::new();
    report.push(crate::ui::table::Block::Resources(
        crate::status::mount_table(std::slice::from_ref(&result.mount)),
    ));
    report.push(crate::ui::table::Block::Resources(table));
    report.render()
}

impl ReauthArgs {
    async fn run(
        self,
        output: Output,
    ) -> anyhow::Result<crate::commands::receipt::MountReauthReceipt> {
        let workspace = Workspace::resolve()?;
        output.intro(format!("omnifs mount reauth {}", self.name))?;
        let prompt = PromptMode::from_flags(output.yes(), output.no_input());
        let result = self.run_with_output(&workspace, &output, prompt).await;
        if result.is_ok() {
            output.outro(format!("Re-authenticated `{}`.", self.name));
        }
        result?;
        Ok(crate::commands::receipt::MountReauthReceipt {
            verdict: crate::commands::receipt::Verdict::Ok,
            mount: self.name.clone(),
        })
    }

    pub(crate) async fn run_with_output(
        &self,
        workspace: &Workspace,
        output: &crate::ui::output::Output,
        prompt: PromptMode,
    ) -> anyhow::Result<()> {
        let mount_name = self.name.as_str();
        let mounts = workspace.desired_state().mounts()?;
        let mount_config = mounts
            .iter()
            .find(|m| m.name.as_str() == mount_name)
            .ok_or_else(|| {
                anyhow!(
                    "no mount named `{mount_name}`; run `omnifs mount add <provider>` to create it"
                )
            })?;
        let Some(auth) = mount_config.config.auth.as_ref() else {
            anyhow::bail!("mount `{mount_name}` needs no authentication");
        };

        let provider = workspace
            .catalog()
            .get(&mount_config.config.provider.id)?
            .ok_or_else(|| {
                anyhow!(
                    "provider artifact `{}` for mount `{mount_name}` is missing",
                    mount_config.config.provider.id
                )
            })?;
        let manifest = provider.manifest()?;

        let selection = crate::auth::AuthSelection {
            auth_type: auth.kind(),
            scheme: auth.scheme().map(str::to_owned),
            account: auth.account().map(str::to_owned),
        };

        // `--no-input` must never reach an OAuth browser handoff (it would hang
        // on the browser confirm or the manual-code paste). Mirror the add-side
        // guard: bail naming the interactive and static-token alternatives.
        let interactive = prompt.interactive;
        if !interactive && selection.is_oauth() {
            return Err(anyhow!(
                "`omnifs mount reauth {mount_name}` cannot complete OAuth without a terminal; run it interactively, or use a static-token scheme with --token - or --token-env VAR"
            ))
            .with_exit_code(ExitCode::AuthRequired);
        }

        let target = if selection.is_oauth() {
            output.note(format!("re-authenticating `{mount_name}` over OAuth"));
            crate::auth::login_with_workspace(
                workspace,
                mount_name,
                selection.account.as_deref(),
                self.no_browser,
                prompt.no_input,
                &self.scopes,
                output,
            )
            .await?
        } else {
            let source = TokenSource::resolve(
                self.token.as_deref(),
                self.token_env.as_deref(),
                interactive,
            )?;
            let token = source.read(output)?;
            run_static_token_init(
                &manifest,
                &selection,
                token,
                workspace.credentials(),
                !self.no_validate,
                output,
            )
            .await?
        };
        for key in target.keys() {
            output.row(&crate::ui::report::Row::new(
                crate::ui::style::Glyph::Done,
                format!("credential `{key}`"),
                "stored; takes effect on the next `omnifs up` or `omnifs apply`",
            ));
        }
        crate::metrics::maybe_print_health_nudge(workspace, output.clone()).await;
        Ok(())
    }
}

#[allow(dead_code)]
pub fn rm(workspace: &Workspace, name: &str, yes: bool) -> anyhow::Result<()> {
    rm_with_options(
        workspace,
        name,
        yes,
        false,
        &Output::new(crate::ui::output::OutputMode::Human, false),
    )
    .map(|_| ())
}

#[allow(clippy::too_many_lines)] // plan, decision, and receipt stay linear
fn rm_with_options(
    workspace: &Workspace,
    name: &str,
    yes: bool,
    dry_run: bool,
    output: &Output,
) -> anyhow::Result<crate::commands::receipt::MountRemoveReceipt> {
    output.intro(format!("omnifs mount rm {name}"))?;
    let output = output.clone();
    let mounts = workspace.desired_state().mounts()?;
    let name =
        MountName::new(name.to_owned()).with_context(|| format!("invalid mount name `{name}`"))?;

    let Some(mount) = mounts.iter().find(|m| m.name == name) else {
        // Removing an already-absent valid mount is an idempotent cleanup
        // operation. Emit the same plan/receipt shape as other destructive
        // commands, but never construct a credential service or touch the
        // credential store when there is no spec to remove.
        let mut plan = Plan::new("plan");
        plan.push(Row::keep(
            "spec",
            "spec",
            format!(
                "{} (already absent)",
                omnifs_workspace::layout::display(&workspace.desired_state().spec_path(&name))
            ),
        ));
        output.plan(&plan);
        if let Some(suggestion) = mounts
            .iter()
            .map(|mount| mount.name.to_string())
            .find(|candidate| candidate.starts_with(name.as_str()))
        {
            output.note(format!("Did you mean `{suggestion}`?"));
        }
        if dry_run {
            output.outro("Dry run; no changes made.");
            return Ok(crate::commands::receipt::MountRemoveReceipt::dry_run(
                name.to_string(),
                plan,
            ));
        }
        let receipt = plan.receipt([Outcome::skip("spec", "already absent")]);
        output.receipt(&receipt);
        output.outro(format!("Mount `{name}` already absent."));
        return Ok(crate::commands::receipt::MountRemoveReceipt::applied(
            name.to_string(),
            plan,
            receipt.rows,
        ));
    };
    let config_path = mount.source.clone();
    // Build the plan without constructing an HTTP client or registering an
    // OAuth revocation. A dry run must stop before any apply-only work.
    let plan = mount_remove_plan(&config_path);
    output.plan(&plan);
    match Decision::resolve(
        PromptMode::from_flags(yes || output.yes(), output.no_input()),
        dry_run,
        "-y",
        &output,
    )? {
        Decision::DryRun => {
            output.outro("Dry run; no changes made.");
            return Ok(crate::commands::receipt::MountRemoveReceipt::dry_run(
                name.to_string(),
                plan,
            ));
        },
        Decision::Apply => {},
    }

    let spec_outcome = match workspace.desired_state().remove_uncommitted(&name) {
        Ok(true) => Outcome::done("spec", "desired-state deletion recorded"),
        Ok(false) => Outcome::skip("spec", "already absent"),
        Err(error) => Outcome::fail("spec", format!("spec kept; local delete failed: {error:#}")),
    };
    let mut outcomes = vec![spec_outcome];
    if outcomes[0].state != crate::ui::consent::OutcomeState::Fail
        && let Err(error) = workspace.desired_state().commit()
    {
        outcomes[0] = Outcome::fail(
            "spec",
            format!("deleted locally; desired-state commit failed: {error:#}"),
        );
    }
    let receipt = plan.receipt(outcomes);
    output.receipt(&receipt);
    output.outro(format!("Removed `{name}`."));
    if receipt
        .rows
        .iter()
        .any(|row| row.id == "spec" && row.state == crate::ui::consent::OutcomeState::Fail)
    {
        anyhow::bail!(
            receipt
                .rows
                .iter()
                .find(|row| row.id == "spec")
                .map_or_else(
                    || "mount spec removal failed".to_owned(),
                    |row| row.value.clone()
                )
        );
    }
    Ok(crate::commands::receipt::MountRemoveReceipt::applied(
        name.to_string(),
        plan,
        receipt.rows,
    ))
}

fn mount_remove_plan(config_path: &Path) -> Plan {
    let mut plan = Plan::new("plan");
    plan.push(Row::remove(
        "spec",
        "spec",
        omnifs_workspace::layout::display(config_path).clone(),
    ));
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture_paths as base_fixture_paths;
    use omnifs_workspace::layout::WorkspaceLayout;
    use tempfile::TempDir;

    fn fixture_paths(root: &Path) -> WorkspaceLayout {
        let paths = base_fixture_paths(root);
        std::fs::create_dir_all(&paths.mounts_dir).unwrap();
        paths
    }

    #[tokio::test]
    async fn rejects_invalid_mount_name() {
        let tmp = TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        let workspace = Workspace::from_layout(paths);
        let err = rm(&workspace, "../leak", true).unwrap_err();
        assert!(format!("{err:#}").contains("invalid mount name"));
    }

    #[tokio::test]
    async fn removing_missing_valid_mount_is_a_noop_without_credentials() {
        let tmp = TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        let workspace = Workspace::from_layout(paths.clone());
        rm(&workspace, "missing", true).unwrap();
        assert!(!paths.credentials_file.exists());
    }

    #[test]
    fn removal_plan_names_desired_state_row() {
        let path = Path::new("/tmp/omnifs/mounts/github.json");
        let plan = mount_remove_plan(path);
        assert_eq!(plan.remove_count(), 1);
        assert_eq!(plan.rows[0].id, "spec");
    }
}
