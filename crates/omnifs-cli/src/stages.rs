//! Shared mount-creation and lifecycle stages used by `mount add` and `up`.
//!
//! Commands own narration. This module owns the stage behavior so mount
//! creation and authentication stay in one path.

use std::time::Duration;

use anyhow::{Context, anyhow};
use omnifs_workspace::mounts::{Limits, Name as MountName, Spec};
use omnifs_workspace::provider::{ProviderAuthManifest, ProviderManifest};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::auth::AuthSelection;
use crate::commands::mount::mount_file::MountFile;
use crate::commands::mount::provider_selection::ProviderSelection;
use crate::commands::mount::spec_creation::{CreatedMountSpec, MountSpecCreator};
use crate::commands::mount::{AddArgs, AuthImportDecision, ImportOutcome};
use crate::error::{ExitCode, WithExitCode};
use crate::provider_bundle::EmbeddedProviders;
use crate::provider_resolver::ProviderResolver;
use crate::token_source::TokenSource;
use omnifs_workspace::Workspace;

pub(crate) struct MountInitOutcome {
    pub(crate) mount_name: String,
    pub(crate) status: MountInitStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountInitStatus {
    Ready,
    SignInDeclined,
}

pub(crate) struct MountInitPlan {
    mount_name: MountName,
    manifest: ProviderManifest,
    effective_auth: Option<AuthSelection>,
    imported_token: Option<secrecy::SecretString>,
    spec: Spec,
    // Whether the provider needs no sign-in at all (captured from the raw
    // auth selection before `AuthImportDecision` resolves it), so the
    // deferred `mount ... created` row (printed only after persist, once
    // authentication has already settled) can still carry the compact
    // receipt's "(no sign-in needed)" annotation.
    no_auth_needed: bool,
}

/// How much a per-mount receipt says (spec 3.2 vs 3.3). Exactly two honest
/// callers need different verbosity from the same mount-creation path:
/// `mount add`'s [`Full`](ReceiptStyle::Full) receipt names every settled
/// fact including the provider artifact retained in the store, while `omnifs
/// setup`'s [`Compact`](ReceiptStyle::Compact) receipt drops that row (the
/// provider already appeared in the services multi-select moments earlier)
/// and annotates a mount needing no authentication inline instead of relying
/// on the reader to infer it from the absence of a `signed in` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptStyle {
    Full,
    Compact,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PromptMode {
    pub(crate) interactive: bool,
    pub(crate) yes: bool,
    pub(crate) no_input: bool,
}

impl PromptMode {
    /// Build prompt policy from command flags and the shared terminal check.
    pub(crate) fn from_flags(yes: bool, no_input: bool) -> Self {
        Self {
            interactive: crate::ui::prompt::is_terminal() && !no_input,
            yes,
            no_input,
        }
    }

