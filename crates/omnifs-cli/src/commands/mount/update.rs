//! Atomic edits to one committed mount spec.

use anyhow::{Context, anyhow};
use clap::{ArgGroup, Args};
use omnifs_core::MountName;
use omnifs_workspace::Workspace;
use omnifs_workspace::creds::CredentialStore as _;
use omnifs_workspace::mounts::{Limits, Spec};
use omnifs_workspace::provider::ProviderAuthManifest;
use serde::Serialize;

use crate::error::{ExitCode, WithExitCode as _};
use crate::stages::PromptMode;
use crate::token_source::TokenSource;
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug, Clone)]
// These booleans are independent CLI switches, not hidden state.
#[allow(clippy::struct_excessive_bools)]
#[command(
    group(
        ArgGroup::new("change")
            .required(true)
            .multiple(true)
            .args([
                "scheme",
                "no_auth",
                "config_json",
                "clear_config",
                "limits_json",
                "clear_limits",
            ])
    ),
    after_help = "Example:\n  omnifs mount update github --config-json '{\"owner\":\"0xff-ai\"}'"
)]
pub(crate) struct UpdateArgs {
    /// Existing mount name.
    pub(crate) name: String,
    /// Select a provider-declared authentication scheme.
    #[arg(long, conflicts_with = "no_auth")]
    pub(crate) scheme: Option<String>,
    /// Remove the mount's auth reference without deleting its credential.
    #[arg(long)]
    pub(crate) no_auth: bool,
    /// Replace the complete provider config object.
    #[arg(long, value_name = "JSON", conflicts_with = "clear_config")]
    pub(crate) config_json: Option<String>,
    /// Remove the provider config override.
    #[arg(long)]
    pub(crate) clear_config: bool,
    /// Replace the complete resource limits object.
    #[arg(long, value_name = "JSON", conflicts_with = "clear_limits")]
    pub(crate) limits_json: Option<String>,
    /// Remove all mount resource limits.
    #[arg(long)]
    pub(crate) clear_limits: bool,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub(crate) no_browser: bool,
    /// Read a static token from this source. Use `-` for stdin.
    #[arg(long, conflicts_with = "token_env")]
    pub(crate) token: Option<String>,
    /// Read a static token from this environment variable.
    #[arg(long, value_name = "ENV_VAR", conflicts_with = "token")]
    pub(crate) token_env: Option<String>,
    /// Store a static token without its upstream validation probe.
    #[arg(long)]
    pub(crate) no_validate: bool,
    /// OAuth scope to request. Repeat for multiple scopes.
    #[arg(long = "scope")]
    pub(crate) scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MountUpdateReceipt {
    verdict: crate::commands::receipt::Verdict,
    mount: String,
    previous_revision: String,
    revision: String,
    changed: Vec<&'static str>,
}

impl UpdateArgs {
    pub(crate) async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let name = MountName::new(self.name.clone())
            .with_context(|| format!("invalid mount name `{}`", self.name))?;
        let observation = workspace
            .desired_state()
            .observe_mount(&name)?
            .ok_or_else(|| anyhow!("no committed mount named `{name}`"))?;
        let previous_revision = observation.revision().clone();
        let mut candidate = observation.spec().clone();
        let manifest =
            omnifs_workspace::mounts::pinned_manifest(workspace.catalog(), observation.spec())?
                .ok_or_else(|| {
                    anyhow!(
                        "provider artifact `{}` for mount `{name}` is missing",
                        observation.spec().provider.id
                    )
                })?;
        let mut changed = Vec::new();

        if self.no_auth {
            set_if_changed(&mut candidate.auth, None, "auth", &mut changed);
        } else if let Some(scheme) = self.scheme.as_deref() {
            let account = candidate
                .auth
                .as_ref()
                .and_then(omnifs_workspace::mounts::Auth::account)
                .map(str::to_owned);
            let auth_manifest = manifest
                .auth
                .as_ref()
                .map(ProviderAuthManifest::wasm_auth_manifest);
            let auth = crate::auth::auth_from_scheme(auth_manifest.as_ref(), scheme, account)?;
            set_if_changed(&mut candidate.auth, Some(auth), "auth", &mut changed);
        }

