//! Shared onboarding and lifecycle stages used by `setup`, `init`, and `up`.
//!
//! Commands own narration. This module owns the stage behavior so the guided
//! setup wizard and express `init` lane cannot drift from each other.

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
use crate::commands::init::mount_file::MountFile;
use crate::commands::init::provider_selection::ProviderSelection;
use crate::commands::init::spec_creation::{CreatedMountSpec, MountSpecCreator};
use crate::commands::init::{AuthImportDecision, ImportOutcome, InitArgs};
use crate::commands::setup::host_os::HostOs;
use crate::config::{ConfigFile, ConfiguredBackend};
use crate::error::{ExitCode, WithExitCode, WithHint};
use crate::launch::LaunchOutcome;
use crate::launch_backend::GUEST_MOUNT;
use crate::mount_config::MountConfig;
use crate::token_source::TokenSource;
use crate::ui::picker::PickerRow;
use crate::workspace::Workspace;

pub(crate) struct EnvironmentReport {
    pub(crate) configured: bool,
}

pub(crate) struct MountInitOutcome {
    pub(crate) mount_name: String,
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
    // mount. A persisted runtime alone (setup interrupted before adding a mount)
    // re-enters the fresh wizard, whose runtime stage preselects that value.
    let configured = workspace.mounts().is_ok_and(|mounts| !mounts.is_empty());
    Ok(EnvironmentReport { configured })
}

/// The recommended default runtime per host OS. Linux and WSL default to native
/// kernel FUSE: it is the host-native path, faster, and needs no container.
/// macOS keeps Docker as its default until the loopback NFS frontend earns it;
/// native on macOS stays the experimental opt-in.
pub(crate) fn default_runtime(os: HostOs) -> ConfiguredBackend {
    match os {
        HostOs::LinuxNative | HostOs::LinuxWsl => ConfiguredBackend::Native,
        // Unsupported never reaches here (setup bails in `environment_check`); it
        // shares macOS's Docker arm as the conservative, dependency-declaring default.
        HostOs::MacOs | HostOs::Unsupported => ConfiguredBackend::Docker,
    }
}

/// The lowercase runtime word used everywhere the wizard states a runtime fact.
pub(crate) fn runtime_word(backend: ConfiguredBackend) -> &'static str {
    match backend {
        ConfiguredBackend::Docker => "docker",
        ConfiguredBackend::Native => "native",
    }
}

pub(crate) fn runtime_selection(
    os: HostOs,
    current: Option<ConfiguredBackend>,
    requested: Option<ConfiguredBackend>,
    mode: PromptMode,
) -> anyhow::Result<ConfiguredBackend> {
    let preferred = current.unwrap_or_else(|| default_runtime(os));
    mode.resolve(
        requested,
        || preferred,
        "--runtime <docker|native>",
        || {
            let id = crate::ui::picker::select("How should the daemon run?", runtime_rows(os))?;
            parse_runtime_id(&id)
        },
    )
}

/// Map a runtime picker row id back to its backend.
fn parse_runtime_id(id: &str) -> anyhow::Result<ConfiguredBackend> {
    match id {
        "docker" => Ok(ConfiguredBackend::Docker),
        "native" => Ok(ConfiguredBackend::Native),
        other => Err(anyhow!("unknown runtime `{other}`")),
    }
}

/// Informational Docker reachability, for the setup environment stage. Never
/// fails setup: an unreachable daemon (or an unresolvable target) is reported,
/// not raised.
pub(crate) enum DockerReachability {
    Running { version: String },
    Unreachable,
}

