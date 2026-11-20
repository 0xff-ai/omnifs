//! Atomic edits to one daemon-owned mount.

use crate::auth::Auth;
use anyhow::{Context, anyhow};
use clap::{ArgGroup, Args};
use omnifs_api::{
    CredentialKey, CredentialMaterial, CredentialSubmission, MountField, MountLimits, MountPatch,
    MutationOpResult,
};
use omnifs_core::MountName;
use omnifs_provider::ProviderManifest;
use secrecy::ExposeSecret as _;
use serde::Serialize;

use crate::client_state::ClientState;
use crate::error::{ExitCode, WithExitCode as _};
use crate::mutation::PlannedOp;
use crate::token_source::TokenSource;
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
#[command(
    group(
        ArgGroup::new("change")
            .required(true)
            .multiple(true)
            .args(["provider", "scheme", "no_auth", "config_json", "clear_config", "limits_json", "clear_limits"])
    ),
    after_help = "Examples:\n  omnifs mount update github --config-json '{\"owner\":\"0xff-ai\"}'\n  omnifs mount update github --provider github"
)]
pub(crate) struct UpdateArgs {
    pub(crate) name: String,
    /// The provider of the mount, by a path, a digest, or the name of an
    /// embedded provider, as `omnifs mount add` takes it.
    #[arg(long)]
    pub(crate) provider: Option<String>,
    #[arg(long, conflicts_with = "no_auth")]
    pub(crate) scheme: Option<String>,
    #[arg(long)]
    pub(crate) no_auth: bool,
    #[arg(long, value_name = "JSON", conflicts_with = "clear_config")]
    pub(crate) config_json: Option<String>,
    #[arg(long)]
    pub(crate) clear_config: bool,
    #[arg(long, value_name = "JSON", conflicts_with = "clear_limits")]
    pub(crate) limits_json: Option<String>,
    #[arg(long)]
    pub(crate) clear_limits: bool,
    #[arg(long)]
    pub(crate) no_browser: bool,
    #[arg(long, conflicts_with = "token_env")]
    pub(crate) token: Option<String>,
    #[arg(long, value_name = "ENV_VAR", conflicts_with = "token")]
    pub(crate) token_env: Option<String>,
    #[arg(long)]
    pub(crate) no_validate: bool,
    #[arg(long = "scope")]
    pub(crate) scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MountUpdateReceipt {
    verdict: ResultVerdict,
    mount: String,
    previous_revision: u64,
    revision: u64,
    changed: Vec<&'static str>,
}

impl UpdateArgs {
    #[allow(clippy::too_many_lines)] // one linear patch assembly and mutation flow
    pub(crate) async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        crate::commands::daemon_start::start(&output).await?;
        let rpc = crate::rpc::RpcClient::resolve()?;
        let name = MountName::new(self.name.clone())
            .with_context(|| format!("invalid mount name `{}`", self.name))?;
        let state = ClientState::resolve()?;
        let current = rpc
            .get_mount(name.clone())
            .await?
            .ok_or_else(|| anyhow!("no mount named `{name}`"))?;
        let (provider, manifest) = if let Some(selector) = self.provider.as_deref() {
            let embedded = rpc.list_embedded_providers().await?;
            let resolved = crate::provider_resolver::ProviderResolver::new(&rpc)
                .resolve(selector, &embedded)
                .await?;
            (resolved.reference.id, resolved.manifest)
        } else {
            let metadata = rpc
                .provider_metadata(current.definition.provider)
                .await?
                .ok_or_else(|| anyhow!("provider metadata is unavailable for `{name}`"))?;
            let manifest = ProviderManifest::from_bytes(&metadata.manifest)
                .context("parse daemon provider metadata")?;
            (current.definition.provider, manifest)
        };
        let (patch, changed) = self.build_patch(&current, provider, &manifest)?;

        if changed.is_empty() {
            // No field differed from the current mount, so no mutation was
            // even attempted: there is no outcome here for a verdict to be
            // derived from, only the fact that nothing needed to change.
            let receipt = MountUpdateReceipt {
                verdict: ResultVerdict::Ok,
                mount: name.to_string(),
                previous_revision: current.revision.get(),
                revision: current.revision.get(),
                changed,
            };
            if output.is_structured() {
                output.emit_result(ResultVerdict::Ok, &receipt)?;
            } else {
                output.outro(format!(
                    "Mount `{name}` already matches revision {}.",
                    current.revision.get()
                ));
            }
            return Ok(ExitCode::Success);
        }