    /// The single decision combinator for every guided prompt site: an explicit
    /// value wins; `--yes` takes the default; `--no-input` and non-interactive
    /// runs bail with a flag hint; otherwise prompt.
    pub(crate) fn resolve<T>(
        self,
        explicit: Option<T>,
        default: impl FnOnce() -> T,
        flag_hint: &str,
        prompt: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if let Some(value) = explicit {
            return Ok(value);
        }
        if self.yes {
            return Ok(default());
        }
        if self.no_input {
            anyhow::bail!("`--no-input` needs {flag_hint}, or pass --yes to accept the default");
        }
        if !self.interactive {
            anyhow::bail!("this step needs a terminal; pass {flag_hint} or --yes");
        }
        prompt()
    }
}

/// Keys `configure_mount`'s block may ever print, across every branch (spec
/// 2.1: a block's key column is sized to the whole block, computed once up
/// front, even though rows settle one at a time as each stage of mount
/// creation completes). `mount name` only fires on an interactive/`--yes`
/// name collision ([`crate::commands::mount::provider_selection`]);
/// `provider` only for [`ReceiptStyle::Full`]; the sign-in branch prints
/// exactly one of `sign in` (declined) or the shared
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
    workspace: &Workspace,
    output: &crate::ui::output::Output,
    prompt: PromptMode,
    receipt_style: ReceiptStyle,
) -> anyhow::Result<MountInitOutcome> {
    let mut plan = spec_creation(&args, workspace, output, prompt, receipt_style)?;
    let status = plan
        .authenticate(&args, workspace, output, prompt, receipt_style)
        .await?;
    persist_mount_spec(workspace, &plan, output)?;

    // The `mount ... created` row prints only once the spec is actually on
    // disk (spec 3.2/3.3): an auth failure above returns before this line
    // runs (`?` on `authenticate`), so the transcript never claims a mount
    // exists when persist never happened. A declined sign-in still reaches
    // here (it resolves to `Ok(SignInDeclined)`, not an error), so this row
    // and the skip row below both print, in that order, and both are true.
    output.ledger_row(
        &crate::ui::render::LedgerRow::new(
            crate::ui::style::Glyph::Done,
            "mount",
            mount_created_value(&plan.mount_name, receipt_style, plan.no_auth_needed),
        ),
        mount_add_key_width(),
    );

    if status == MountInitStatus::SignInDeclined {
        output.ledger_row(
            &crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Skip,
                "sign in",
                sign_in_declined_value(&plan.mount_name),
            ),
            mount_add_key_width(),
        );
    }

    // The single closing line (spec 3.3) is the caller's job: `mount add`
    // names the mount it just created, while `omnifs setup` calls this in a
    // loop across several providers and prints its own summary once at the
    // end, so no per-provider closing line belongs here.
    crate::metrics::maybe_print_health_nudge(workspace, output.clone()).await;

    Ok(MountInitOutcome {
        mount_name: plan.mount_name.to_string(),
        status,
    })
}

/// Init is interactive only with real stdin and stderr terminals and without
/// `--no-input`. A piped stdin is non-interactive even without the flag, so
/// prompt sites bail cleanly (naming the satisfying flags) instead of hitting
/// a prompt library's raw "not a terminal" error.
fn init_interactive(prompt: PromptMode) -> bool {
    prompt.interactive
}

