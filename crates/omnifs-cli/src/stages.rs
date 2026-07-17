//! Shared mount-creation and lifecycle stages used by `mount add` and `up`.
//!
//! Commands own narration. This module owns the stage behavior so mount
//! creation and authentication stay in one path.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow};
use omnifs_workspace::layout::WorkspaceLayout;
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
use crate::workspace::Workspace;

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
    mount_path: PathBuf,
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

#[allow(clippy::too_many_lines)] // linear narration reads best inline
pub(crate) async fn configure_mount(
    args: AddArgs,
    workspace: &Workspace,
    output: &crate::ui::output::Output,
    prompt: PromptMode,
) -> anyhow::Result<MountInitOutcome> {
    let mut plan = spec_creation(&args, workspace, output, prompt)?;
    let status = plan.authenticate(&args, workspace, output, prompt).await?;
    persist_mount_spec(workspace, &plan, output)?;

    match status {
        MountInitStatus::Ready => output.row(&crate::ui::report::Row::new(
            crate::ui::style::Glyph::Done,
            "mount ready",
            plan.mount_name.as_str(),
        )),
        MountInitStatus::SignInDeclined => output.row(&crate::ui::report::Row::new(
            crate::ui::style::Glyph::Skip,
            "sign in",
            format!(
                "skipped; run `omnifs mount reauth {}` later",
                plan.mount_name
            ),
        )),
    }
    if !workspace.daemon().ready().await {
        output.note("run `omnifs up` to start serving it");
    }

    let running = workspace.daemon().ready().await;
    if running {
        let path = browse_path(plan.mount_name.as_str());
        output.note(crate::ui::hint(
            &format!("ls {}", path.display()),
            "browse it",
        ));
    } else {
        output.note(crate::ui::hint("omnifs up", "start serving"));
    }

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
) -> anyhow::Result<MountInitPlan> {
    let paths = workspace.layout();
    let interactive = init_interactive(prompt);
    let mounts = workspace.mounts()?;
    let embedded = EmbeddedProviders::load()?;
    let provider_selection = ProviderSelection::new(&mounts, &embedded);

    // No provider argument in an interactive output: choose one with the
    // generic single-select prompt instead of a bare list.
    let picked = if args.provider.is_none() && interactive {
        let options = crate::provider_resolver::provider_options(
            &embedded,
            &std::collections::BTreeMap::new(),
        );
        let choices = options
            .into_iter()
            .map(|option| (option.name.clone(), option.name, option.hint));
        Some(
            crate::ui::prompt::Select::new("Which provider?")
                .options(choices)
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
    let resolved = ProviderResolver::new(&paths.providers_dir, &embedded).resolve(&selector)?;
    if resolved.newly_retained
        && let Err(error) = crate::provider_warmup::ProviderWarmup::new(workspace.layout())
            .spawn_background(resolved.reference.id, output)
    {
        output.narrate(crate::ui::style::warn(format!(
            "Couldn't start background provider warmup ({error:#}); daemon startup will load the provider."
        )));
    }
    let provider_name = resolved.reference.meta.name.to_string();
    let mount_name = provider_selection.mount_name(
        &resolved.manifest.default_mount,
        args.as_name.as_deref(),
        interactive,
        prompt.yes,
        output,
    )?;
    let reference = resolved.reference;
    let manifest = resolved.manifest;
    if mounts.iter().any(|mount| mount.name == mount_name) {
        anyhow::bail!(
            "mount `{mount_name}` already exists; remove it first or choose a different name"
        );
    }

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
    // Resolve auth first: an ambient credential (imported under --yes or on the
    // interactive prompt) promotes an OAuth default to a static token, which lets
    // a `--no-input` run of an OAuth-default provider complete headlessly. The
    // OAuth bail only fires when nothing was imported.
    let import_outcome = AuthImportDecision::new(
        default_auth,
        auth_manifest.as_ref(),
        &provider_name,
        interactive,
        prompt.yes,
    )
    .resolve(output)?;
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
    let mount_path = paths.mounts_dir.join(format!("{mount_name}.json"));

    Ok(MountInitPlan {
        mount_name,
        manifest: manifest.clone(),
        effective_auth: auth,
        imported_token: token,
        spec,
        mount_path,
    })
}

impl MountInitPlan {
    async fn authenticate(
        &mut self,
        args: &AddArgs,
        workspace: &Workspace,
        output: &crate::ui::output::Output,
        prompt: PromptMode,
    ) -> anyhow::Result<MountInitStatus> {
        crate::commands::mount::render_consent_block(output, &self.manifest);
        let plan = self;
        let Some(auth) = plan.effective_auth.as_ref() else {
            return Ok(MountInitStatus::Ready);
        };
        let interactive = init_interactive(prompt);
        if let Some(token) = plan.imported_token.take() {
            crate::commands::mount::run_static_token_init(
                &plan.manifest,
                auth,
                token,
                &workspace.layout().credentials_file,
                !args.no_validate,
                output,
            )
            .await?;
        } else if auth.is_oauth() {
            // Gate the browser handoff when interactive: a decline is a clean skip,
            // not a failure.
            if interactive && !prompt.yes {
                let proceed = crate::ui::prompt::Confirm::new(format!(
                    "Sign in to {} in your browser now?",
                    plan.mount_name
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
                args.no_browser,
                prompt.no_input,
                &args.scopes,
                output,
            )
            .await
            .inspect_err(|_| {
                output.note(format!(
                    "login did not complete; run `omnifs mount reauth {}` to finish",
                    plan.mount_name
                ));
            })?;
            output.row(&crate::ui::report::Row::new(
                crate::ui::style::Glyph::Done,
                "signed in",
                "done",
            ));
        } else {
            if interactive && let Ok(scheme) = auth.static_token_scheme(&plan.manifest) {
                let guidance = plan
                    .manifest
                    .auth
                    .as_ref()
                    .map(|auth| auth.guidance_for(&scheme.key))
                    .unwrap_or_default();
                if let Some(url) = &scheme.creation_url {
                    output.note(format!("create a token at {url}"));
                }
                for step in &guidance.setup_steps {
                    output.note(step);
                }
                if let Some(url) = &guidance.docs_url {
                    output.note(url);
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
                &workspace.layout().credentials_file,
                !args.no_validate,
                output,
            )
            .await?;
        }
        Ok(MountInitStatus::Ready)
    }
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

fn persist_mount_spec(
    workspace: &Workspace,
    plan: &MountInitPlan,
    output: &crate::ui::output::Output,
) -> anyhow::Result<()> {
    workspace.put_mount_uncommitted(&plan.spec)?;
    workspace.commit_mounts()?;
    output.row(&crate::ui::report::Row::new(
        crate::ui::style::Glyph::Done,
        "desired state",
        format!("{} recorded", plan.mount_name),
    ));
    // `Wrote <path>` collapses to a single dim continuation, printed once.
    output.note(format!(
        "wrote {}",
        WorkspaceLayout::display(&plan.mount_path)
    ));
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

fn browse_path(mount_name: &str) -> PathBuf {
    omnifs_workspace::layout::resolve_mount_point()
        .unwrap_or_else(|| PathBuf::from("~/omnifs"))
        .join(mount_name)
}
