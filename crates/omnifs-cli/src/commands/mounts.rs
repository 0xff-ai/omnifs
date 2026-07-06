//! `omnifs mounts` — list, re-authenticate, or remove configured mounts.

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand};
use omnifs_auth::{CredentialService, OAuthClient};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::mounts::Name as MountName;
use omnifs_workspace::mounts::Registry;
use std::path::Path;
use std::sync::Arc;

use crate::credential_target::CredentialTarget;
use crate::error::{ExitCode, WithExitCode};
use crate::mount_config::MountConfig;
use crate::token_source::TokenSource;
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;
use std::io::IsTerminal as _;

#[derive(Args, Debug, Clone)]
pub struct MountsArgs {
    #[command(subcommand)]
    pub command: MountsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MountsCommand {
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
    },
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

impl MountsArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self.command {
            MountsCommand::Ls(args) => ls(&args),
            MountsCommand::Reauth(args) => args.run().await.map(|()| ExitCode::Success),
            MountsCommand::Rm {
                name,
                yes,
                keep_credentials,
            } => {
                let workspace = Workspace::resolve()?;
                rm(&workspace, &name, yes, keep_credentials)
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
        crate::mount_report::scan_user_mount_configs(workspace.catalog(), mounts.clone(), &store);
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
        let payload = MountsJson { mounts: statuses };
        anstream::println!("{}", serde_json::to_string(&payload)?);
        return Ok(exit_code);
    }
    if statuses.is_empty() {
        anstream::println!("No mounts configured.");
        anstream::println!("Run `omnifs init` to add one.");
        return Ok(ExitCode::Success);
    }
    for status in &statuses {
        anstream::println!("{}", render_mount_row(status));
    }
    Ok(exit_code)
}

/// One text row per mount, sourced from the same scan that drives the JSON
/// output and exit code. Invalid mounts (missing or corrupt provider artifact)
/// render explicitly as a broken row rather than masquerading as normal.
fn render_mount_row(status: &crate::status::UserMountStatus) -> String {
    use crate::auth::AuthTerminalKind;
    use crate::status::UserMountStatus;
    match status {
        UserMountStatus::Ready(mount) => {
            let row = mount.auth.terminal_row();
            let glyph = match row.kind {
                AuthTerminalKind::None => crate::style::dim("◯"),
                AuthTerminalKind::Ready => crate::style::success("●"),
                AuthTerminalKind::Missing | AuthTerminalKind::Error => crate::style::error("●"),
            };
            format!(
                "{glyph}  {}  {}  {}",
                crate::style::bold(&mount.mount),
                crate::style::dim(&mount.provider),
                row.summary,
            )
        },
        UserMountStatus::Invalid { config_path, error } => {
            let name = config_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unknown>");
            format!(
                "{}  {}  invalid ({error})",
                crate::style::error("●"),
                crate::style::bold(name),
            )
        },
    }
}

impl ReauthArgs {
    async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        self.run_in_workspace(&workspace).await
    }

