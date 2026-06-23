//! Contributor dev session orchestration.

mod discover;
pub(crate) mod fixtures;
mod profiles;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};
use clap::{Args, Subcommand};
use omnifs_creds::FileStore;
use omnifs_home::WorkspaceLayout;
use omnifs_mount::mounts::Spec;
use tokio::signal;

use crate::auth::AuthSelection;
use crate::catalog::ProviderTemplates;
use crate::commands::init::{AuthImportDecision, TokenValidationMode, run_static_token_init};
use crate::dev_support::{DevImageTag, WorkspaceRoot, contributor_layout};
use crate::launch::{LaunchSpec, launch_runtime};
use crate::launch_backend::{DockerTarget, LaunchBackend};
use crate::provider_bundle;
use crate::runtime::ContainerExtras;
use crate::session::{
    CONTAINER_NAME, ENV_CONTAINER_NAME, GUEST_FUSE_MOUNT, MountConfig, env_string, set_private_dir,
};
use crate::workspace::Workspace;

pub(crate) use fixtures::DevSessionRecord;
use fixtures::{DevSessionFixtures, FixtureBinds, FixtureSession};

#[derive(Args, Debug, Clone)]
pub struct DevArgs {
    #[command(subcommand)]
    pub command: Option<DevCommand>,

    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Dev mount profile from `contrib/dev-profiles/` (default: `default`).
    #[arg(long, default_value = "default")]
    pub profile: String,
    /// Run a pre-built image instead of building one from the workspace.
    #[arg(long)]
    pub image: Option<String>,
    /// Bootstrap fixtures and runtime, then return without an interactive shell.
    #[arg(long)]
    pub detach: bool,
    /// Skip the interactive shell after bootstrap (for CI smoke steps).
    #[arg(long)]
    pub no_shell: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DevCommand {
    /// Attach an interactive shell to a detached dev session container.
    Attach,
}

impl DevArgs {
    pub async fn run(self) -> Result<()> {
        if matches!(self.command, Some(DevCommand::Attach)) {
            return attach_session().await;
        }
        self.run_session().await
    }

    async fn run_session(self) -> Result<()> {
        let workspace = WorkspaceRoot::discover()?;
        anstream::println!("Workspace: {}", workspace.path().display());

        let dev_home = contributor_layout()?.config_dir;

        let profile_mounts = profiles::load(workspace.path(), &self.profile)?;
        let discovered = discover::discover(workspace.path())?;
        let discovered_mounts = discover::filter_by_profile(&discovered, &profile_mounts)?;

        let image = match &self.image {
            Some(image) => image.clone(),
            None => DevImageTag::synthesize(&workspace)?.as_str().to_owned(),
        };
        let container_name =
            env_string(ENV_CONTAINER_NAME).unwrap_or_else(|| CONTAINER_NAME.to_string());
        let keep_running = self.detach || self.no_shell;

        if !self.yes {
            confirm_session(
                &dev_home,
                &self.profile,
                &profile_mounts,
                &image,
                container_name.as_str(),
                keep_running,
            )?;
        }

        fs::create_dir_all(&dev_home).with_context(|| format!("create {}", dev_home.display()))?;
        set_private_dir(&dev_home)?;

        let (fixture_session, fixture_binds) =
            FixtureSession::up(&profile_mounts, &dev_home, workspace.path())?;

        match self
            .bootstrap_runtime(
                &workspace,
                &dev_home,
                &image,
                &container_name,
                discovered_mounts,
                fixture_binds,
            )
            .await
        {
            Ok(()) => {
                session_record(
                    &dev_home,
                    workspace.path(),
                    &self.profile,
                    &container_name,
                    &fixture_session,
                )?;

                if keep_running {
                    // Persist live fixture handles until `omnifs down` or foreground teardown.
                    std::mem::forget(fixture_session);
                    anstream::println!("✓ {GUEST_FUSE_MOUNT} is ready inside `{container_name}`");
                    if self.detach {
                        anstream::println!(
                            "Detached. Stop with `omnifs down` or attach with `omnifs dev attach`."
                        );
                    }
                    return Ok(());
                }

                match run_container_shell(&container_name).await {
                    Ok(()) => {},
                    Err(error) => {
                        if let Err(teardown) = DevSessionRecord::teardown_all(&dev_home) {
                            anstream::eprintln!("note: dev session teardown: {teardown:#}");
                        }
                        return Err(error);
                    },
                }

                DevSessionRecord::teardown_all(&dev_home)?;
                Ok(())
            },
            Err(error) => {
                if let Err(teardown) = fixture_session.down() {
                    anstream::eprintln!("note: fixture teardown: {teardown:#}");
                }
                Err(error)
            },
        }
    }

