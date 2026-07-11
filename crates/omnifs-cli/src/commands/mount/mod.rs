//! `omnifs mount` — add, list, re-authenticate, remove, or snapshot mounts.

pub(crate) mod add;
pub(crate) mod auth_import;
pub(crate) mod detect;
pub(crate) mod mount_file;
pub(crate) mod provider_selection;
pub(crate) mod snapshot;
pub(crate) mod spec_creation;
mod token_validation;

pub(crate) use add::AddArgs;
pub(crate) use add::{render_consent_block, run_static_token_init};
pub(crate) use auth_import::AuthImportDecision;
pub(crate) use auth_import::ImportOutcome;

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand};
use omnifs_auth::{CredentialService, OAuthClient};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::mounts::Name as MountName;
use std::path::Path;
use std::sync::Arc;

use crate::credential_target::CredentialTarget;
use crate::error::{ExitCode, WithExitCode};
use crate::mount_config::MountConfig;
use crate::stages::PromptMode;
use crate::token_source::TokenSource;
use crate::ui::consent::{Decision, Outcome, Plan, Row};
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
    /// Re-authenticate an existing mount.
    Reauth(ReauthArgs),
    /// Remove a mount config (and its stored credential, by default).
    Rm {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
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
pub struct LsArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ReauthArgs {
    /// Existing mount name to re-authenticate.
    pub name: String,
    /// Skip prompts. Static-token mounts also require --token or --token-env.
    #[arg(long)]
    pub no_input: bool,
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
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self.command {
            MountCommand::Add(args) => args.run().await,
            MountCommand::Ls(args) => ls(&args),
            MountCommand::Reauth(args) => args.run().await.map(|()| ExitCode::Success),
            MountCommand::Snapshot(args) => args.run().await.map(|()| ExitCode::Success),
            MountCommand::Rm {
                name,
                yes,
                keep_credentials,
                dry_run,
            } => {
                let workspace = Workspace::resolve()?;
                rm_with_options(&workspace, &name, yes, keep_credentials, dry_run)
                    .await
                    .map(|()| ExitCode::Success)
            },
        }
    }
}

#[derive(serde::Serialize)]
struct MountsJson {
    mounts: Vec<crate::status::UserMountStatus>,
}

fn ls(args: &LsArgs) -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let layout = workspace.layout();
    let mounts = workspace.mounts()?;
    let store = FileStore::new(&layout.credentials_file);
    let statuses =
        crate::mount_report::scan_user_mount_configs(workspace.catalog(), &mounts, &store);
    let exit_code = if statuses.iter().any(|status| match status {
        crate::status::UserMountStatus::Ready(mount) => matches!(
            mount.auth.terminal_row().kind,
            crate::auth::AuthTerminalKind::Missing | crate::auth::AuthTerminalKind::Error
        ),
        crate::status::UserMountStatus::Invalid { .. } => true,
    }) {
        ExitCode::Degraded
    } else {
        ExitCode::Success
    };
    if args.json {
        crate::ui::print_json(&MountsJson { mounts: statuses })?;
        return Ok(exit_code);
    }
    // Same rendering as status's Mounts section, from the one shared row owner.
    let mut section = crate::ui::report::Section::new("Mounts").counted(statuses.len());
    if statuses.is_empty() {
        section.push(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Skip,
            "",
            "no mounts configured; run `omnifs mount add <provider>`",
        ));
    } else {
        for status in &statuses {
            section.push(crate::mount_report::mount_row(status));
        }
    }
    let mut report = crate::ui::report::Report::new();
    report.push(section);
    report.print();
    Ok(exit_code)
}

impl ReauthArgs {
    async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let mut session =
            crate::ui::session::Session::intro(format!("omnifs mount reauth {}", self.name))?;
        let result = self.run_in_session(&workspace, &mut session).await;
        if result.is_ok() {
            session.outro(format!("Re-authenticated `{}`.", self.name));
        }
        result
    }

    pub(crate) async fn run_in_session(
        &self,
        workspace: &Workspace,
        session: &mut crate::ui::session::Session,
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
        let interactive = !self.no_input && crate::ui::prompt::is_terminal();
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
                self.no_input,
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
        crate::telemetry::maybe_print_health_nudge(workspace).await;
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
    rm_with_options(workspace, name, yes, keep_credentials, false).await
}

#[allow(clippy::too_many_lines)] // plan, decision, and receipt stay linear
async fn rm_with_options(
    workspace: &Workspace,
    name: &str,
    yes: bool,
    keep_credentials: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let layout = workspace.layout();
    let mut session = crate::ui::session::Session::intro(format!("omnifs mount rm {name}"))?;
    let mounts = workspace.mounts()?;
    let name =
        MountName::new(name.to_owned()).with_context(|| format!("invalid mount name `{name}`"))?;

    let mount = mounts.iter().find(|m| m.name == name).ok_or_else(|| {
        missing_mount_error(&layout.config_file, &mounts, name.as_str()).unwrap_err()
    })?;
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
    match Decision::resolve(PromptMode::from_flags(yes, false), dry_run, "-y")? {
        Decision::Decline => {
            session.outro("Removal aborted.");
            return Ok(());
        },
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

fn missing_mount_error(
    config_file: &Path,
    mounts: &[MountConfig],
    name: &str,
) -> anyhow::Result<()> {
    let suggestion = crate::mount_report::closest_mount_name(mounts, name);
    let mut message = format!(
        "no mount config named `{name}` in {}",
        WorkspaceLayout::display(config_file)
    );
    if let Some(suggestion) = suggestion {
        let _ = std::fmt::Write::write_fmt(
            &mut message,
            format_args!("\nDid you mean `{suggestion}`?"),
        );
    }
    bail!(message)
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