        // Credential collection is interactive (an OAuth browser handoff or a
        // token prompt), so it happens before any lease is acquired: neither
        // may run under the daemon's 30s mutation lease.
        let submission = if let MountField::Set(auth) = &patch.auth {
            let auth_manifest = manifest
                .auth
                .as_ref()
                .map(omnifs_provider::ProviderAuthManifest::wasm_auth_manifest);
            let selected = crate::auth::Auth::from_scheme(
                auth_manifest.as_ref(),
                &auth.scheme,
                Some(auth.account_label.clone()),
            )?;
            Some(
                self.collect_submission(provider, &manifest, &selected, auth, &output)
                    .await?,
            )
        } else {
            None
        };

        let provider_name = current.provider.name.clone();
        let rpc_ref = &rpc;
        let manifest_ref = &manifest;
        let this = &self;
        let outcome = crate::mutation::run(rpc_ref, &state, &output, move || async move {
            // Read the mount again under the lease: the authoritative patch
            // is built from this fresh state, closing the gap between the
            // pre-lease read above and the batch actually applying.
            let fresh = rpc_ref
                .get_mount(name.clone())
                .await?
                .ok_or_else(|| anyhow!("no mount named `{name}`"))?;
            let (patch, changed) = this.build_patch(&fresh, provider, manifest_ref)?;
            if changed.is_empty() {
                return Ok(Vec::new());
            }
            let mut ops = Vec::with_capacity(2);
            if let Some(submission) = submission {
                let key = CredentialKey {
                    provider_name,
                    scheme: submission.scheme.clone(),
                    account_label: submission.account_label.clone(),
                };
                ops.push(PlannedOp::submit_credential(&key, submission));
            }
            ops.push(PlannedOp::mount_update(name, patch));
            Ok(ops)
        })
        .await?;

        let Some(outcome) = outcome else {
            output.outro(format!(
                "Mount `{}` already matches the requested state.",
                self.name
            ));
            let receipt = MountUpdateReceipt {
                verdict: ResultVerdict::Ok,
                mount: self.name.clone(),
                previous_revision: current.revision.get(),
                revision: current.revision.get(),
                changed: Vec::new(),
            };
            if output.is_structured() {
                output.emit_result(ResultVerdict::Ok, &receipt)?;
            }
            return Ok(ExitCode::Success);
        };
        crate::mutation::narrate_serving(&output, &outcome.serving);
        let revision = outcome
            .results
            .into_iter()
            .find_map(|result| match result {
                MutationOpResult::Mount(mount) => Some(mount.revision),
                MutationOpResult::Credential(_) => None,
            })
            .context("mount update batch did not include a mount result")?;
        emit_receipt(
            &output,
            &self.name,
            current.revision.get(),
            revision.get(),
            changed,
        )
    }

    /// Build the patch and the list of changed field names from a mount
    /// record, with `provider` and its manifest: pure so the pre-lease and
    /// under-lease reads share the exact same diffing logic. The artifact of
    /// the mount itself changes nothing.
    fn build_patch(
        &self,
        current: &omnifs_api::MountRecord,
        provider: omnifs_core::ProviderId,
        manifest: &ProviderManifest,
    ) -> anyhow::Result<(MountPatch, Vec<&'static str>)> {
        let mut changed = Vec::new();
        let mut patch = MountPatch::default();

        if provider != current.definition.provider {
            patch.provider = Some(provider);
            changed.push("provider");
        }

        if self.no_auth {
            if current.definition.auth.is_some() {
                patch.auth = MountField::Clear;
                changed.push("auth");
            }
        } else if let Some(scheme) = self.scheme.as_deref() {
            let account = current
                .definition
                .auth
                .as_ref()
                .map_or_else(|| "default".to_owned(), |auth| auth.account_label.clone());
            let next = omnifs_api::MountCredential {
                scheme: scheme.to_owned(),
                account_label: account,
            };
            if current.definition.auth.as_ref() != Some(&next) {
                patch.auth = MountField::Set(next);
                changed.push("auth");
            }
        }

        if let Some(raw) = self.config_json.as_deref() {
            let value: serde_json::Value =
                serde_json::from_str(raw).context("parse --config-json")?;
            crate::commands::mount::spec_creation::validate_config(manifest, &value)?;
            let bytes = serde_json::to_vec(&value)?;
            if current.definition.config != bytes {
                patch.config = MountField::Set(bytes);
                changed.push("config");
            }
        } else if self.clear_config && !current.definition.config.is_empty() {
            patch.config = MountField::Clear;
            changed.push("config");
        }

        if let Some(raw) = self.limits_json.as_deref() {
            let limits: MountLimits = serde_json::from_str(raw).context("parse --limits-json")?;
            if current.definition.limits.as_ref() != Some(&limits) {
                patch.limits = MountField::Set(limits);
                changed.push("limits");
            }
        } else if self.clear_limits && current.definition.limits.is_some() {
            patch.limits = MountField::Clear;
            changed.push("limits");
        }

        Ok((patch, changed))
    }

