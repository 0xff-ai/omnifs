//! Shared onboarding and lifecycle stages used by `setup`, `mount add`, and `up`.
//!
//! Commands own narration. This module owns the stage behavior so the guided
//! setup wizard and express `mount add` lane cannot drift from each other.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, anyhow};
use omnifs_caps::{Grants, Limits};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::{Name as MountName, Spec, UpgradePlan};
use omnifs_workspace::provider::{Catalog, ProviderManifest};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::auth::AuthSelection;
use crate::commands::mount::mount_file::MountFile;
use crate::commands::mount::provider_selection::ProviderSelection;
use crate::commands::mount::spec_creation::{CreatedMountSpec, MountSpecCreator};
use crate::commands::mount::{AddArgs, AuthImportDecision, ImportOutcome};
use crate::commands::setup::host_os::HostOs;
use crate::error::{ExitCode, WithExitCode, WithHint};
use crate::launch::LaunchOutcome;
use crate::mount_config::MountConfig;
use crate::token_source::TokenSource;
use crate::workspace::Workspace;

pub(crate) struct EnvironmentReport {
    pub(crate) configured: bool,
}

pub(crate) struct MountInitOutcome {
    pub(crate) mount_name: String,
    pub(crate) status: MountInitStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountInitStatus {
    Ready,
    SignInDeclined,
}

pub(crate) struct FirstRead {
    pub(crate) command: String,
    pub(crate) output: String,
}

pub(crate) struct MountInitPlan {
    mount_name: MountName,
    manifest: ProviderManifest,
    effective_auth: Option<AuthSelection>,
    imported_token: Option<secrecy::SecretString>,
    spec: Spec,
    mount_path: PathBuf,
    existing_mount: bool,
    upgrade_approval: Option<UpgradePlan>,
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

    /// The single decision combinator for every wizard prompt site: an explicit
    /// value wins; `--yes` takes the default; `--no-input` and non-interactive
    /// sessions bail with a flag hint; otherwise prompt.
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

pub(crate) fn environment_check(
    os: HostOs,
    workspace: &Workspace,
) -> anyhow::Result<EnvironmentReport> {
    if os == HostOs::Unsupported {
        anyhow::bail!("omnifs does not yet run on this platform");
    }
    // Review mode is for a workspace that already has something to review: a
    // mount.
    let configured = workspace.mounts().is_ok_and(|mounts| !mounts.is_empty());
    Ok(EnvironmentReport { configured })
}

/// Informational Docker reachability, for setup's environment stage (the
/// daemon always runs host-native; the caller shows this row only when the
/// effective `[[frontends]]` plan actually launches a Docker frontend).
/// Never fails setup: an unreachable daemon (or an unresolvable target) is
/// reported, not raised.
pub(crate) enum DockerReachability {
    Running { version: String },
    Unreachable,
}

pub(crate) async fn probe_docker_reachability(
    config: &crate::config::Config,
) -> DockerReachability {
    use crate::frontend_container::{FRONTEND_CONTAINER_BASE, resolve_frontend_image};
    use crate::launch_backend::DockerTarget;
    use crate::runtime::{DockerProbeOutcome, Runtime};

    let Ok(image) = resolve_frontend_image(None, config) else {
        return DockerReachability::Unreachable;
    };
    let Ok(target) = DockerTarget::new(
        FRONTEND_CONTAINER_BASE.to_string(),
        image.as_str().to_string(),
    ) else {
        return DockerReachability::Unreachable;
    };
    match Runtime::probe_docker(&target).await {
        DockerProbeOutcome::Reachable(runtime) => {
            let version = runtime
                .server_version()
                .await
                .unwrap_or_else(|| "running".to_string());
            DockerReachability::Running { version }
        },
        DockerProbeOutcome::ConnectFailed(_) | DockerProbeOutcome::PingFailed(_) => {
            DockerReachability::Unreachable
        },
    }
}

#[allow(clippy::too_many_lines)] // linear ledger narration reads best inline
pub(crate) async fn configure_mount(
    args: AddArgs,
    workspace: &Workspace,
    standalone: bool,
    session: &mut crate::ui::session::Session,
) -> anyhow::Result<MountInitOutcome> {
    let mut plan = spec_creation(&args, workspace, session)?;
    if standalone {
        session.phase(plan.manifest.id.as_str());
    }
    persist_mount_spec(workspace, &plan, session).await?;
    let status = plan.authenticate(&args, workspace, session).await?;

    match status {
        MountInitStatus::Ready => session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Done,
            "mount ready",
            plan.mount_name.as_str(),
        )),
        MountInitStatus::SignInDeclined => session.row(crate::ui::report::Row::new(
            crate::ui::style::Glyph::Skip,
            "sign in",
            format!(
                "skipped; run `omnifs mount reauth {}` later",
                plan.mount_name
            ),
        )),
    }
    if standalone && !workspace.daemon().ready().await {
        session.note("run `omnifs up` to start serving it");
    }