#[allow(clippy::too_many_lines)] // one linear spec-assembly path
pub(crate) fn spec_creation(
    args: &AddArgs,
    workspace: &Workspace,
    output: &crate::ui::output::Output,
    prompt: PromptMode,
    receipt_style: ReceiptStyle,
) -> anyhow::Result<MountInitPlan> {
    let interactive = init_interactive(prompt);
    let mounts = crate::mount_config::load_mounts(workspace)?;
    let embedded = EmbeddedProviders::load()?;
    let provider_selection = ProviderSelection::new(&mounts, &embedded);

    // No provider argument in an interactive output: choose one with the
    // generic single-select prompt instead of a bare list. The panel carries
    // the full, untruncated consent facts (spec 2.6): domains called, memory
    // ceiling, and auth scheme, one sentence per line, never the compact
    // truncated summary `mount add`'s later consent block uses.
    let picked = if args.provider.is_none() && interactive {
        let options = crate::provider_resolver::provider_options(
            &embedded,
            &std::collections::BTreeMap::new(),
        );
        let choices = options.into_iter().map(|option| {
            let detail = embedded
                .by_name(&option.name)
                .map(|entry| crate::capability::consent_detail(entry.manifest()))
                .unwrap_or_default();
            (option.name.clone(), option.name, detail)
        });
        Some(
            crate::ui::prompt::Select::new("Which provider?")
                .detailed_options(choices)
                .ask_with_output(output)?,
        )
    } else {
        None
    };
    let selector = provider_selection.select(
        args.provider.as_deref().or(picked.as_deref()),
        interactive,
        output,
    )?;
    let resolved = ProviderResolver::new(workspace.catalog(), &embedded).resolve(&selector)?;
    if resolved.newly_retained
        && let Err(error) = crate::provider_warmup::ProviderWarmup::new(
            workspace.warmup().clone(),
            workspace.catalog().clone(),
        )
        .spawn_background(resolved.reference.id, output)
    {
        output.narrate(crate::ui::style::warn(
            format!(
                "Couldn't start background provider warmup ({error:#}); daemon startup will load the provider."
            ),
            crate::ui::style::Stream::Stderr,
        ));
    }
    let provider_name = resolved.reference.meta.name.to_string();
    let mount_name = provider_selection.mount_name(
        &resolved.manifest.default_mount,
        args.as_name.as_deref(),
        interactive,
        prompt.yes,
        output,
        mount_add_key_width(),
    )?;
    let reference = resolved.reference;
    let manifest = resolved.manifest;
    if mounts.iter().any(|mount| mount.name == mount_name) {
        anyhow::bail!(
            "mount `{mount_name}` already exists; remove it first or choose a different name"
        );
    }
    // Auth is resolved before either receipt row prints, not just before
    // `authenticate` runs, because the compact receipt (spec 3.2) folds
    // whether this provider needs a sign-in step into the `mount` row's
    // value itself (`no sign-in needed`) rather than relying on the reader
    // to infer it from the absence of a later `signed in` row.
    let auth_manifest = manifest
        .auth
        .as_ref()
        .map(ProviderAuthManifest::wasm_auth_manifest);
    let default_auth = selected_auth(
        args,
        &reference,
        &mount_name,
        &manifest,
        auth_manifest.as_ref(),
    )?;

    // Receipt rows for the two facts already true at this point: the
    // provider artifact is retained in the store (`ProviderResolver::resolve`
    // above either found it there or just retained it), and the mount name is
    // validated and free. The remaining work below (auth, then the actual
    // spec write in `persist_mount_spec`) either fills in these two rows'
    // consequences or fails outright, so nothing here overclaims (spec 3.3).
    // The compact style (setup) drops the `provider` row: the provider
    // already appeared in the services multi-select moments earlier, so
    // repeating its retained-artifact fact here would be noise.
    let key_width = mount_add_key_width();
    if receipt_style == ReceiptStyle::Full {
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
    }
    // Captured before `default_auth` moves into `AuthImportDecision::new`
    // below; the deferred `mount ... created` row (printed by `configure_mount`
    // only once the spec is actually persisted) still needs this fact for its
    // compact-receipt annotation.
    let no_auth_needed = default_auth.is_none();

    // An ambient credential (imported under --yes or on the interactive
    // prompt) promotes an OAuth default to a static token, which lets a
    // `--no-input` run of an OAuth-default provider complete headlessly. The
    // OAuth bail only fires when nothing was imported.
    let import_outcome = AuthImportDecision::new(
        default_auth,
        auth_manifest.as_ref(),
        &provider_name,
        interactive,
        prompt.yes,
    )
    .resolve(output, mount_add_key_width())?;
    let ImportOutcome { auth, token } = import_outcome;

    if !interactive && token.is_none() && auth.as_ref().is_some_and(AuthSelection::is_oauth) {
        return Err(anyhow!(
            "cannot complete OAuth for `{provider_name}` without an interactive terminal; pass --token-env VAR with --scheme <static-token-scheme>, pass --no-auth, or run interactively"
        ))
        .with_exit_code(ExitCode::AuthRequired);
    }

    let creator = MountSpecCreator::new(&reference, &mount_name, &manifest);
    if !interactive && creator.requires_prompt() && args.config_json.is_none() {
        anyhow::bail!(
            "cannot complete provider config prompts for `{provider_name}` without an interactive terminal; pass --config-json <json>"
        );
    }
    // A supplied --config-json owns the whole config: skip default generation
    // (which validates manifest defaults and fails on required fields the
    // override provides) and validate the override where it is applied.
    let mut created = if args.config_json.is_some() {
        creator.create_for_config_override()
    } else {
        creator.create(output, interactive)?
    };
    apply_mount_overrides(args, &manifest, &creator, &mut created)?;

    let mount_file = MountFile::new(
        &mount_name,
        &reference,
        auth.as_ref(),
        &args.scopes,
        created,
    );
    let spec = mount_file.into_spec();

    Ok(MountInitPlan {
        mount_name,
        manifest: manifest.clone(),
        effective_auth: auth,
        imported_token: token,
        spec,
        no_auth_needed,
    })
}

