//! Mount-creation orchestration shared by `mount add` and `setup`.
//!
//! Commands own narration. This module owns the stage behavior so mount
//! creation and authentication stay in one path.

use crate::auth::Auth;
use anyhow::{Context, anyhow};
use omnifs_api::{
    CredentialKey, CredentialSubmission, MountCredential, MountDefinition, MountLimits,
    MutationOpResult,
};
use omnifs_core::{MountName, MountRevision, ProviderId};
use omnifs_provider::{ProviderAuthManifest, ProviderManifest};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::spec_creation::create_config;
use super::{AddArgs, AuthImportDecision, ImportOutcome, provider_selection};
use crate::client_state::ClientState;
use crate::error::{ExitCode, WithExitCode};
use crate::mutation::PlannedOp;
use crate::provider_resolver::ProviderResolver;
use crate::rpc::RpcClient;
use crate::token_source::TokenSource;
use crate::ui::output::PromptMode;

pub(crate) struct MountInitOutcome {
    pub(crate) mount_name: String,
    pub(crate) status: MountInitStatus,
    pub(crate) revision: Option<MountRevision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MountInitStatus {
    /// The mount is authenticated and ready to serve.
    Ready,
    /// Authentication was declined; no mount was created.
    SignInDeclined,
}

impl MountInitStatus {
    /// `SignInDeclined` means no mount was created, so a receipt reporting
    /// it must never headline `ok`: a consumer would have to cross-check
    /// `status` to learn that nothing happened.
    pub(crate) const fn verdict(self) -> crate::ui::output::ResultVerdict {
        match self {
            Self::Ready => crate::ui::output::ResultVerdict::Ok,
            Self::SignInDeclined => crate::ui::output::ResultVerdict::Degraded,
        }
    }
}

pub(crate) struct MountBuild {
    provider: ProviderId,
    mount_name: MountName,
    manifest: ProviderManifest,
    imported_token: Option<secrecy::SecretString>,
    auth: Option<Auth>,
    limits: Option<MountLimits>,
    config: Vec<u8>,
    credential_ready: bool,
    // Credential material `authenticate` collected but has not yet been sent
    // to the daemon. `configure_mount` folds it into the same batch as the
    // mount-create op, so a fresh sign-in and a new mount always commit or
    // fail together.
    pending_submission: Option<CredentialSubmission>,
}

/// Keys `configure_mount`'s block may ever print, across every branch: a
/// block's key column is sized to the whole block, computed once up
/// front, even though rows settle one at a time as each stage of mount
/// creation completes). `mount name` only fires on an interactive/`--yes`
/// name collision ([`crate::commands::mount::provider_selection`]); the
/// sign-in branch prints exactly one of `sign in` (declined) or the shared
/// [`crate::auth::AUTH_RECEIPT_KEYS`] (`oauth`/`signed in`/`credential`,
/// completed) depending on the auth path taken. A key that never actually
/// fires still counts toward the width, so the block stays aligned
/// regardless of which branch runs.
const MOUNT_ADD_KEYS: [&str; 4] = ["mount name", "provider", "mount", "sign in"];

pub(crate) fn mount_add_key_width() -> usize {
    crate::ui::render::key_field_width(&MOUNT_ADD_KEYS).max(crate::auth::auth_receipt_key_width())
}

#[allow(clippy::too_many_lines)] // linear narration reads best inline
pub(crate) async fn configure_mount(
    args: AddArgs,
    output: &crate::ui::output::Output,
    prompt: PromptMode,
) -> anyhow::Result<MountInitOutcome> {
    crate::commands::daemon_start::start().await?;
    let rpc = RpcClient::resolve()?;
    let state = ClientState::resolve()?;
    let mut plan = assemble_mount_build(&args, &rpc, output, prompt).await?;
    if plan.authenticate(&args, output, prompt).await? == MountInitStatus::SignInDeclined {
        return Ok(MountInitOutcome {
            mount_name: plan.mount_name.to_string(),
            status: MountInitStatus::SignInDeclined,
            revision: None,
        });
    }

    let definition = MountDefinition {
        name: plan.mount_name.clone(),
        provider: plan.provider,
        auth: plan.auth.as_ref().map(|auth| MountCredential {
            scheme: auth.scheme().unwrap_or_default().to_owned(),
            account_label: auth.account_or_default().to_owned(),
        }),
        limits: plan.limits.clone(),
        config: plan.config.clone(),
    };
    let mut ops = Vec::with_capacity(2);
    if let Some(submission) = plan.pending_submission.take() {
        let key = CredentialKey {
            provider_name: plan.manifest.id.clone(),
            scheme: submission.scheme.clone(),
            account_label: submission.account_label.clone(),
        };
        ops.push(PlannedOp::submit_credential(&key, submission));
    }
    ops.push(PlannedOp::mount_create(definition));

    let outcome = crate::mutation::run(&rpc, &state, output, || async move { Ok(ops) })
        .await?
        .context("mount creation produced no result")?;
    crate::mutation::narrate_serving(output, &outcome.serving);
    let revision = outcome
        .results
        .into_iter()
        .find_map(|result| match result {
            MutationOpResult::Mount(mount) => Some(mount.revision),
            MutationOpResult::Credential(_) => None,
        })
        .context("mount creation batch did not include a mount result")?;

    output.ledger_row(
        &crate::ui::render::LedgerRow::new(
            crate::ui::style::Glyph::Done,
            "mount",
            mount_created_value(&plan.mount_name),
        ),
        mount_add_key_width(),
    );
    Ok(MountInitOutcome {
        mount_name: plan.mount_name.to_string(),
        status: MountInitStatus::Ready,
        revision: Some(revision),
    })
}

/// Init is interactive only with real stdin and stderr terminals and without
/// `--no-input`. A piped stdin is non-interactive even without the flag, so
/// prompt sites bail cleanly (naming the satisfying flags) instead of hitting
/// a prompt library's raw "not a terminal" error.
fn init_interactive(prompt: PromptMode) -> bool {
    prompt.interactive()
}

#[allow(clippy::too_many_lines)] // one linear spec-assembly path
pub(crate) async fn assemble_mount_build(
    args: &AddArgs,
    rpc: &RpcClient,
    output: &crate::ui::output::Output,
    prompt: PromptMode,
) -> anyhow::Result<MountBuild> {
    let interactive = init_interactive(prompt);
    let embedded = rpc.list_embedded_providers().await?;

    // No provider argument in an interactive output: choose one with the
    // generic single-select prompt instead of a bare list. Rows render the
    // same three aligned columns (name, description, auth label) as `omnifs
    // setup`'s provider catalog, with a dim `mounted` marker on an
    // already-configured provider rather than hiding it: a second mount of
    // the same provider under a different name is legitimate. The detail
    // panel carries the full, untruncated consent facts: domains called,
    // memory ceiling, and auth scheme, one sentence per line, never the
    // compact truncated summary `mount add`'s later consent block uses.
    let picked = if args.provider.is_none() && interactive {
        let mounted: std::collections::BTreeSet<String> = rpc
            .list_mounts()
            .await?
            .iter()
            .map(|mount| mount.provider.name.clone())
            .collect();
        let options = crate::provider_resolver::provider_options(&embedded, &mounted);
        // `provider_options` only ever returns an option whose manifest bytes
        // already parsed once; re-parsing those same bytes here is a pure
        // function of them and cannot fail differently, so `manifests` stays
        // index-aligned with `options`.
        let manifests: Vec<ProviderManifest> = options
            .iter()
            .filter_map(|option| {
                embedded
                    .iter()
                    .find(|entry| entry.reference.name == option.name)
                    .and_then(|entry| ProviderManifest::from_bytes(&entry.manifest).ok())
            })
            .collect();
        let rows: Vec<_> = manifests
            .iter()
            .map(crate::provider_catalog::provider_catalog_row)
            .collect();
        let aligned = crate::provider_catalog::align_provider_catalog_rows(&rows);
        let choices = options.into_iter().zip(manifests.iter()).zip(aligned).map(
            |((option, manifest), mut label)| {
                if option.mounted {
                    label.push_str("  ");
                    label.push_str(&crate::ui::style::dim(
                        "mounted",
                        crate::ui::style::Stream::Stderr,
                    ));
                }
                let detail = crate::capability::consent_detail(manifest);
                (option.name, label, detail)
            },
        );
        Some(
            crate::ui::prompt::Select::new("Which provider?")
                .detailed_options(choices)
                .ask_with_output(output)?,
        )
    } else {
        None
    };
    let selector = provider_selection::select(
        &embedded,
        args.provider.as_deref().or(picked.as_deref()),
        interactive,
        output,
    )?;
    let resolved = ProviderResolver::new(rpc).resolve(&selector).await?;
    let provider_name = resolved.reference.meta.name.to_string();
    let mounts = rpc.list_mounts().await?;
    let mount_name = provider_selection::mount_name(
        &mounts,
        &resolved.manifest.default_mount,
        args.name.as_deref(),
        interactive,
        prompt.yes(),
        output,
        mount_add_key_width(),
    )?;
    // An explicit `--name` is returned as-is by `provider_selection::mount_name`
    // (only the auto-generated default name goes through unique-name
    // disambiguation), so a collision on an explicit name must be caught here,
    // before authentication spends an interactive OAuth round trip on a mount
    // that cannot be created. The atomic batch below still catches a same-name
    // race that lands between this read and `ApplyMutation`.
    if args.name.is_some()
        && mounts
            .iter()
            .any(|mount| mount.definition.name == mount_name)
    {
        anyhow::bail!("mount `{mount_name}` already exists");
    }
    let reference = resolved.reference;
    let provider_id = reference.id;
    let manifest = resolved.manifest;
    let auth_manifest = manifest
        .auth
        .as_ref()
        .map(ProviderAuthManifest::wasm_auth_manifest);
    let default_auth = args.selected_auth(&manifest, auth_manifest.as_ref())?;

    // Receipt rows for the two facts already true at this point: the
    // provider artifact is retained in the store (`ProviderResolver::resolve`
    // above either found it there or just retained it), and the mount name is
    // validated and free. The remaining work below (auth, then the actual
    // spec write in `persist_mount_spec`) either fills in these two rows'
    // consequences or fails outright, so nothing here overclaims.
    let key_width = mount_add_key_width();
    let provider_identity = reference.meta.version.as_ref().map_or_else(
        || provider_name.clone(),
        |version| format!("{provider_name}@{version}"),
    );
    output.ledger_row(
        &crate::ui::render::LedgerRow::new(
            crate::ui::style::Glyph::Done,
            "provider",
            format!("{provider_identity} retained"),
        ),
        key_width,
    );

    // An ambient credential (imported under --yes or on the interactive
    // prompt) promotes an OAuth default to a static token, which lets a
    // `--no-input` run of an OAuth-default provider complete headlessly. The
    // OAuth bail only fires when nothing was imported.
    let import_outcome = AuthImportDecision::new(
        default_auth,
        auth_manifest.as_ref(),
        &provider_name,
        interactive,
        prompt.yes(),
    )
    .resolve(output, mount_add_key_width())?;
    let ImportOutcome { auth, token } = import_outcome;

    if !interactive && token.is_none() && auth.as_ref().is_some_and(Auth::is_oauth) {
        return Err(anyhow!(
            "cannot complete OAuth for `{provider_name}` without an interactive terminal; pass --token-env VAR with --scheme <static-token-scheme>, pass --no-auth, or run interactively"
        ))
        .with_exit_code(ExitCode::AuthRequired);
    }

    if !interactive && manifest.requires_mount_input() && args.config_json.is_none() {
        anyhow::bail!(
            "cannot complete provider config prompts for `{provider_name}` without an interactive terminal; pass --config-json <json>"
        );
    }
    // A supplied --config-json owns the whole config: skip default generation
    // (which validates manifest defaults and fails on required fields the
    // override provides) and validate the override where it is applied.
    let config_raw = if args.config_json.is_some() {
        None
    } else {
        create_config(&manifest, output, interactive)?
    };
    let mut auth = auth;
    if let Some(Auth::OAuth(oauth)) = auth.as_mut()
        && !args.scopes.is_empty()
    {
        oauth.scopes = Some(args.scopes.clone());
    }
    let mut limits = manifest_limits(&manifest);
    if let Some(raw) = args.limits_json.as_deref() {
        limits = Some(parse_json_flag("--limits-json", raw)?);
    }
    let mut config = config_bytes(config_raw)?;
    args.apply_mount_overrides(&manifest, &mut config)?;

    let auth_ready = if token.is_none() {
        if let Some(auth) = auth.as_ref() {
            let account = auth.account_or_default();
            let key = CredentialKey {
                provider_name: manifest.id.clone(),
                scheme: auth.scheme().unwrap_or_default().to_owned(),
                account_label: account.to_owned(),
            };
            rpc.credential_status(key).await?.is_some_and(|status| {
                status.provider == provider_id
                    && matches!(status.status, omnifs_api::CredentialStatusKind::Active)
            })
        } else {
            false
        }
    } else {
        false
    };

    Ok(MountBuild {
        provider: provider_id,
        mount_name,
        manifest,
        imported_token: token,
        auth,
        limits,
        config,
        credential_ready: auth_ready,
        pending_submission: None,
    })
}

impl MountBuild {
    /// Collect fresh credential material before any mutation begins: the
    /// OAuth browser handoff and static-token collection are both
    /// interactive, so neither may run under the daemon's 30s mutation
    /// lease. Sets `pending_submission` for `configure_mount` to fold into
    /// the same batch as the mount-create op; leaves it `None` when no new
    /// material was needed (no auth, or an existing credential is already
    /// active).
    async fn authenticate(
        &mut self,
        args: &AddArgs,
        output: &crate::ui::output::Output,
        prompt: PromptMode,
    ) -> anyhow::Result<MountInitStatus> {
        super::render_consent_block(output, &self.manifest);
        let plan = self;
        let Some(auth) = plan.auth.as_ref() else {
            return Ok(MountInitStatus::Ready);
        };
        if plan.credential_ready {
            return Ok(MountInitStatus::Ready);
        }
        let interactive = init_interactive(prompt);
        let key_width = mount_add_key_width();
        let submission = if let Some(token) = plan.imported_token.take() {
            super::run_static_token_init(
                plan.provider,
                &plan.manifest,
                auth,
                token,
                !args.no_validate,
                output,
            )
            .await?
        } else if auth.is_oauth() {
            // Gate the browser handoff when interactive: a decline is a clean skip,
            // not a failure.
            if interactive && !prompt.yes() {
                let proceed = crate::ui::prompt::Confirm::new(format!(
                    "Sign in to {} in your browser now?",
                    plan.manifest.display_name
                ))
                .with_default(true)
                .ask_with_output(output)?;
                if !proceed {
                    return Ok(MountInitStatus::SignInDeclined);
                }
            }
            let account = auth.account_or_default();
            crate::auth::login::login_for_submission(
                plan.provider,
                &plan.manifest,
                auth,
                account,
                crate::auth::LoginInteractivity {
                    no_browser: args.no_browser,
                    no_input: prompt.no_input(),
                    scopes: (!args.scopes.is_empty()).then_some(args.scopes.as_slice()),
                },
                output,
                key_width,
            )
            .await
            .inspect_err(|_| {
                output.narrate(sign_in_failed_value(&plan.mount_name));
            })?
        } else {
            if interactive && let Ok(scheme) = auth.static_token_scheme(&plan.manifest) {
                let guidance = plan
                    .manifest
                    .auth
                    .as_ref()
                    .map(|auth| auth.guidance_for(&scheme.key))
                    .unwrap_or_default();
                // Dim sentences: informational setup guidance the
                // user reads once before pasting a token, not a settled fact.
                let dim =
                    |text: String| crate::ui::style::dim(text, crate::ui::style::Stream::Stderr);
                if let Some(url) = &scheme.creation_url {
                    output.narrate(dim(format!("create a token at {url}")));
                }
                for step in &guidance.setup_steps {
                    output.narrate(dim(step.clone()));
                }
                if let Some(url) = &guidance.docs_url {
                    output.narrate(dim(url.clone()));
                }
            }
            let source = TokenSource::resolve(
                args.token.as_deref(),
                args.token_env.as_deref(),
                interactive,
            )?;
            let token = source.read(output)?;
            super::run_static_token_init(
                plan.provider,
                &plan.manifest,
                auth,
                token,
                !args.no_validate,
                output,
            )
            .await?
        };
        plan.pending_submission = Some(submission);
        Ok(MountInitStatus::Ready)
    }
}

/// The `mount` receipt row's value: `/<name> created`. Pure so the exact
/// wording is testable without a profile.
fn mount_created_value(mount_name: &MountName) -> String {
    format!("/{mount_name} created")
}

/// The OAuth sign-in failure note: unlike a decline, an actual
/// login error propagates before `CreateMount`, so no mount exists yet.
fn sign_in_failed_value(mount_name: &MountName) -> String {
    format!("sign-in did not complete; mount `{mount_name}` was not created")
}

pub(super) fn parse_json_flag<T: DeserializeOwned>(
    flag: &'static str,
    raw: &str,
) -> anyhow::Result<T> {
    serde_json::from_str(raw).with_context(|| format!("parse {flag}"))
}

/// The manifest's declared resource limits translated into the wire
/// `MountLimits` shape, or `None` when the provider declares none. Shared by
/// `mount add`'s default path and setup's quick-start path
/// ([`quick_start_definition`]) so both express a provider's limits
/// identically before any `--limits-json` override.
fn manifest_limits(manifest: &ProviderManifest) -> Option<MountLimits> {
    (!manifest.limits.is_empty()).then(|| MountLimits {
        max_memory_mb: manifest
            .limits
            .max_memory_mb
            .as_ref()
            .map(|limit| limit.value),
        max_fetch_blob_bytes: manifest
            .limits
            .max_fetch_blob_bytes
            .as_ref()
            .map(|limit| limit.value),
    })
}

/// Serialize a generated config to the wire bytes a mount definition carries,
/// falling back to an empty object when the provider generated none. Shared
/// by `mount add`'s default path and [`quick_start_definition`].
fn config_bytes(config: Option<Value>) -> anyhow::Result<Vec<u8>> {
    Ok(match config {
        Some(config) => serde_json::to_vec(&config).context("encode provider config")?,
        None => b"{}".to_vec(),
    })
}

/// Build a ready-to-submit `MountDefinition` for a provider that needs no
/// sign-in and no interactive config input: `omnifs setup`'s quick-start
/// mount batch. Shares config and limits derivation with `mount add`'s
/// default path ([`manifest_limits`], [`config_bytes`]) so both commands
/// write the same defaults for the same provider; unlike `mount add` there is
/// no `--config-json`/`--limits-json` override and no auth to resolve, since
/// setup only ever offers this path to a provider whose manifest declares
/// neither.
pub(crate) async fn quick_start_definition(
    rpc: &RpcClient,
    output: &crate::ui::output::Output,
    provider_id: ProviderId,
    manifest: &ProviderManifest,
) -> anyhow::Result<MountDefinition> {
    let mounts = rpc.list_mounts().await?;
    let mount_name = provider_selection::mount_name(
        &mounts,
        &manifest.default_mount,
        None,
        false,
        true,
        output,
        mount_add_key_width(),
    )?;
    let config = config_bytes(create_config(manifest, output, false)?)?;
    Ok(MountDefinition {
        name: mount_name,
        provider: provider_id,
        auth: None,
        limits: manifest_limits(manifest),
        config,
    })
}

#[cfg(test)]
mod tests {
    use super::{MountName, mount_created_value, sign_in_failed_value};

    #[test]
    fn mount_created_value_names_the_mount() {
        let name = MountName::try_from("dns").unwrap();
        assert_eq!(mount_created_value(&name), "/dns created");
    }

    #[test]
    fn sign_in_failed_value_says_no_mount_was_created() {
        let name = MountName::try_from("github").unwrap();
        assert_eq!(
            sign_in_failed_value(&name),
            "sign-in did not complete; mount `github` was not created"
        );
    }
}