    async fn bootstrap_runtime(
        &self,
        workspace: &WorkspaceRoot,
        dev_home: &Path,
        image: &str,
        container_name: &str,
        discovered_mounts: Vec<discover::DiscoveredMount>,
        fixture_binds: FixtureBinds,
    ) -> Result<()> {
        if self.image.is_none() {
            build_image(workspace.path(), image)?;
        }

        let layout = WorkspaceLayout::under_root(dev_home);
        let workspace_home = Workspace::from_layout(layout.clone());
        let config = workspace_home.config()?;
        let docker_target = DockerTarget::resolve(
            Some(container_name.to_string()),
            Some(image.to_string()),
            &config,
        )?;

        if self.image.is_some() {
            provider_bundle::ensure_providers_installed(&layout.providers_dir)?;
        } else {
            provider_bundle::install_target_bundle(workspace.path(), &layout.providers_dir)?;
        }

        let templates = workspace_home.catalog().provider_templates()?;
        let pinned = pin_dev_mounts(discovered_mounts, &templates)?;
        let configs = provision_dev_mounts(pinned, &templates, &layout.credentials_file).await?;
        write_dev_mounts(&layout.mounts_dir, &configs)?;

        let store = Box::new(FileStore::new(&layout.credentials_file));
        launch_runtime(
            LaunchSpec {
                backend: LaunchBackend::Docker(docker_target),
                paths: &layout,
                store,
                verb: "omnifs dev",
                configs,
                extras: ContainerExtras {
                    binds: fixture_binds.binds,
                },
            },
            workspace_home.catalog(),
        )
        .await?;
        Ok(())
    }
}

async fn attach_session() -> Result<()> {
    let dev_home = contributor_layout()?.config_dir;
    let record = DevSessionRecord::read(&dev_home)?
        .context("no detached dev session; run `omnifs dev --detach` first")?;
    run_container_shell(&record.container_name).await
}

fn session_record(
    dev_home: &Path,
    workspace: &Path,
    profile: &str,
    container_name: &str,
    fixture_session: &FixtureSession,
) -> Result<()> {
    DevSessionRecord {
        workspace: workspace.to_path_buf(),
        profile: profile.to_string(),
        container_name: container_name.to_string(),
        fixtures: DevSessionFixtures {
            k8s: fixture_session.k8s_active(),
            k8s_sock_dir: fixture_session.k8s_sock_dir().map(Path::to_path_buf),
            db_container_id: fixture_session.db_container_id(),
        },
    }
    .write(dev_home)
}

async fn run_container_shell(container_name: &str) -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    anstream::println!(
        "Opening shell at {GUEST_FUSE_MOUNT} inside `{container_name}` (exit or Ctrl+D to end session)"
    );

    let container = container_name.to_string();
    let shell_task = tokio::task::spawn_blocking(move || {
        let status = Command::new("docker")
            .args(["exec", "-it", "-w", GUEST_FUSE_MOUNT, &container, &shell])
            .status()
            .with_context(|| format!("docker exec shell in `{container}`"))?;
        Ok::<std::process::ExitStatus, anyhow::Error>(status)
    });

    tokio::select! {
        result = shell_task => {
            let status = result.context("shell task join")??;
            match status.code() {
                Some(0) | None => Ok(()),
                Some(code) => {
                    anstream::eprintln!("shell exited with status {code}");
                    Ok(())
                }
            }
        }
        _ = signal::ctrl_c() => {
            anstream::println!();
            anstream::println!("Interrupted; tearing down dev session…");
            Ok(())
        }
    }
}

