//! `omnifs mount` — add, list, re-authenticate, remove, or snapshot mounts.

pub(crate) mod add;
pub(crate) mod auth_import;
pub(crate) mod detect;
pub(crate) mod mount_file;
pub(crate) mod provider_selection;
pub(crate) mod snapshot;
pub(crate) mod spec_creation;
mod token_validation;
pub(crate) mod upgrade;

pub(crate) use add::AddArgs;
pub(crate) use add::{render_consent_block, run_static_token_init};
pub(crate) use auth_import::AuthImportDecision;
pub(crate) use auth_import::ImportOutcome;

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};
use omnifs_auth::{CredentialService, OAuthClient};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::mounts::Name as MountName;
use std::path::Path;
use std::sync::Arc;

use crate::credential_target::CredentialTarget;
use crate::error::{ExitCode, WithExitCode};
use crate::stages::PromptMode;
use crate::token_source::TokenSource;
use crate::ui::consent::{Decision, Outcome, Plan, Row};
use crate::ui::output::Output;
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;

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
    /// Repin one mount to an explicit or semver-selected provider artifact.
    Upgrade(upgrade::UpgradeArgs),
    /// Re-authenticate an existing mount.
    Reauth(ReauthArgs),
    /// Remove a mount config (and its stored credential, by default).
    Rm {
        name: String,
        /// Skip the credential delete.
        #[arg(long)]
        keep_credentials: bool,
        /// Print the removal plan without changing the workspace.
        #[arg(long)]
        dry_run: bool,
    },
    /// Export a mount's canonical cache to a directory.
    Snapshot(snapshot::SnapshotArgs),
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
            MountCommand::Upgrade(args) => args.run(output).await,
            MountCommand::Reauth(args) => args.run(output).await.map(|()| ExitCode::Success),
            MountCommand::Snapshot(args) => args.run(output).await,
            MountCommand::Rm {
                name,
                keep_credentials,
                dry_run,
            } => {
                let workspace = Workspace::resolve()?;
                rm_with_options(
                    &workspace,
                    &name,
                    output.yes(),
                    keep_credentials,
                    dry_run,
                    output,
                )
                .await
                .map(|()| ExitCode::Success)
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
            Column::new("Environment", Priority::Essential, WidthPolicy::Auto),
            Column::new("Path", Priority::Essential, WidthPolicy::Path),
            Column::new("State", Priority::Secondary, WidthPolicy::Auto),
        ],
    );
    for path in &result.access_paths {
        let state = match path.state {
            crate::inventory::AccessState::Available => StateToken::positive(path.state.label()),
            crate::inventory::AccessState::FrontendStopped
            | crate::inventory::AccessState::Offline => StateToken::neutral(path.state.label()),
            crate::inventory::AccessState::Failed => StateToken::failure(path.state.label()),
        };
        let row_state = state.clone();
        table.push(ResourceRow::new(
            [
                Cell::new(path.filesystem.label()),
                Cell::new(path.environment.label()),
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
    async fn run(self, output: Output) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let mut session = crate::ui::session::Session::intro_with_output(
            format!("omnifs mount reauth {}", self.name),
            output,
        )?;
        let prompt = PromptMode::from_flags(output.yes(), output.no_input());
        let result = self.run_in_session(&workspace, &mut session, prompt).await;
        if result.is_ok() {
            session.outro(format!("Re-authenticated `{}`.", self.name));
        }
        result
    }

    pub(crate) async fn run_in_session(
        &self,
        workspace: &Workspace,
        session: &mut crate::ui::session::Session,
        prompt: PromptMode,
    ) -> anyhow::Result<()> {
        let paths = workspace.layout();
        let mount_name = self.name.as_str();
        let mounts = workspace.mounts()?;
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

        let installed = crate::catalog::installed_providers(workspace.catalog())?;
        let provider_name = mount_config.config.provider_name();
        let (_, manifest) = crate::catalog::find_installed(&installed, provider_name.as_str())
            .ok_or_else(|| {
                anyhow!("provider `{provider_name}` for mount `{mount_name}` is not installed")
            })?;

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
            session.note(format!("re-authenticating `{mount_name}` over OAuth"));
            crate::auth::login_with_workspace(
                workspace,
                mount_name,
                selection.account.as_deref(),
                self.no_browser,
                prompt.no_input,
                &self.scopes,
                session,
            )
            .await?
        } else {
            let source = TokenSource::resolve(
                self.token.as_deref(),
                self.token_env.as_deref(),
                interactive,
            )?;
            let token = source.read()?;
            run_static_token_init(
                manifest,
                &selection,
                token,
                &paths.credentials_file,
                !self.no_validate,
                session,
            )
            .await?
        };
        for key in target.keys() {
            match workspace.daemon().reload_credential_if_ready(key).await {
                Ok(Some(_)) => {
                    session.row(crate::ui::report::Row::new(
                        crate::ui::style::Glyph::Done,
                        format!("credential `{key}`"),
                        "reloaded in running daemon",
                    ));
                },
                Ok(None) => {},
                Err(error) => {
                    session.row(crate::ui::report::Row::new(
                        crate::ui::style::Glyph::Warn,
                        format!("credential `{key}`"),
                        format!("stored, but live reload failed: {error:#}"),
                    ));
                    session.note("run `omnifs up` to restart with the new credential");
                },
            }
        }
        crate::telemetry::maybe_print_health_nudge(workspace, session.output()).await;
        Ok(())
    }
}

#[allow(dead_code)]
pub async fn rm(
    workspace: &Workspace,
    name: &str,
    yes: bool,
    keep_credentials: bool,
) -> anyhow::Result<()> {
    rm_with_options(
        workspace,
        name,
        yes,
        keep_credentials,
        false,
        Output::new(crate::ui::output::OutputMode::Human, false),
    )
    .await
}

#[allow(clippy::too_many_lines)] // plan, decision, and receipt stay linear
async fn rm_with_options(
    workspace: &Workspace,
    name: &str,
    yes: bool,
    keep_credentials: bool,
    dry_run: bool,
    output: Output,
) -> anyhow::Result<()> {
    let layout = workspace.layout();
    let mut session =
        crate::ui::session::Session::intro_with_output(format!("omnifs mount rm {name}"), output)?;
    let mounts = workspace.mounts()?;
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
                WorkspaceLayout::display(&layout.mounts_dir.join(format!("{name}.json")))
            ),
        ));
        session.plan(&plan);
        if let Some(suggestion) = mounts
            .iter()
            .map(|mount| mount.name.to_string())
            .find(|candidate| candidate.starts_with(name.as_str()))
        {
            session.note(format!("Did you mean `{suggestion}`?"));
        }
        let receipt = plan.receipt([Outcome::skip("spec", "already absent")]);
        session.receipt(&receipt);
        session.outro(format!("Mount `{name}` already absent."));
        return Ok(());
    };
    let config_path = mount.source.clone();
    // Build the plan without constructing an HTTP client or registering an
    // OAuth revocation. A dry run must stop before any apply-only setup.
    let planned_credential_target = CredentialTarget::for_mount(&mount.config);

    let plan = mount_remove_plan(
        name.as_str(),
        &config_path,
        &planned_credential_target,
        keep_credentials,
    );
    session.plan(&plan);
    match Decision::resolve(
        PromptMode::from_flags(yes || output.yes(), output.no_input()),
        dry_run,
        "-y",
        output,
    )? {
        Decision::DryRun => {
            session.outro("Dry run; no changes made.");
            return Ok(());
        },
        Decision::Apply => {},
    }

    let (service, credential_target) = if keep_credentials {
        (None, planned_credential_target)
    } else {
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(&layout.credentials_file));
        let service = CredentialService::new(store, OAuthClient::new()?);
        let target = crate::auth::MountAuth::from_spec(workspace.catalog(), mount.config.clone())
            .register_revocation(&service)?;
        (Some(service), target)
    };
    let credential_outcomes = if keep_credentials {
        if matches!(credential_target, CredentialTarget::Internal(_)) {
            vec![Outcome::skip("credential", "kept (--keep-credentials)")]
        } else {
            Vec::new()
        }
    } else {
        let service = service
            .as_ref()
            .ok_or_else(|| anyhow!("credential service missing during mount removal"))?;
        delete_credentials(service, &credential_target).await
    };
    let credential_outcomes = credential_outcomes
        .into_iter()
        .map(|outcome| outcome.with_id("credential"))
        .collect::<Vec<_>>();
    if let Some(failure) = credential_outcomes
        .iter()
        .find(|outcome| outcome.state == crate::ui::consent::OutcomeState::Fail)
    {
        // Credential deletion happens before spec mutation so a store failure
        // never leaves an on-disk mount pointing at a missing credential. The
        // plan has already been shown; settle its credential row before
        // returning the error instead of dropping the receipt on the floor.
        let message = failure.value.clone();
        let receipt = plan.receipt(credential_outcomes);
        session.receipt(&receipt);
        session.outro(format!("Removal failed for `{name}`."));
        anyhow::bail!(message);
    }

    let spec_id = "spec";
    let spec_outcome = match workspace
        .daemon()
        .delete_mount_if_ready(name.as_str())
        .await
    {
        Ok(Some(report)) if report.failure.is_none() => {
            Outcome::done(spec_id, "deleted (hot unload from running daemon)")
        },
        Ok(Some(report)) => {
            let reason = report
                .failure
                .as_ref()
                .map_or("unknown daemon error", |failure| failure.reason.as_str());
            Outcome::warn(spec_id, format!("deleted; hot unload failed ({reason})"))
        },
        Ok(None) => match workspace.remove_mount(&name) {
            Ok(true) => Outcome::done(spec_id, "deleted (cold delete; daemon not running)"),
            Ok(false) => Outcome::skip(spec_id, "already absent (cold delete)"),
            Err(error) => Outcome::fail(
                spec_id,
                format!("spec kept; local delete failed: {error:#}"),
            ),
        },
        Err(error) => match workspace.remove_mount(&name) {
            Ok(true) => Outcome::warn(
                spec_id,
                format!("deleted (cold delete; hot unload unavailable: {error:#})"),
            ),
            Ok(false) => Outcome::skip(
                spec_id,
                format!("already absent (cold delete; hot unload unavailable: {error:#})"),
            ),
            Err(local_error) => Outcome::fail(
                spec_id,
                format!(
                    "spec kept; hot unload failed ({error:#}); local delete failed: {local_error:#}"
                ),
            ),
        },
    };
    let mut outcomes = Vec::with_capacity(1 + credential_outcomes.len());
    outcomes.push(spec_outcome);
    outcomes.extend(credential_outcomes);
    let receipt = plan.receipt(outcomes);
    session.receipt(&receipt);
    session.outro(format!("Removed `{name}`."));
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
    Ok(())
}