    async fn collect_submission(
        &self,
        provider: omnifs_core::ProviderId,
        manifest: &ProviderManifest,
        auth: &Auth,
        credential: &omnifs_api::MountCredential,
        output: &Output,
    ) -> anyhow::Result<CredentialSubmission> {
        let prompt = output.prompt_mode();
        if auth.is_oauth() {
            if !prompt.interactive() {
                return Err(anyhow!(
                    "scheme `{}` needs OAuth sign-in; rerun this command in a terminal",
                    credential.scheme
                ))
                .with_exit_code(ExitCode::AuthRequired);
            }
            return crate::auth::login::login_for_submission(
                provider,
                manifest,
                auth,
                &credential.account_label,
                crate::auth::LoginInteractivity {
                    no_browser: self.no_browser,
                    no_input: prompt.no_input(),
                    scopes: (!self.scopes.is_empty()).then_some(self.scopes.as_slice()),
                },
                output,
                crate::auth::auth_receipt_key_width(),
            )
            .await;
        }
        let source = TokenSource::resolve(
            self.token.as_deref(),
            self.token_env.as_deref(),
            prompt.interactive(),
        )?;
        let token = source.read(output)?;
        if !self.no_validate {
            let scheme = auth.static_token_scheme(manifest)?;
            if let Some(validation) = scheme.validation.as_ref() {
                super::token_validation::validate_static_token(
                    validation,
                    scheme.header_name.as_deref().unwrap_or("Authorization"),
                    &scheme.value_prefix,
                    token.expose_secret(),
                    output,
                )
                .await?;
            }
        }
        Ok(CredentialSubmission {
            provider,
            scheme: credential.scheme.clone(),
            account_label: credential.account_label.clone(),
            material: CredentialMaterial::StaticToken {
                token: omnifs_api::SecretBytes::new(token.expose_secret().as_bytes().to_vec()),
            },
            overrides: omnifs_api::CredentialClientOverrides {
                client_id: None,
                client_secret: None,
                redirect_uri: None,
                scopes: None,
            },
        })
    }
}

fn emit_receipt(
    output: &Output,
    name: &str,
    previous_revision: u64,
    revision: u64,
    changed: Vec<&'static str>,
) -> anyhow::Result<ExitCode> {
    // Reaching this point already proves the daemon committed the patch:
    // `run`'s only caller propagates a failed mutation via `?` before
    // `emit_receipt` is ever called, so there is no outcome here for a
    // verdict to be derived from.
    let result = MountUpdateReceipt {
        verdict: ResultVerdict::Ok,
        mount: name.to_owned(),
        previous_revision,
        revision,
        changed,
    };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, &result)?;
    } else {
        output.outro(format!("Updated `{name}` at revision {revision}."));
    }
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::UpdateArgs;
    use omnifs_core::{MountName, MountRevision, MountVersion, MutationId, ProviderId};
    use omnifs_provider::ProviderManifest;

    fn args(provider: &str) -> UpdateArgs {
        UpdateArgs {
            name: "github".to_owned(),
            provider: Some(provider.to_owned()),
            scheme: None,
            no_auth: false,
            config_json: None,
            clear_config: false,
            limits_json: None,
            clear_limits: false,
            no_browser: false,
            token: None,
            token_env: None,
            no_validate: false,
            scopes: Vec::new(),
        }
    }

    fn mount(provider: ProviderId) -> omnifs_api::MountRecord {
        omnifs_api::MountRecord {
            definition: omnifs_api::MountDefinition {
                name: MountName::new("github").unwrap(),
                provider,
                auth: None,
                limits: None,
                config: Vec::new(),
            },
            provider: omnifs_api::ProviderReference {
                id: provider,
                name: "github".to_owned(),
                version: None,
            },
            version: MountVersion::from_digest([1; 32]),
            revision: MountRevision::new(1),
            health: omnifs_api::MountHealth::Active,
            auth_health: None,
            last_mutation_id: MutationId::from_bytes([1; 16]),
        }
    }

    fn manifest() -> ProviderManifest {
        ProviderManifest::from_bytes(
            br#"{
                "id": "github",
                "displayName": "GitHub",
                "provider": "github.wasm",
                "defaultMount": "github",
                "projection": 1,
                "refreshIntervalSecs": 0
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn the_artifact_of_the_mount_itself_changes_nothing() {
        let current = ProviderId::from_wasm_bytes(b"current");
        let (patch, changed) = args("github")
            .build_patch(&mount(current), current, &manifest())
            .unwrap();
        assert!(changed.is_empty());
        assert_eq!(patch.provider, None);

        let another = ProviderId::from_wasm_bytes(b"another");
        let (patch, changed) = args("github")
            .build_patch(&mount(current), another, &manifest())
            .unwrap();
        assert_eq!(changed, ["provider"]);
        assert_eq!(patch.provider, Some(another));
    }
}