        if let Some(raw) = self.config_json.as_deref() {
            let config: serde_json::Value =
                serde_json::from_str(raw).context("parse --config-json")?;
            crate::commands::mount::spec_creation::validate_config(&manifest, &config)?;
            set_if_changed(
                &mut candidate.config_raw,
                Some(config),
                "config",
                &mut changed,
            );
        } else if self.clear_config {
            if let Some(metadata) = manifest.config.as_ref() {
                crate::commands::mount::spec_creation::validate_config(
                    &manifest,
                    &metadata.defaults(),
                )?;
            }
            set_if_changed(&mut candidate.config_raw, None, "config", &mut changed);
        }

        if let Some(raw) = self.limits_json.as_deref() {
            let limits: Limits = serde_json::from_str(raw).context("parse --limits-json")?;
            set_if_changed(&mut candidate.limits, Some(limits), "limits", &mut changed);
        } else if self.clear_limits {
            set_if_changed(&mut candidate.limits, None, "limits", &mut changed);
        }

        if changed.contains(&"auth") && candidate.auth.is_some() {
            self.ensure_credential(&workspace, &candidate, &manifest, &output)
                .await?;
        }

        let revision = workspace
            .desired_state()
            .replace_mount(&observation, &candidate)
            .map_err(|error| {
                if changed.contains(&"auth") {
                    anyhow!(
                        "{error}; a credential acquired before the failed commit may remain stored"
                    )
                } else {
                    anyhow!(error)
                }
            })?;
        let receipt = MountUpdateReceipt {
            verdict: crate::commands::receipt::Verdict::Ok,
            mount: name.to_string(),
            previous_revision: previous_revision.to_string(),
            revision: revision.to_string(),
            changed,
        };
        if output.is_structured() {
            output.emit_result(ResultVerdict::Ok, &receipt)?;
        } else if receipt.changed.is_empty() {
            output.outro(format!(
                "Mount `{name}` already matches revision {}.",
                short_revision(&revision)
            ));
        } else {
            output.outro(format!(
                "Updated `{name}` at revision {}. Apply it: `omnifs up`",
                short_revision(&revision)
            ));
        }
        Ok(ExitCode::Success)
    }

    async fn ensure_credential(
        &self,
        workspace: &Workspace,
        candidate: &Spec,
        manifest: &omnifs_workspace::provider::ProviderManifest,
        output: &Output,
    ) -> anyhow::Result<()> {
        let auth_view = crate::auth::MountAuth::from_spec(workspace.catalog(), candidate.clone());
        let Some(target) = auth_view.credential_id()? else {
            return Ok(());
        };
        if workspace.credentials().get(&target)?.is_some() {
            return Ok(());
        }
        let auth = candidate.auth.as_ref().expect("credential target has auth");
        let prompt = PromptMode::from_flags(output.yes(), output.no_input());
        if auth.is_oauth() {
            if !prompt.interactive {
                return Err(anyhow!(
                    "scheme `{}` needs OAuth sign-in; rerun this command in a terminal",
                    auth.scheme().unwrap_or("oauth")
                ))
                .with_exit_code(ExitCode::AuthRequired);
            }
            crate::auth::login::login(
                workspace.catalog(),
                auth_view,
                workspace.credentials(),
                auth.account(),
                crate::auth::LoginInteractivity {
                    no_browser: self.no_browser,
                    no_input: prompt.no_input,
                    scopes: &self.scopes,
                },
                output,
                crate::auth::auth_receipt_key_width(),
            )
            .await?;
        } else {
            let source = TokenSource::resolve(
                self.token.as_deref(),
                self.token_env.as_deref(),
                prompt.interactive,
            )?;
            let token = source.read(output)?;
            crate::commands::mount::run_static_token_init(
                manifest,
                auth,
                token,
                workspace.credentials(),
                !self.no_validate,
                output,
                crate::auth::auth_receipt_key_width(),
            )
            .await?;
        }
        Ok(())
    }
}

fn set_if_changed<T: PartialEq>(
    target: &mut T,
    value: T,
    field: &'static str,
    changed: &mut Vec<&'static str>,
) {
    if target != &value {
        *target = value;
        changed.push(field);
    }
}

fn short_revision(revision: &omnifs_workspace::mounts::Revision) -> &str {
    &revision.as_str()[..revision.as_str().len().min(8)]
}