    if standalone {
        let running = workspace.daemon().ready().await;
        if running {
            let path = browse_path(plan.mount_name.as_str());
            session.note(crate::ui::hint(
                &format!("ls {}", path.display()),
                "browse it",
            ));
        } else {
            session.note(crate::ui::hint("omnifs up", "start serving"));
        }
    }

    crate::telemetry::maybe_print_health_nudge(workspace).await;

    Ok(MountInitOutcome {
        mount_name: plan.mount_name.to_string(),
        status,
    })
}

/// Init is interactive only with real stdin and stderr terminals and without
/// `--no-input`. A piped stdin is non-interactive even without the flag, so
/// prompt sites bail cleanly (naming the satisfying flags) instead of hitting
/// a prompt library's raw "not a terminal" error. Mirrors setup's terminal derivation.
fn init_interactive(args: &AddArgs) -> bool {
    !args.no_input && crate::ui::prompt::is_terminal()
}

#[allow(clippy::too_many_lines)] // one linear spec-assembly path
pub(crate) fn spec_creation(
    args: &AddArgs,
    workspace: &Workspace,
    session: &mut crate::ui::session::Session,
) -> anyhow::Result<MountInitPlan> {
    let paths = workspace.layout();
    crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;
    let interactive = init_interactive(args);
    let catalog = workspace.catalog();
    let mounts = workspace.mounts()?;
    let installed = crate::catalog::installed_providers(catalog)?;
    if installed.is_empty() {
        anyhow::bail!("no built-in or disk providers are available");
    }

    // No provider argument in an interactive session: choose one with the
    // single-select variant of the setup picker instead of a bare list.
    let picked = if args.provider.is_none() && interactive {
        let rows = crate::ui::picker::build_rows(&installed, &std::collections::BTreeMap::new());
        Some(crate::ui::picker::select("Which provider?", rows)?)
    } else {
        None
    };
    let provider_selection = ProviderSelection::new(&mounts, &installed);
    let (provider_name, mount_name) = provider_selection.resolve(
        args.provider.as_deref().or(picked.as_deref()),
        args.as_name.as_deref(),
        interactive,
        args.yes,
        session,
    )?;

    let (provider, manifest) = crate::catalog::find_installed(&installed, &provider_name)
        .ok_or_else(|| {
            anyhow!(
                "provider `{provider_name}` not found; available: {}",
                provider_selection.provider_names().join(", ")
            )
        })
        .with_hint("Run `omnifs provider ls` to list available providers (or `omnifs mount add` with no args to pick one interactively)")
        .with_hint(format!(
            "Or run `omnifs provider add <wasm-or-dir>` to install provider artifacts into {}",
            paths.providers_dir.display()
        ))?;
    let reference = provider.reference();
    let existing_mount = mounts.iter().find(|mount| mount.name == mount_name);
    let upgrade_approval = match existing_mount {
        Some(existing) if existing.config.provider.id == provider.id => {
            anyhow::bail!(
                "mount `{mount_name}` already exists for this provider artifact; remove it first or choose a different name"
            );
        },
        Some(existing) => approved_upgrade_for_existing_mount(
            catalog,
            existing,
            manifest,
            &provider_name,
            &mount_name,
            interactive,
            session,
        )?,
        None => None,
    };

    let auth_manifest = manifest.wasm_auth_manifest();
    let default_auth = selected_auth(
        args,
        &reference,
        &mount_name,
        manifest,
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
        args.yes,
    )
    .resolve(Some(session))?;
    let ImportOutcome { auth, token } = import_outcome;

    if !interactive && token.is_none() && auth.as_ref().is_some_and(AuthSelection::is_oauth) {
        return Err(anyhow!(
            "cannot complete OAuth for `{provider_name}` without an interactive terminal; pass --token-env VAR with --scheme <static-token-scheme>, pass --no-auth, or run interactively"
        ))
        .with_exit_code(ExitCode::AuthRequired);
    }

    let creator = MountSpecCreator::new(&reference, &mount_name, manifest);
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
        creator.create(interactive)?
    };
    apply_mount_overrides(args, manifest, &creator, &mut created)?;

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
        existing_mount: existing_mount.is_some(),
        upgrade_approval,
    })
}