impl MountInitPlan {
    async fn authenticate(
        &mut self,
        args: &AddArgs,
        workspace: &Workspace,
        output: &crate::ui::output::Output,
        prompt: PromptMode,
        receipt_style: ReceiptStyle,
    ) -> anyhow::Result<MountInitStatus> {
        // The compact receipt (setup) skips the description/needs/limits
        // lines: the services multi-select's detail panel already showed the
        // same consent facts (`capability.rs::consent_detail`) moments
        // earlier, so repeating them here would be noise rather than new
        // information.
        if receipt_style == ReceiptStyle::Full {
            crate::commands::mount::render_consent_block(output, &self.manifest);
        }
        let plan = self;
        let Some(auth) = plan.effective_auth.as_ref() else {
            return Ok(MountInitStatus::Ready);
        };
        let interactive = init_interactive(prompt);
        let key_width = mount_add_key_width();
        if let Some(token) = plan.imported_token.take() {
            crate::commands::mount::run_static_token_init(
                &plan.manifest,
                auth,
                token,
                workspace.credentials(),
                !args.no_validate,
                output,
                key_width,
            )
            .await?;
        } else if auth.is_oauth() {
            // Gate the browser handoff when interactive: a decline is a clean skip,
            // not a failure.
            if interactive && !prompt.yes {
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
            crate::auth::login::login_with_spec(
                workspace,
                &plan.spec,
                auth.account.as_deref(),
                crate::auth::LoginInteractivity {
                    no_browser: args.no_browser,
                    no_input: prompt.no_input,
                    scopes: &args.scopes,
                },
                output,
                key_width,
            )
            .await
            .inspect_err(|_| {
                // `persist_mount_spec` runs only after this call returns
                // `Ok` (spec 3.3), so a login failure here means nothing was
                // ever written: the recovery is re-running the whole add,
                // not `reauth` against a mount name that does not exist on
                // disk.
                output.note(sign_in_failed_value(&plan.manifest.id));
            })?;
        } else {
            if interactive && let Ok(scheme) = auth.static_token_scheme(&plan.manifest) {
                let guidance = plan
                    .manifest
                    .auth
                    .as_ref()
                    .map(|auth| auth.guidance_for(&scheme.key))
                    .unwrap_or_default();
                // Dim sentences (spec 3.3): informational setup guidance the
                // user reads once before pasting a token, not a settled fact.
                let dim =
                    |text: String| crate::ui::style::dim(text, crate::ui::style::Stream::Stderr);
                if let Some(url) = &scheme.creation_url {
                    output.note(dim(format!("create a token at {url}")));
                }
                for step in &guidance.setup_steps {
                    output.note(dim(step.clone()));
                }
                if let Some(url) = &guidance.docs_url {
                    output.note(dim(url.clone()));
                }
            }
            let source = TokenSource::resolve(
                args.token.as_deref(),
                args.token_env.as_deref(),
                interactive,
            )?;
            let token = source.read(output)?;
            crate::commands::mount::run_static_token_init(
                &plan.manifest,
                auth,
                token,
                workspace.credentials(),
                !args.no_validate,
                output,
                key_width,
            )
            .await?;
        }
        Ok(MountInitStatus::Ready)
    }
}

/// The `mount` receipt row's value (spec 3.2/3.3): `/<name> created`, plus a
/// `(no sign-in needed)` annotation only for the compact style when the
/// provider has no default auth at all. Pure so the exact wording is
/// testable without a workspace.
fn mount_created_value(
    mount_name: &MountName,
    receipt_style: ReceiptStyle,
    no_auth_needed: bool,
) -> String {
    if receipt_style == ReceiptStyle::Compact && no_auth_needed {
        format!("/{mount_name} created  (no sign-in needed)")
    } else {
        format!("/{mount_name} created")
    }
}

/// The `sign in` skip row's value (spec 3.2) when interactive sign-in is
/// declined: names the exact recovery command rather than just "skipped".
/// A decline still reaches `persist_mount_spec` (it resolves to
/// `Ok(SignInDeclined)`, not an error), so the mount really exists on disk
/// and `reauth` is a truthful recovery. Pure so the exact wording is testable
/// without a workspace.
fn sign_in_declined_value(mount_name: &MountName) -> String {
    format!("skipped; run `omnifs mount reauth {mount_name}` later")
}

/// The OAuth sign-in failure note (spec 3.3): unlike a decline, an actual
/// login error propagates as `Err` out of `authenticate`, so
/// `persist_mount_spec` never runs and nothing exists on disk. The recovery
/// is re-running the whole add, not `reauth` against a mount name nothing
/// created. Pure so the exact wording is testable without a workspace.
fn sign_in_failed_value(provider_id: &str) -> String {
    format!("sign-in did not complete; re-run `omnifs mount add {provider_id}` to retry")
}

pub(crate) fn parse_wait_duration(raw: &str) -> anyhow::Result<Duration> {
    let Some(value) = raw.strip_suffix('s') else {
        anyhow::bail!("duration `{raw}` must use seconds, for example 30s");
    };
    let seconds = value
        .parse::<u64>()
        .with_context(|| format!("parse duration `{raw}`"))?;
    Ok(Duration::from_secs(seconds))
}

fn selected_auth(
    args: &AddArgs,
    reference: &omnifs_workspace::ids::ProviderRef,
    mount_name: &MountName,
    manifest: &ProviderManifest,
    auth_manifest: Option<&omnifs_workspace::authn::AuthManifest>,
) -> anyhow::Result<Option<AuthSelection>> {
    if args.no_auth {
        return Ok(None);
    }
    if args.token.is_some() || args.token_env.is_some() {
        return AuthSelection::static_token(auth_manifest, args.scheme.as_deref(), None).map(Some);
    }
    if let Some(scheme) = args.scheme.as_deref() {
        return AuthSelection::from_scheme(auth_manifest, scheme, None).map(Some);
    }
    Ok(AuthSelection::from_provider_default(
        reference, mount_name, manifest,
    ))
}

/// Write the mount spec. Silent: `configure_mount` prints the `mount ...
/// created` row (spec 3.3) right after this call returns `Ok`, so the
/// transcript never claims the mount exists before it actually does. A
/// second row here would just restate the same fact in different words.
fn persist_mount_spec(
    workspace: &Workspace,
    plan: &MountInitPlan,
    _output: &crate::ui::output::Output,
) -> anyhow::Result<()> {
    workspace.desired_state().put_uncommitted(&plan.spec)?;
    workspace.desired_state().commit()?;
    Ok(())
}

fn apply_mount_overrides(
    args: &AddArgs,
    manifest: &ProviderManifest,
    creator: &MountSpecCreator<'_>,
    created: &mut CreatedMountSpec,
) -> anyhow::Result<()> {
    if let Some(raw) = args.config_json.as_deref() {
        let config: Value = parse_json_flag("--config-json", raw)?;
        if manifest.config.is_none() {
            anyhow::bail!(
                "--config-json was passed, but provider `{}` takes no config",
                manifest.id
            );
        }
        creator.validate(&config)?;
        created.config = Some(config);
    }
    if let Some(raw) = args.limits_json.as_deref() {
        created.limits = Some(parse_json_flag::<Limits>("--limits-json", raw)?);
    }
    Ok(())
}

fn parse_json_flag<T: DeserializeOwned>(flag: &'static str, raw: &str) -> anyhow::Result<T> {
    serde_json::from_str(raw).with_context(|| format!("parse {flag}"))
}

#[cfg(test)]
mod tests {
    use super::{
        MountName, PromptMode, ReceiptStyle, mount_created_value, sign_in_declined_value,
        sign_in_failed_value,
    };