/// Pin each discovered dev mount's provider name to the installed provider's
/// content reference, producing runtime-ready specs. The dev `mount.json`
/// authors `provider` as a bare name; the pinned `ProviderRef` is spliced into
/// the raw JSON so the rest of the spec resolves through `Spec`'s own
/// deserialization. A mount whose provider is not installed is skipped.
fn pin_dev_mounts(
    discovered: Vec<discover::DiscoveredMount>,
    templates: &ProviderTemplates,
) -> Result<Vec<MountConfig>> {
    let mut configs = Vec::new();
    for mount in discovered {
        let Some(template) = templates.by_id(&mount.provider_name) else {
            anstream::eprintln!(
                "  ! dev mount `{}` references provider `{}`, which is not installed; skipping",
                mount.mount_name,
                mount.provider_name
            );
            continue;
        };
        let mut value = mount.raw;
        value["provider"] = serde_json::to_value(&template.reference)
            .context("serialize pinned provider reference")?;
        let mut spec: Spec = serde_json::from_value(value)
            .with_context(|| format!("resolve dev mount `{}`", mount.mount_name))?;
        // Seed explicit grants from the manifest's needs, like `omnifs init`:
        // the manifest never grants at runtime, so a dev mount must carry its
        // own grants or the host's required-capabilities check rejects it at
        // materialize time. An explicit grant authored in the dev `mount.json`
        // wins.
        if spec.capabilities.is_none() && !template.manifest.capabilities.is_empty() {
            spec.capabilities = Some(template.manifest.provider_capabilities());
        }
        let source = PathBuf::from(format!("{}.json", mount.mount_name));
        configs.push(MountConfig::from_parsed(spec, source)?);
    }
    Ok(configs)
}

async fn provision_dev_mounts(
    configs: Vec<MountConfig>,
    templates: &ProviderTemplates,
    credentials_file: &Path,
) -> Result<Vec<MountConfig>> {
    let mut ready = Vec::new();
    for config in configs {
        let provider_name = config.config.provider.meta.name.clone();
        let Some(template) = templates.by_id(provider_name.as_str()) else {
            anstream::eprintln!(
                "  ! mount `{}` references unknown provider `{}`; skipping",
                config.name,
                provider_name.as_str()
            );
            continue;
        };

        let default_auth = AuthSelection::from_provider_default(&template.manifest);
        if default_auth.is_none() {
            ready.push(config);
            continue;
        }

        let outcome = AuthImportDecision::new(
            default_auth,
            template.auth_manifest.as_ref(),
            provider_name.as_str(),
            true,
            true,
        )
        .resolve()?;

        if let (Some(auth), Some(token)) = (outcome.auth, outcome.token) {
            run_static_token_init(
                &template.manifest,
                &auth,
                token,
                credentials_file,
                TokenValidationMode::Skip,
            )
            .await?;
            ready.push(config);
        } else {
            anstream::eprintln!(
                "  ! no host credential found for `{}`; skipping its dev mount (authenticate it and rerun `omnifs dev`)",
                provider_name.as_str()
            );
        }
    }
    Ok(ready)
}

fn confirm_session(
    dev_home: &Path,
    profile: &str,
    mounts: &[String],
    image: &str,
    container_name: &str,
    keep_running: bool,
) -> Result<()> {
    anstream::println!();
    anstream::println!("{}", crate::style::bold("omnifs dev session"));
    anstream::println!("  Profile     {profile}");
    anstream::println!("  Mounts      {}", mounts.join(", "));
    anstream::println!("  Image       {image}");
    anstream::println!("  Container   {container_name}");
    anstream::println!("  Dev home    {}", dev_home.display());
    anstream::println!();
    anstream::println!("{}", crate::style::bold("Session model"));
    if keep_running {
        anstream::println!("  Bootstrap fixtures and runtime, then return (detached / CI mode).");
    } else {
        anstream::println!("  Fixtures → runtime → interactive shell at {GUEST_FUSE_MOUNT}.");
        anstream::println!("  Exit the shell or press Ctrl+C to tear everything down.");
    }
    anstream::println!();
    let proceed = inquire::Confirm::new("Proceed?")
        .with_default(true)
        .prompt()
        .map_err(|e| anyhow::anyhow!("confirm prompt: {e}"))?;
    if !proceed {
        anyhow::bail!("aborted by user");
    }
    Ok(())
}

fn write_dev_mounts(mounts_dir: &Path, configs: &[MountConfig]) -> Result<()> {
    fs::create_dir_all(mounts_dir).with_context(|| format!("create {}", mounts_dir.display()))?;
    for config in configs {
        let path = mounts_dir.join(format!("{}.json", config.name));
        let json = serde_json::to_vec_pretty(&config.config)
            .with_context(|| format!("serialize dev mount `{}`", config.name))?;
        fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn build_image(workspace: &Path, image: &str) -> Result<()> {
    anstream::println!("Building image `{image}` (cached layers reused)");
    let min_launcher = env!("CARGO_PKG_VERSION");
    let status = Command::new("docker")
        .args([
            "build",
            "-t",
            image,
            "--build-arg",
            &format!("OMNIFS_MIN_LAUNCHER_VERSION={min_launcher}"),
            ".",
        ])
        .current_dir(workspace)
        .status()
        .context("invoke docker build")?;
    if !status.success() {
        anyhow::bail!("docker build failed");
    }
    Ok(())
}