    /// Re-acquire the credential for an existing mount: OAuth login or a fresh
    /// static token, dispatched on the mount's stored auth. The spec is left
    /// untouched; only the credential store changes.
    pub(crate) async fn run_in_workspace(self, workspace: &Workspace) -> anyhow::Result<()> {
        let paths = workspace.layout();
        let mount_name = self.name.as_str();
        let mounts = workspace.mounts()?;
        let mount_config = mounts
            .iter()
            .find(|m| m.name.as_str() == mount_name)
            .ok_or_else(|| {
                anyhow!("no mount named `{mount_name}`; run `omnifs init <provider>` to create it")
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
        // on the browser confirm or the manual-code paste). Mirror the init-side
        // guard: bail naming the interactive and static-token alternatives.
        if self.no_input && selection.is_oauth() {
            return Err(anyhow!(
                "`omnifs mounts reauth {mount_name} --no-input` cannot complete OAuth; run it interactively, or use a static-token scheme with --token - or --token-env VAR"
            ))
            .with_exit_code(ExitCode::AuthRequired);
        }

        let target = if selection.is_oauth() {
            anstream::eprintln!("Re-authenticating `{mount_name}` over OAuth ...");
            crate::auth::login_with_workspace(
                workspace,
                mount_name,
                selection.account.as_deref(),
                self.no_browser,
                self.no_input,
                &self.scopes,
            )
            .await?
        } else {
            let source = TokenSource::resolve(
                self.token.as_deref(),
                self.token_env.as_deref(),
                !self.no_input,
            )?;
            let token = source.read()?;
            crate::commands::init::run_static_token_init(
                manifest,
                &selection,
                token,
                &paths.credentials_file,
                !self.no_validate,
            )
            .await?
        };
        self.reload_live_credentials(&target).await;

        anstream::eprintln!();
        anstream::eprintln!("✓ Re-authenticated `{mount_name}`.");
        crate::telemetry::maybe_print_health_nudge(workspace).await;
        Ok(())
    }

    async fn reload_live_credentials(&self, target: &CredentialTarget) {
        let client = crate::client::DaemonClient::new();
        for key in target.keys() {
            match client.reload_credential_if_ready(key).await {
                Ok(Some(_)) => {
                    anstream::eprintln!("✓ Reloaded `{key}` in the running daemon.");
                },
                Ok(None) => {},
                Err(error) => {
                    anstream::eprintln!(
                        "Credential `{key}` was stored, but live daemon reload failed: {error:#}"
                    );
                    anstream::eprintln!("Run `omnifs up` to restart with the new credential.");
                },
            }
        }
    }
}

pub async fn rm(
    workspace: &Workspace,
    name: &str,
    yes: bool,
    keep_credentials: bool,
) -> anyhow::Result<()> {
    let layout = workspace.layout();
    let mounts = workspace.mounts()?;
    let name =
        MountName::new(name.to_owned()).with_context(|| format!("invalid mount name `{name}`"))?;

    let mount = mounts.iter().find(|m| m.name == name).ok_or_else(|| {
        missing_mount_error(&layout.config_file, &mounts, name.as_str()).unwrap_err()
    })?;
    let config_path = mount.source.clone();
    let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(&layout.credentials_file));
    let service = CredentialService::new(Arc::clone(&store), OAuthClient::new()?);
    let credential_target = if keep_credentials {
        CredentialTarget::for_mount(&mount.config)
    } else {
        crate::auth::MountAuth::from_spec(workspace.catalog(), mount.config.clone())
            .register_revocation(&service)?
    };

    if !yes
        && !confirm(
            name.as_str(),
            &config_path,
            &credential_target,
            keep_credentials,
        )?
    {
        anstream::eprintln!("Aborted.");
        return Ok(());
    }

    delete_credentials(
        &service,
        &credential_target,
        keep_credentials,
        name.as_str(),
    )
    .await?;

    let daemon_delete = match workspace
        .daemon()
        .delete_mount_if_ready(name.as_str())
        .await
    {
        Ok(report) => report,
        Err(error) => {
            anstream::eprintln!("Running daemon could not remove mount `{name}`: {error:#}");
            anstream::eprintln!("Falling back to a local mount config delete.");
            None
        },
    };
    if daemon_delete.is_none() {
        Registry::load(&layout.mounts_dir)?.remove(&name)?;
    }
    anstream::eprintln!(
        "Removed mount `{name}` ({})",
        WorkspaceLayout::display(&config_path)
    );

    if let Some(report) = daemon_delete {
        if let Some(failure) = report.failure {
            anstream::eprintln!(
                "Mount config removed, but unloading it from the running daemon failed: {}",
                failure.reason
            );
        } else {
            anstream::eprintln!("✓ Unloaded from the running daemon");
        }
    }
    Ok(())
}

/// Show what the removal will delete, then ask for confirmation. Returns
/// whether the user chose to proceed; a declined prompt is a normal outcome,
/// not an error.
fn confirm(
    name: &str,
    config_path: &Path,
    target: &CredentialTarget,
    keep_credentials: bool,
) -> anyhow::Result<bool> {
    // Without a terminal there is no one to answer the prompt; fail fast naming
    // the skip flag instead of surfacing inquire's raw NotATTY error.
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "cannot confirm removal of `{name}` on a non-interactive stdin; pass -y to skip confirmation"
        );
    }
    anstream::eprintln!("Remove mount `{name}`? This will:");
    anstream::eprintln!("  • delete {}", WorkspaceLayout::display(config_path));
    match target {
        CredentialTarget::Internal(_) if !keep_credentials => {
            for key in target.keys() {
                anstream::eprintln!("  • delete the stored credential `{}`", key.storage_key());
            }
        },
        CredentialTarget::Internal(_) => {
            anstream::eprintln!("  • keep the stored credential (--keep-credentials)");
        },
        CredentialTarget::None => {},
    }
    inquire::Confirm::new("Proceed?")
        .with_default(false)
        .prompt()
        .map_err(crate::ui::from_inquire)
}

pub(crate) async fn delete_credentials(
    service: &CredentialService,
    target: &CredentialTarget,
    keep_credentials: bool,
    name: &str,
) -> anyhow::Result<()> {
    if keep_credentials {
        return Ok(());
    }
    // Credential delete happens before the mount-file delete so that on
    // store failure we don't orphan the mount config.
    for key in target.keys() {
        let outcome = service.revoke_and_delete(key).await;
        if let Some(error) = outcome.delete_error() {
            bail!("delete credential for mount `{name}`: {error}");
        }
        anstream::eprintln!("Credential `{}`: {outcome}", key.storage_key());
    }
    Ok(())
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
        delete_credentials(&service, &target, false, "github")
            .await
            .unwrap();

        assert!(store.get(&key).unwrap().is_none());
    }
}