pub(crate) async fn probe_docker_reachability(
    config: &crate::config::Config,
) -> DockerReachability {
    use crate::launch_backend::DockerTarget;
    use crate::runtime::{DockerProbeOutcome, Runtime};

    let Ok(target) = DockerTarget::resolve(None, None, config) else {
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

pub(crate) fn persist_runtime(
    paths: &WorkspaceLayout,
    backend: ConfiguredBackend,
) -> anyhow::Result<()> {
    let mut file = ConfigFile::load(&paths.config_file)?;
    file.set_system_backend(backend)?;
    file.save()
}

pub(crate) fn mount_point_resolution(
    requested: Option<PathBuf>,
    mode: PromptMode,
) -> anyhow::Result<PathBuf> {
    let default = omnifs_workspace::layout::resolve_mount_point().ok_or_else(|| {
        anyhow!("cannot resolve host mount point: set HOME or OMNIFS_MOUNT_POINT")
    })?;
    let default_display = WorkspaceLayout::display(&default);
    mode.resolve(
        requested,
        || default.clone(),
        "--mount-point <path>",
        || {
            let raw = inquire::Text::new("Mount point")
                .with_default(&default_display)
                .prompt()
                .map_err(crate::ui::from_inquire)?;
            Ok(expand_tilde_path(raw.trim()))
        },
    )
}

#[allow(clippy::too_many_lines)] // linear ledger narration reads best inline
pub(crate) async fn configure_mount(
    args: InitArgs,
    workspace: &Workspace,
    standalone: bool,
) -> anyhow::Result<MountInitOutcome> {
    ensure_express_defaults(workspace)?;
    let mut plan = spec_creation(&args, workspace)?;
    // Standalone `omnifs init` opens its own provider block; setup opens the
    // block in its per-provider loop before calling in.
    if standalone {
        anstream::eprintln!("{}", crate::ui::rule(plan.manifest.id.as_str()));
        crate::commands::init::render_consent_block(&plan.manifest);
    }
    let daemon_report = persist_mount_spec(workspace, &plan).await?;
    auth(&args, workspace, &mut plan).await?;

    anstream::eprintln!("{}", crate::ui::ok("mount ready", plan.mount_name.as_str()));
    match &daemon_report {
        Some(report) if report.failure.is_none() => {
            anstream::eprintln!("{}", crate::ui::note("applied to the running daemon"));
        },
        Some(report) => {
            let reason = report
                .failure
                .as_ref()
                .map_or("unknown error", |failure| failure.reason.as_str());
            anstream::eprintln!("{}", crate::ui::warn_row("daemon", reason));
            anstream::eprintln!(
                "{}",
                crate::ui::note("saved locally; run `omnifs up` to restart with the new mount")
            );
        },
        None if standalone => {
            anstream::eprintln!("{}", crate::ui::note("run `omnifs up` to start serving it"));
        },
        // Inside setup the launch section owns "start the daemon" messaging;
        // repeating it under every provider block is noise.
        None => {},
    }

    if standalone {
        let running = workspace.daemon().ready().await;
        anstream::eprintln!();
        if running {
            let path = browse_path(plan.mount_name.as_str());
            anstream::eprintln!(
                "{}",
                crate::ui::hint(&format!("ls {}", path.display()), "browse it")
            );
        } else {
            anstream::eprintln!("{}", crate::ui::hint("omnifs up", "start serving"));
        }
    }

    crate::telemetry::maybe_print_health_nudge(workspace).await;

    Ok(MountInitOutcome {
        mount_name: plan.mount_name.to_string(),
    })
}

/// Init is interactive only with a real terminal on both ends and without
/// `--no-input`. A piped stdin is non-interactive even without the flag, so
/// prompt sites bail cleanly (naming the satisfying flags) instead of hitting
/// inquire's raw "not a terminal" error. Mirrors setup's terminal derivation.
fn init_interactive(args: &InitArgs) -> bool {
    use std::io::IsTerminal;
    !args.no_input && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[allow(clippy::too_many_lines)] // one linear spec-assembly path
pub(crate) fn spec_creation(
    args: &InitArgs,
    workspace: &Workspace,
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
    )?;

    let (provider, manifest) = crate::catalog::find_installed(&installed, &provider_name)
        .ok_or_else(|| {
            anyhow!(
                "provider `{provider_name}` not found; available: {}",
                provider_selection.provider_names().join(", ")
            )
        })
        .with_hint("Run `omnifs providers ls` to list available providers (or `omnifs init` with no args to pick one interactively)")
        .with_hint(format!(
            "Or run `omnifs providers add <wasm-or-dir>` to install provider artifacts into {}",
            paths.providers_dir.display()
        ))?;
    let reference = provider.reference();
    let existing_mount = mounts.iter().find(|mount| mount.name == mount_name);
    let upgrade_approval = match existing_mount {
        Some(existing) => approved_upgrade_for_existing_mount(
            catalog,
            existing,
            manifest,
            &provider_name,
            &mount_name,
            interactive,
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
    .resolve()?;
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

pub(crate) async fn auth(
    args: &InitArgs,
    workspace: &Workspace,
    plan: &mut MountInitPlan,
) -> anyhow::Result<()> {
    let Some(auth) = plan.effective_auth.as_ref() else {
        return Ok(());
    };
    let interactive = init_interactive(args);
    if let Some(token) = plan.imported_token.take() {
        crate::commands::init::run_static_token_init(
            &plan.manifest,
            auth,
            token,
            &workspace.layout().credentials_file,
            !args.no_validate,
        )
        .await?;
    } else if auth.is_oauth() {
        // Gate the browser handoff when interactive: a decline is a clean skip,
        // not a failure.
        if interactive && !args.yes {
            let proceed = inquire::Confirm::new(&format!(
                "Sign in to {} in your browser now?",
                plan.mount_name
            ))
            .with_default(true)
            .prompt()
            .map_err(crate::ui::from_inquire)?;
            if !proceed {
                anstream::eprintln!(
                    "{}",
                    crate::ui::note(format!(
                        "run `omnifs mounts reauth {}` to sign in later",
                        plan.mount_name
                    ))
                );
                return Err(anyhow!("sign-in skipped")).with_exit_code(ExitCode::AuthRequired);
            }
        }
        crate::auth::login_with_workspace(
            workspace,
            plan.mount_name.as_str(),
            auth.account.as_deref(),
            args.no_browser,
            args.no_input,
            &args.scopes,
        )
        .await
        .inspect_err(|_| {
            anstream::eprintln!(
                "{}",
                crate::ui::note(format!(
                    "login did not complete; run `omnifs mounts reauth {}` to finish",
                    plan.mount_name
                ))
            );
        })?;
        anstream::eprintln!("{}", crate::ui::ok("signed in", "done"));
    } else {
        if interactive && let Ok(scheme) = auth.static_token_scheme(&plan.manifest) {
            let guidance = plan
                .manifest
                .auth
                .as_ref()
                .map(|auth| auth.guidance_for(&scheme.key))
                .unwrap_or_default();
            if let Some(url) = &scheme.creation_url {
                anstream::eprintln!("{}", crate::ui::note(format!("create a token at {url}")));
            }
            for step in &guidance.setup_steps {
                anstream::eprintln!("{}", crate::ui::note(step));
            }
            if let Some(url) = &guidance.docs_url {
                anstream::eprintln!("{}", crate::ui::note(url));
            }
        }
        let source = TokenSource::resolve(
            args.token.as_deref(),
            args.token_env.as_deref(),
            interactive,
        )?;
        let token = source.read()?;
        crate::commands::init::run_static_token_init(
            &plan.manifest,
            auth,
            token,
            &workspace.layout().credentials_file,
            !args.no_validate,
        )
        .await?;
    }
    Ok(())
}

pub(crate) fn verify_first_read(
    outcome: &LaunchOutcome,
    mount_name: &str,
) -> anyhow::Result<FirstRead> {
    match outcome {
        LaunchOutcome::Native { mount_point } => {
            let mount_point = mount_point
                .clone()
                .or_else(omnifs_workspace::layout::resolve_mount_point)
                .ok_or_else(|| anyhow!("cannot resolve mount point for first read"))?;
            run_host_ls(&mount_point.join(mount_name))
        },
        LaunchOutcome::Docker { target } => {
            run_docker_ls(target.container_name().as_str(), mount_name)
        },
    }
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
                "daemon did not become ready within {}",
                format_duration(timeout)
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

fn ensure_express_defaults(workspace: &Workspace) -> anyhow::Result<()> {
    let config = workspace.config()?;
    if config.system.runtime.is_some() {
        return Ok(());
    }
    let backend = default_runtime(HostOs::detect());
    persist_runtime(workspace.layout(), backend)?;
    anstream::eprintln!(
        "{}",
        crate::ui::note(format!(
            "using the {} runtime (`omnifs setup` to change)",
            runtime_word(backend)
        ))
    );
    Ok(())
}

fn selected_auth(
    args: &InitArgs,
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
) -> anyhow::Result<Option<omnifs_api::MountReport>> {
    let report = match if plan.existing_mount {
        workspace
            .daemon()
            .update_mount_if_ready(&plan.spec, plan.upgrade_approval.as_ref())
            .await
    } else {
        workspace.daemon().create_mount_if_ready(&plan.spec).await
    } {
        Ok(Some(report)) => Some(report),
        Ok(None) => {
            workspace.put_mount(&plan.spec)?;
            None
        },
        Err(error) => {
            workspace.put_mount(&plan.spec)?;
            anstream::eprintln!(
                "{}",
                crate::ui::warn_row(
                    "daemon",
                    format!("could not save mount `{}`: {error:#}", plan.mount_name)
                )
            );
            anstream::eprintln!(
                "{}",
                crate::ui::note("saved locally; run `omnifs up` to restart with the new mount")
            );
            None
        },
    };
    // `Wrote <path>` collapses to a single dim continuation, printed once.
    anstream::eprintln!(
        "{}",
        crate::ui::note(format!(
            "wrote {}",
            WorkspaceLayout::display(&plan.mount_path)
        ))
    );
    Ok(report)
}

fn apply_mount_overrides(
    args: &InitArgs,
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
    first_read_from_output(format!("ls {}", path.display()), &output)
}

fn run_docker_ls(container: &str, mount_name: &str) -> anyhow::Result<FirstRead> {
    let path = format!("{GUEST_MOUNT}/{mount_name}");
    let output = Command::new("docker")
        .args(["exec", container, "ls", &path])
        .output()
        .with_context(|| format!("run docker exec {container} ls {path}"))?;
    first_read_from_output(format!("docker exec {container} ls {path}"), &output)
}

fn first_read_from_output(
    command: String,
    output: &std::process::Output,
) -> anyhow::Result<FirstRead> {
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

fn expand_tilde_path(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(raw)
}

fn format_duration(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

/// The runtime picker rows, ordered and defaulted per host OS. The default row
/// leads with `default_on: true` and a "recommended" summary. On Linux/WSL that
/// is native kernel FUSE (no container layer); on macOS it is Docker, while
/// native there stays the experimental loopback-NFS opt-in.
fn runtime_rows(os: HostOs) -> Vec<PickerRow> {
    use crate::ui::picker::{Detail, PanelLine, PanelRole};

    let plain = |text: &str| PanelLine {
        text: text.to_string(),
        role: PanelRole::Plain,
    };
    let dim = |text: &str| PanelLine {
        text: text.to_string(),
        role: PanelRole::Dim,
    };
    let head = |text: &str| PanelLine {
        text: text.to_string(),
        role: PanelRole::Head,
    };

    let docker_detail = || Detail {
        lines: vec![
            head("docker, Linux FUSE in a container"),
            plain("isolated from your host; no kernel extensions or admin setup"),
            plain(&format!(
                "files appear at {GUEST_MOUNT} inside the container"
            )),
            dim("requires Docker running; browse with omnifs shell"),
        ],
    };

    if os == HostOs::MacOs {
        // macOS: Docker leads as the recommended default; native loopback NFS is
        // the experimental opt-in until it earns the default.
        let docker = PickerRow {
            id: "docker".to_string(),
            summary: "recommended — isolated, no kernel dependencies".to_string(),
            cap_tags: Vec::new(),
            auth_tag: None,
            default_on: true,
            detail: docker_detail(),
        };
        let native = PickerRow {
            id: "native".to_string(),
            summary: "experimental NFS loopback".to_string(),
            cap_tags: Vec::new(),
            auth_tag: None,
            default_on: false,
            detail: Detail {
                lines: vec![
                    head("native, loopback NFS on macOS"),
                    plain("files appear directly under your chosen mount point"),
                    dim("experimental: NFSv4 loopback, read-only"),
                    dim("no Docker needed"),
                ],
            },
        };
        return vec![docker, native];
    }

    // Linux/WSL: native kernel FUSE leads as the recommended default; Docker
    // follows as the isolated, dependency-declaring alternative.
    let native = PickerRow {
        id: "native".to_string(),
        summary: "recommended — host kernel FUSE, no container layer".to_string(),
        cap_tags: Vec::new(),
        auth_tag: None,
        default_on: true,
        detail: Detail {
            lines: vec![
                head("native, host kernel FUSE"),
                plain("files appear directly under your chosen mount point"),
                plain("uses your kernel's FUSE; no container layer"),
                dim("needs /dev/fuse access"),
            ],
        },
    };
    let docker = PickerRow {
        id: "docker".to_string(),
        summary: "isolated, no kernel dependencies".to_string(),
        cap_tags: Vec::new(),
        auth_tag: None,
        default_on: false,
        detail: docker_detail(),
    };
    vec![native, docker]
}

fn approved_upgrade_for_existing_mount(
    catalog: &Catalog,
    existing: &MountConfig,
    candidate_manifest: &ProviderManifest,
    provider_name: &str,
    mount_name: &MountName,
    interactive: bool,
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
            "`omnifs init --no-input` cannot approve provider upgrade changes for existing mount `{mount_name}`"
        );
    }

    anstream::eprintln!();
    anstream::eprintln!("{}", crate::ui::rule(provider_name));
    anstream::eprintln!("  {provider_name} now requests different access:");
    for change in crate::upgrade::describe_upgrade_plan(&plan) {
        anstream::eprintln!("{}", crate::ui::note(change));
    }
    let approved = inquire::Confirm::new("Approve this provider upgrade?")
        .with_default(false)
        .prompt()
        .map_err(crate::ui::from_inquire)?;
    if !approved {
        anyhow::bail!("aborted");
    }
    Ok(Some(plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_runtime_is_native_on_linux_docker_on_macos() {
        assert_eq!(default_runtime(HostOs::MacOs), ConfiguredBackend::Docker);
        assert_eq!(
            default_runtime(HostOs::LinuxNative),
            ConfiguredBackend::Native
        );
        assert_eq!(default_runtime(HostOs::LinuxWsl), ConfiguredBackend::Native);
    }

    #[test]
    fn runtime_rows_lead_with_the_per_os_default() {
        // The picker's first row is what the single-select cursor lands on, so
        // the default runtime must lead and carry the recommended summary.
        let mac = runtime_rows(HostOs::MacOs);
        assert_eq!(mac[0].id, "docker");
        assert!(mac[0].default_on);
        assert!(mac[0].summary.contains("recommended"));
        assert_eq!(mac[1].id, "native");
        assert!(!mac[1].default_on);

        for os in [HostOs::LinuxNative, HostOs::LinuxWsl] {
            let rows = runtime_rows(os);
            assert_eq!(rows[0].id, "native");
            assert!(rows[0].default_on);
            assert!(rows[0].summary.contains("recommended"));
            assert_eq!(rows[1].id, "docker");
            assert!(!rows[1].default_on);
            // Docker drops the "recommended" claim on Linux/WSL.
            assert!(!rows[1].summary.contains("recommended"));
        }
    }
}