impl MountInitPlan {
    async fn authenticate(
        &mut self,
        args: &AddArgs,
        workspace: &Workspace,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<MountInitStatus> {
        crate::commands::mount::render_consent_block(session, &self.manifest);
        let plan = self;
        let Some(auth) = plan.effective_auth.as_ref() else {
            return Ok(MountInitStatus::Ready);
        };
        let interactive = init_interactive(args);
        if let Some(token) = plan.imported_token.take() {
            crate::commands::mount::run_static_token_init(
                &plan.manifest,
                auth,
                token,
                &workspace.layout().credentials_file,
                !args.no_validate,
                session,
            )
            .await?;
        } else if auth.is_oauth() {
            // Gate the browser handoff when interactive: a decline is a clean skip,
            // not a failure.
            if interactive && !args.yes {
                let proceed = crate::ui::prompt::Confirm::new(format!(
                    "Sign in to {} in your browser now?",
                    plan.mount_name
                ))
                .with_default(true)
                .ask()?;
                if !proceed {
                    return Ok(MountInitStatus::SignInDeclined);
                }
            }
            crate::auth::login::login_with_spec(
                workspace,
                &plan.spec,
                auth.account.as_deref(),
                args.no_browser,
                args.no_input,
                &args.scopes,
                session,
            )
            .await
            .inspect_err(|_| {
                session.note(format!(
                    "login did not complete; run `omnifs mount reauth {}` to finish",
                    plan.mount_name
                ));
            })?;
            session.row(crate::ui::report::Row::new(
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
                    session.note(format!("create a token at {url}"));
                }
                for step in &guidance.setup_steps {
                    session.note(step);
                }
                if let Some(url) = &guidance.docs_url {
                    session.note(url);
                }
            }
            let source = TokenSource::resolve(
                args.token.as_deref(),
                args.token_env.as_deref(),
                interactive,
            )?;
            let token = source.read()?;
            crate::commands::mount::run_static_token_init(
                &plan.manifest,
                auth,
                token,
                &workspace.layout().credentials_file,
                !args.no_validate,
                session,
            )
            .await?;
        }
        Ok(MountInitStatus::Ready)
    }
}

pub(crate) fn verify_first_read(
    outcome: &LaunchOutcome,
    mount_name: &str,
) -> anyhow::Result<FirstRead> {
    let mount_point = outcome
        .mount_point
        .clone()
        .or_else(omnifs_workspace::layout::resolve_mount_point)
        .ok_or_else(|| anyhow!("cannot resolve mount point for first read"))?;
    run_host_ls(&mount_point.join(mount_name))
}

pub(crate) async fn wait_until_ready(
    workspace: &Workspace,
    timeout: Duration,
) -> anyhow::Result<()> {
    let started = tokio::time::Instant::now();
    loop {
        if workspace.daemon().ready().await {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(anyhow!(
                "daemon did not become ready within {}s",
                timeout.as_secs()
            ))
            .with_exit_code(ExitCode::DaemonUnavailable);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
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

async fn persist_mount_spec(
    workspace: &Workspace,
    plan: &MountInitPlan,
    session: &mut crate::ui::session::Session,
) -> anyhow::Result<()> {
    let request = async {
        if plan.existing_mount {
            workspace
                .daemon()
                .update_mount_if_ready(&plan.spec, plan.upgrade_approval.as_ref())
                .await
        } else {
            workspace.daemon().create_mount_if_ready(&plan.spec).await
        }
    };
    let (result, progress) =
        await_with_elapsed_progress("mount", &format!("saving {}", plan.mount_name), request).await;

    match result {
        Ok(Some(report)) if report.failure.is_none() => {
            progress.settle_ok(format!("{} applied to daemon", plan.mount_name));
        },
        Ok(Some(report)) => {
            let reason = report
                .failure
                .as_ref()
                .map_or("unknown error", |failure| failure.reason.as_str());
            progress.settle_warn(format!("{} saved; daemon: {reason}", plan.mount_name));
            session.note("saved locally; run `omnifs up` to restart with the new mount");
        },
        Ok(None) => {
            workspace.put_mount(&plan.spec)?;
            progress.settle_ok(format!("{} saved locally", plan.mount_name));
        },
        Err(error) => {
            workspace.put_mount(&plan.spec)?;
            progress.settle_warn(format!("{} saved; daemon unavailable", plan.mount_name));
            session.note(format!(
                "could not apply mount `{}`: {error:#}",
                plan.mount_name
            ));
            session.note("saved locally; run `omnifs up` to restart with the new mount");
        },
    }
    // `Wrote <path>` collapses to a single dim continuation, printed once.
    session.note(format!(
        "wrote {}",
        WorkspaceLayout::display(&plan.mount_path)
    ));
    Ok(())
}

/// Drive a future while emitting elapsed progress often enough for the text
/// spinner and NDJSON event stream to remain live during a slow operation.
async fn await_with_elapsed_progress<F, T>(
    key: &str,
    verb: &str,
    future: F,
) -> (T, crate::ui::LiveRow)
where
    F: Future<Output = T>,
{
    let mut progress = crate::ui::LiveRow::start(key, verb);
    progress.update(verb);
    tokio::pin!(future);
    let mut ticks = tokio::time::interval(Duration::from_millis(200));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticks.tick().await;
    loop {
        tokio::select! {
            output = &mut future => return (output, progress),
            _ = ticks.tick() => progress.update_elapsed(verb),
        }
    }
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
    if let Some(raw) = args.capabilities_json.as_deref() {
        created.capabilities = Some(parse_json_flag::<Grants>("--capabilities-json", raw)?);
    }
    if let Some(raw) = args.limits_json.as_deref() {
        created.limits = Some(parse_json_flag::<Limits>("--limits-json", raw)?);
    }
    Ok(())
}

fn parse_json_flag<T: DeserializeOwned>(flag: &'static str, raw: &str) -> anyhow::Result<T> {
    serde_json::from_str(raw).with_context(|| format!("parse {flag}"))
}

fn run_host_ls(path: &Path) -> anyhow::Result<FirstRead> {
    let output = Command::new("ls")
        .arg(path)
        .output()
        .with_context(|| format!("run ls {}", path.display()))?;
    let command = format!("ls {}", path.display());
    if !output.status.success() {
        anyhow::bail!(
            "first read failed: `{command}` exited with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(FirstRead {
        command,
        output: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

fn browse_path(mount_name: &str) -> PathBuf {
    omnifs_workspace::layout::resolve_mount_point()
        .unwrap_or_else(|| PathBuf::from("~/omnifs"))
        .join(mount_name)
}

fn approved_upgrade_for_existing_mount(
    catalog: &Catalog,
    existing: &MountConfig,
    candidate_manifest: &ProviderManifest,
    provider_name: &str,
    mount_name: &MountName,
    interactive: bool,
    session: &mut crate::ui::session::Session,
) -> anyhow::Result<Option<UpgradePlan>> {
    let existing_provider = existing.config.provider_name();
    if existing_provider.as_str() != provider_name {
        anyhow::bail!(
            "mount `{mount_name}` already exists for provider `{existing_provider}`; remove it first or choose a different name"
        );
    }

    let Some(pinned) = catalog
        .get(&existing.config.provider.id)
        .with_context(|| format!("load pinned provider for mount `{mount_name}`"))?
    else {
        anyhow::bail!(
            "the provider version mount `{mount_name}` was created with is no longer installed"
        );
    };
    let pinned_manifest = pinned
        .manifest()
        .with_context(|| format!("read pinned provider manifest for mount `{mount_name}`"))?;
    let plan = UpgradePlan::diff(&pinned_manifest, candidate_manifest);
    if !plan.requires_approval() {
        return Ok(None);
    }
    if !interactive {
        anyhow::bail!(
            "`omnifs mount add --no-input` cannot approve provider upgrade changes for existing mount `{mount_name}`"
        );
    }

    session.note(format!("{provider_name} now requests different access:"));
    for change in crate::upgrade::describe_upgrade_plan(&plan) {
        session.note(change);
    }
    let approved = crate::ui::prompt::Confirm::new("Approve this provider upgrade?")
        .with_default(false)
        .ask()?;
    if !approved {
        anyhow::bail!("aborted");
    }
    Ok(Some(plan))
}