    #[test]
    fn mount_created_value_is_plain_for_full_receipts_even_without_auth() {
        let name = MountName::try_from("dns").unwrap();
        assert_eq!(
            mount_created_value(&name, ReceiptStyle::Full, true),
            "/dns created"
        );
    }

    #[test]
    fn mount_created_value_annotates_no_sign_in_needed_only_for_compact_without_auth() {
        let name = MountName::try_from("dns").unwrap();
        assert_eq!(
            mount_created_value(&name, ReceiptStyle::Compact, true),
            "/dns created  (no sign-in needed)"
        );
        let name = MountName::try_from("github").unwrap();
        assert_eq!(
            mount_created_value(&name, ReceiptStyle::Compact, false),
            "/github created"
        );
    }

    #[test]
    fn sign_in_declined_value_names_the_exact_reauth_command() {
        let name = MountName::try_from("github").unwrap();
        assert_eq!(
            sign_in_declined_value(&name),
            "skipped; run `omnifs mount reauth github` later"
        );
    }

    #[test]
    fn sign_in_failed_value_points_at_retrying_add_not_reauth() {
        // Unlike a decline, a real login failure means nothing was ever
        // persisted (the whole point of Bug 2): `reauth` would target a
        // mount name that does not exist on disk, so the recovery must be
        // re-running the add instead.
        assert_eq!(
            sign_in_failed_value("github"),
            "sign-in did not complete; re-run `omnifs mount add github` to retry"
        );
        assert!(!sign_in_failed_value("github").contains("reauth"));
    }