pub(crate) async fn delete_credentials(
    service: &CredentialService,
    target: &CredentialTarget,
) -> Vec<Outcome> {
    // Credential delete happens before the mount-file delete so that on
    // store failure we don't orphan the mount config.
    let mut outcomes = Vec::new();
    for key in target.keys() {
        let outcome = service.revoke_and_delete(key).await;
        let typed = Outcome::credential(key, &outcome);
        let failed = typed.state == crate::ui::consent::OutcomeState::Fail;
        outcomes.push(typed);
        if failed {
            // The caller must settle this failure before touching the mount
            // spec. Keep any earlier per-key outcomes so the receipt is a
            // complete account of what happened.
            break;
        }
    }
    outcomes
}

fn mount_remove_plan(
    name: &str,
    config_path: &Path,
    target: &CredentialTarget,
    keep_credentials: bool,
) -> Plan {
    let mut plan = Plan::new("plan");
    plan.push(Row::remove(
        "spec",
        "spec",
        WorkspaceLayout::display(config_path).clone(),
    ));
    if matches!(target, CredentialTarget::Internal(_)) {
        if keep_credentials {
            plan.push(Row::keep(
                "credential",
                "credential",
                "kept (--keep-credentials)",
            ));
        } else {
            plan.push(Row::remove(
                "credential",
                "credential",
                format!("oauth `{name}` (revoke upstream)"),
            ));
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture_paths as base_fixture_paths;
    use omnifs_workspace::authn::CredentialId;
    use omnifs_workspace::creds::{CredentialEntry, MemoryStore};
    use secrecy::SecretString;
    use tempfile::TempDir;
    use time::OffsetDateTime;

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
        let err = rm(&workspace, "../leak", true, false).await.unwrap_err();
        assert!(format!("{err:#}").contains("invalid mount name"));
    }

    #[tokio::test]
    async fn removing_missing_valid_mount_is_a_noop_without_credentials() {
        let tmp = TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        let workspace = Workspace::from_layout(paths.clone());
        rm(&workspace, "missing", true, false).await.unwrap();
        assert!(!paths.credentials_file.exists());
    }

    #[tokio::test]
    async fn delete_credentials_deletes_internal_key() {
        let store = Arc::new(MemoryStore::new());
        let key = CredentialId::new("github", "device", "default").unwrap();
        let entry = CredentialEntry::static_token(
            SecretString::from("secret".to_owned()),
            OffsetDateTime::now_utc(),
        );
        store.put(&key, &entry).unwrap();

        let target = CredentialTarget::Internal(key.clone());
        let service = CredentialService::new(store.clone(), OAuthClient::new().unwrap());
        delete_credentials(&service, &target).await;

        assert!(store.get(&key).unwrap().is_none());
    }

    #[test]
    fn removal_plan_names_hot_unload_and_credential_rows() {
        let path = Path::new("/tmp/omnifs/mounts/github.json");
        let key = CredentialId::new("github", "oauth", "default").unwrap();
        let plan = mount_remove_plan("github", path, &CredentialTarget::Internal(key), false);
        assert_eq!(plan.remove_count(), 2);
        assert_eq!(plan.rows[0].id, "spec");
        assert!(plan.rows[1].value.contains("revoke upstream"));
    }
}
