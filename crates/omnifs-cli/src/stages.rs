//! Shared onboarding and lifecycle stages used by `setup`, `init`, and `up`.
//!
//! Commands own narration. This module owns the stage behavior so the guided
//! setup wizard and express `init` lane cannot drift from each other.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, anyhow};
use omnifs_api::DaemonBackend;
use omnifs_caps::{Grants, Limits};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::{Name as MountName, Registry, Spec, UpgradePlan};
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
use crate::launch::{LaunchOutcome, Launcher};
use crate::launch_backend::GUEST_MOUNT;
use crate::mount_config::MountConfig;
use crate::token_source::TokenSource;
use crate::workspace::Workspace;

pub(crate) struct EnvironmentReport {
    pub(crate) os: HostOs,
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

pub(crate) fn environment_check(
    os: HostOs,
    workspace: &Workspace,
) -> anyhow::Result<EnvironmentReport> {
    if os == HostOs::Unsupported {
        anyhow::bail!("omnifs does not yet run on this platform");
    }
    let configured = workspace
        .config()
        .is_ok_and(|config| config.system.runtime.is_some())
        || workspace.mounts().is_ok_and(|mounts| !mounts.is_empty());
    Ok(EnvironmentReport { os, configured })
}

pub(crate) fn default_runtime(os: HostOs) -> ConfiguredBackend {
    match os {
        HostOs::MacOs => ConfiguredBackend::Docker,
        HostOs::LinuxNative | HostOs::LinuxWsl | HostOs::Unsupported => ConfiguredBackend::Native,
    }
}

pub(crate) fn runtime_selection(
    os: HostOs,
    current: Option<ConfiguredBackend>,
    requested: Option<ConfiguredBackend>,
    mode: PromptMode,
) -> anyhow::Result<ConfiguredBackend> {
    if let Some(requested) = requested {
        return Ok(requested);
    }

    let preferred = current.unwrap_or_else(|| default_runtime(os));
    if mode.yes {
        return Ok(preferred);
    }
    if mode.no_input {
        anyhow::bail!(
            "`omnifs setup --no-input` needs --runtime <docker|native>, or use --yes to accept the default runtime"
        );
    }
    if !mode.interactive {
        anyhow::bail!(
            "`omnifs setup` needs an interactive terminal for runtime selection; pass --runtime <docker|native> or --yes"
        );
    }

    let choices = runtime_choices(os);
    let start = choices
        .iter()
        .position(|choice| choice.backend == preferred)
        .unwrap_or(0);
    let chosen = inquire::Select::new("Which runtime should omnifs use?", choices)
        .with_starting_cursor(start)
        .with_help_message("up/down to move, enter to confirm; rerun setup to change it")
        .prompt()
        .map_err(|e| anyhow!("runtime prompt: {e}"))?;
    Ok(chosen.backend)
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
    if let Some(requested) = requested {
        return Ok(requested);
    }
    let default = omnifs_workspace::layout::resolve_mount_point().ok_or_else(|| {
        anyhow!("cannot resolve host mount point: set HOME or OMNIFS_MOUNT_POINT")
    })?;
    if mode.yes {
        return Ok(default);
    }
    if mode.no_input {
        anyhow::bail!(
            "`omnifs setup --no-input` needs --mount-point <path>, or use --yes to accept the default mount point"
        );
    }
    if !mode.interactive {
        anyhow::bail!(
            "`omnifs setup` needs an interactive terminal for mount-point selection; pass --mount-point <path> or --yes"
        );
    }
    let raw = inquire::Text::new("Mount point")
        .with_default(&WorkspaceLayout::display(&default))
        .prompt()
        .map_err(|e| anyhow!("mount point prompt: {e}"))?;
    Ok(expand_tilde_path(raw.trim()))
}

pub(crate) async fn configure_mount(
    args: InitArgs,
    workspace: &Workspace,
) -> anyhow::Result<MountInitOutcome> {
    ensure_express_defaults(workspace)?;
    let mut plan = spec_creation(&args, workspace)?;
    let daemon_report = persist_mount_spec(workspace, &plan).await?;
    auth(&args, workspace, &mut plan).await?;

    anstream::eprintln!();
    anstream::eprintln!("Mount `{}` is ready.", plan.mount_name);

    match &daemon_report {
        Some(report) if report.failure.is_none() => {
            anstream::eprintln!("Applied to the running daemon.");
        },
        Some(report) => {
            let reason = report
                .failure
                .as_ref()
                .map_or("unknown error", |failure| failure.reason.as_str());
            anstream::eprintln!(
                "Mount config saved, but loading it into the running daemon failed: {reason}"
            );
            anstream::eprintln!("Run `omnifs up` to restart with the new mount.");
        },
        None => anstream::eprintln!("Run `omnifs up` to start it."),
    }

    if daemon_report
        .as_ref()
        .is_some_and(|report| report.failure.is_none())
        && let Ok(read) = verify_first_read_from_running(workspace, plan.mount_name.as_str()).await
    {
        anstream::eprintln!("Verified first read with `{}`.", read.command);
    }
    let try_path = browse_path(plan.mount_name.as_str());
    anstream::eprintln!("try: ls {}", try_path.display());
    crate::telemetry::maybe_print_health_nudge(workspace).await;

    Ok(MountInitOutcome {
        mount_name: plan.mount_name.to_string(),
    })
}

pub(crate) fn spec_creation(
    args: &InitArgs,
    workspace: &Workspace,
) -> anyhow::Result<MountInitPlan> {
    let paths = workspace.layout();
    crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;
    let interactive = !args.no_input;
    let catalog = workspace.catalog();
    let mounts = workspace.mounts()?;
    let installed = crate::catalog::installed_providers(catalog)?;
    if installed.is_empty() {
        anyhow::bail!("no built-in or disk providers are available");
    }

    let provider_selection = ProviderSelection::new(&mounts, &installed);
    let (provider_name, mount_name) = provider_selection.resolve(
        args.provider.as_deref(),
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
        .with_hint("Run `omnifs init` with no args to see available providers")
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
    if interactive {
        crate::commands::init::print_capability_justifications(manifest);
    }
    if args.no_input && default_auth.as_ref().is_some_and(AuthSelection::is_oauth) {
        return Err(anyhow!(
            "`omnifs init --no-input` cannot complete OAuth for `{provider_name}`; pass --token-env VAR with --scheme <static-token-scheme>, pass --no-auth, or run interactively"
        ))
        .with_exit_code(ExitCode::AuthRequired);
    }

    let creator = MountSpecCreator::new(&reference, &mount_name, manifest);
    if args.no_input && creator.requires_prompt() && args.config_json.is_none() {
        anyhow::bail!(
            "`omnifs init --no-input` cannot complete provider config prompts for `{provider_name}`; pass --config-json <json>"
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

    let import_outcome = AuthImportDecision::new(
        default_auth,
        auth_manifest.as_ref(),
        &provider_name,
        interactive,
        args.yes,
    )
    .resolve()?;
    let ImportOutcome { auth, token } = import_outcome;

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
        anstream::eprintln!("Starting OAuth login for `{}` ...", plan.mount_name);
        crate::auth::login_with_workspace(
            workspace,
            plan.mount_name.as_str(),
            auth.account.as_deref(),
            args.no_browser,
            &args.scopes,
        )
        .await
        .inspect_err(|_| {
            anstream::eprintln!(
                "Mount `{}` was created, but login did not complete. Run `omnifs mounts reauth {}` to finish.",
                plan.mount_name,
                plan.mount_name
            );
        })?;
    } else {
        if !args.no_input
            && let Ok(scheme) = auth.static_token_scheme(&plan.manifest)
        {
            let guidance = plan
                .manifest
                .auth
                .as_ref()
                .map(|auth| auth.guidance_for(&scheme.key))
                .unwrap_or_default();
            anstream::eprintln!();
            anstream::eprintln!("Authenticating `{}` with a static token:", plan.mount_name);
            crate::auth::explain::render_static_token_intro(
                scheme.creation_url.as_deref(),
                &guidance,
            );
        }
        let source = TokenSource::resolve(
            args.token.as_deref(),
            args.token_env.as_deref(),
            !args.no_input,
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

pub(crate) async fn launch(
    workspace: &Workspace,
    runtime: Option<ConfiguredBackend>,
    verb: &'static str,
) -> anyhow::Result<LaunchOutcome> {
    Launcher::new(workspace, verb)
        .with_runtime_override(runtime)
        .launch()
        .await
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

pub(crate) async fn verify_first_read_from_running(
    workspace: &Workspace,
    mount_name: &str,
) -> anyhow::Result<FirstRead> {
    let status = workspace.daemon().status().await?;
    match status.backend {
        DaemonBackend::Native { .. } => run_host_ls(&status.mount_point.join(mount_name)),
        DaemonBackend::Docker { container_name, .. } => run_docker_ls(&container_name, mount_name),
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
    let os = HostOs::detect();
    let backend = default_runtime(os);
    persist_runtime(workspace.layout(), backend)?;
    anstream::eprintln!(
        "omnifs is not set up yet - using defaults ({} runtime on {}).",
        runtime_word(backend),
        os.name()
    );
    anstream::eprintln!("For the guided tour, run `omnifs setup`.");
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
    let paths = workspace.layout();
    let report = match if plan.existing_mount {
        workspace
            .daemon()
            .update_mount_if_ready(&plan.spec, plan.upgrade_approval.as_ref())
            .await
    } else {
        workspace.daemon().create_mount_if_ready(&plan.spec).await
    } {
        Ok(Some(report)) => {
            anstream::eprintln!("Wrote {}", WorkspaceLayout::display(&plan.mount_path));
            Some(report)
        },
        Ok(None) => {
            Registry::load(&paths.mounts_dir)?.put(&plan.spec)?;
            anstream::eprintln!("Wrote {}", WorkspaceLayout::display(&plan.mount_path));
            None
        },
        Err(error) => {
            anstream::eprintln!(
                "Running daemon could not save mount `{}`: {error:#}",
                plan.mount_name
            );
            anstream::eprintln!("Falling back to a local mount config write.");
            Registry::load(&paths.mounts_dir)?.put(&plan.spec)?;
            anstream::eprintln!("Wrote {}", WorkspaceLayout::display(&plan.mount_path));
            None
        },
    };
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

fn runtime_word(backend: ConfiguredBackend) -> &'static str {
    match backend {
        ConfiguredBackend::Docker => "Docker",
        ConfiguredBackend::Native => "native",
    }
}

struct RuntimeChoice {
    backend: ConfiguredBackend,
    label: &'static str,
}

impl fmt::Display for RuntimeChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

fn runtime_choices(os: HostOs) -> Vec<RuntimeChoice> {
    let docker = RuntimeChoice {
        backend: ConfiguredBackend::Docker,
        label: "docker - Linux FUSE inside a container",
    };
    let native = RuntimeChoice {
        backend: ConfiguredBackend::Native,
        label: match os {
            HostOs::MacOs => "native - host loopback NFS (experimental)",
            _ => "native - host kernel FUSE",
        },
    };
    match default_runtime(os) {
        ConfiguredBackend::Docker => vec![docker, native],
        ConfiguredBackend::Native => vec![native, docker],
    }
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
            "mount `{mount_name}` pinned provider artifact {id} is missing; cannot compute an upgrade approval",
            id = existing.config.provider.id,
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

    anstream::println!();
    anstream::println!(
        "Mount `{mount_name}` already exists. `{provider_name}` changed its provider surface:"
    );
    for change in crate::upgrade::describe_upgrade_changes(&plan) {
        anstream::println!("  - {change}");
    }
    let approved = inquire::Confirm::new("Approve this provider upgrade?")
        .with_default(false)
        .prompt()
        .map_err(|error| anyhow!("confirm prompt: {error}"))?;
    if !approved {
        anyhow::bail!("aborted");
    }
    Ok(Some(plan))
}