    fn mode(interactive: bool, yes: bool, no_input: bool) -> PromptMode {
        PromptMode {
            interactive,
            yes,
            no_input,
        }
    }

    #[test]
    fn explicit_value_wins_without_touching_yes_no_input_or_the_prompt() {
        let called = mode(false, false, true).resolve(
            Some("explicit"),
            || "default",
            "--as",
            || panic!("explicit value must short-circuit before the prompt runs"),
        );
        assert_eq!(called.unwrap(), "explicit");
    }

    #[test]
    fn yes_takes_the_default_without_prompting() {
        let resolved = mode(true, true, false).resolve(
            None,
            || "default",
            "--as",
            || panic!("--yes must short-circuit before the prompt runs"),
        );
        assert_eq!(resolved.unwrap(), "default");
    }

    #[test]
    fn no_input_bails_naming_the_flag_hint_before_yes_or_the_prompt() {
        let error = mode(true, false, true)
            .resolve(
                None,
                || "default",
                "--as <name>",
                || panic!("--no-input must bail before the prompt runs"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("--as <name>"));
        assert!(error.to_string().contains("--yes"));
    }

    #[test]
    fn non_interactive_without_no_input_still_bails_naming_the_flag() {
        // A piped stdin with neither --yes nor --no-input is still
        // non-interactive: the bail message is the same shape as --no-input's.
        let error = mode(false, false, false)
            .resolve(
                None,
                || "default",
                "--as <name>",
                || panic!("a non-interactive run must bail before the prompt runs"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("--as <name>"));
        assert!(error.to_string().contains("terminal"));
    }

    #[test]
    fn interactive_without_yes_or_no_input_calls_the_prompt() {
        let resolved = mode(true, false, false).resolve(None, || "default", "--as", || Ok("typed"));
        assert_eq!(resolved.unwrap(), "typed");
    }
}
