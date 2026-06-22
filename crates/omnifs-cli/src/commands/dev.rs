//! `omnifs dev` — contributor sandbox launcher.
//!
//! Brings up the full local integration sandbox: builds the canonical omnifs
//! image from the workspace (or runs a pre-built one with `--image`), provisions
//! credentials for the built-in dev mounts from the contributor's host (`gh`
//! CLI, provider env vars) into a dedicated dev home at `~/.omnifs-dev`,
//! exposes the Docker socket and a Chinook `SQLite` fixture, and starts the
//! container. Nothing is written into the source checkout, so a stray `git add`
//! can never leak a token. Contributors-only; requires a source checkout.

use anyhow::Context;
use clap::Args;
use omnifs_creds::FileStore;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::auth::AuthSelection;
use crate::catalog::ProviderTemplates;
use crate::commands::init::{AuthImportDecision, TokenValidationMode, run_static_token_init};
use crate::dev_mounts::{self, DevMountSeed};
use crate::dev_support::{DevImageTag, WorkspaceRoot};
use crate::launch::{LaunchSpec, launch_runtime};
use crate::launch_backend::{DockerTarget, LaunchBackend};
use crate::runtime::ContainerExtras;
use crate::session::{
    CONTAINER_NAME, ENV_CONTAINER_NAME, GUEST_FUSE_MOUNT, MountConfig, env_string, set_private_dir,
};
use omnifs_home::WorkspaceLayout;

const CHINOOK_URL: &str = "https://raw.githubusercontent.com/lerocha/chinook-database/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite";
const GUEST_DB_DIR: &str = "/data";

#[derive(Args, Debug, Clone, Default)]
pub struct DevArgs {
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Run a pre-built image instead of building one from the workspace.
    ///
    /// Skips `docker build` and installs providers from the launcher's
    /// embedded provider bundle. CI uses this to smoke the published image
    /// through the real `omnifs dev` launch path (credential provisioning,
    /// mount push, fixtures, and all).
    #[arg(long)]
    pub image: Option<String>,
}

impl DevArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = WorkspaceRoot::discover()?;
        anstream::println!("Workspace: {}", workspace.path().display());

        // Dedicated dev home under the standard omnifs home. Credentials and
        // fixtures live here, never in the repo checkout (no credential-via-git
        // leak) and never mixed into the user's real `~/.omnifs`.
        let dev_home = crate::dev_support::dev_home_root()?;
        let db_dir = dev_home.join("db");
        let db_path = db_dir.join("test.db");

        // Either build the canonical image from the workspace, or run a
        // pre-built one as-is. The pre-built path is how CI smokes the exact
        // published image through this same launch sequence.
        let image = match &self.image {
            Some(image) => image.clone(),
            None => DevImageTag::synthesize(&workspace)?.as_str().to_owned(),
        };
        let container_name =
            env_string(ENV_CONTAINER_NAME).unwrap_or_else(|| CONTAINER_NAME.to_string());

        if !self.yes {
            confirm_session(&dev_home, &db_path, &image, container_name.as_str())?;
        }

        fs::create_dir_all(&dev_home).with_context(|| format!("create {}", dev_home.display()))?;
        set_private_dir(&dev_home)?;
        ensure_db_fixture(&db_dir, &db_path).await?;

        // In build mode, kick off the dev Kubernetes cluster on a blocking
        // thread so k3s pulling and booting overlaps the image build below. It
        // is best-effort (joined further down): a cluster that fails to come up
        // must not abort the dev session. Skipped with `--image` (the lean
        // pre-built/CI path), which has no use for a local k3s cluster.
        let cluster_task = self.image.is_none().then(|| {
            let workspace = workspace.path().to_path_buf();
            let sock_dir = dev_home.join("k8s");
            tokio::task::spawn_blocking(move || {
                crate::kubernetes_testenv::up(&workspace, &sock_dir)
            })
        });

        if self.image.is_none() {
            build_image(workspace.path(), &image)?;
        }

        let dev_workspace =
            crate::workspace::Workspace::from_layout(WorkspaceLayout::under_root(&dev_home));
        let config = dev_workspace.config()?;
        let paths = dev_workspace.layout();
        let docker_target =
            DockerTarget::resolve(Some(container_name.clone()), Some(image), &config)?;

        // Providers: export freshly from the workspace, or unpack the launcher
        // bundle when running a pre-built image (no workspace build).
        if self.image.is_some() {
            crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;
        } else {
            crate::provider_bundle::install_workspace_bundle(
                workspace.path(),
                &paths.providers_dir,
            )?;
        }

        // Provision each dev mount's credential from the host into the dev-home
        // store, then launch only the mounts we could provision (plus the ones
        // that need no auth). Same detect→validate→store path as `omnifs init`.
        let templates = dev_workspace.catalog().provider_templates()?;
        let mut configs =
            provision_dev_mounts(dev_mounts::seeds()?, &templates, &paths.credentials_file).await?;

        let mut binds = vec![format!("{}:{GUEST_DB_DIR}:ro", db_dir.display())];

        // Join the kubernetes cluster bring-up (build mode only). Best-effort:
        // if it failed, mount everything else without kubernetes and surface
        // the reason rather than failing the command.
        if let Some(cluster_task) = cluster_task {
            match cluster_task.await {
                Ok(Ok((socket_bind, seed))) => match seed.pin(&templates) {
                    Some(spec) => match MountConfig::from_parsed(
                        spec,
                        PathBuf::from("dev cluster testenv mount"),
                    ) {
                        Ok(config) => {
                            binds.push(socket_bind);
                            configs.push(config);
                        },
                        Err(error) => anstream::eprintln!(
                            "warning: dev Kubernetes mount was invalid; mounting without it: {error:#}"
                        ),
                    },
                    None => anstream::eprintln!(
                        "warning: kubernetes provider is not installed; mounting without the dev cluster"
                    ),
                },
                Ok(Err(error)) => {
                    anstream::eprintln!(
                        "warning: dev Kubernetes cluster did not start; mounting without it: {error:#}"
                    );
                },
                Err(join) => {
                    anstream::eprintln!("warning: dev Kubernetes cluster task panicked: {join}");
                },
            }
        }

        write_dev_mounts(&paths.mounts_dir, &configs)?;

        let store = Box::new(FileStore::new(&paths.credentials_file));
        launch_runtime(
            LaunchSpec {
                backend: LaunchBackend::docker(docker_target.clone()),
                paths,
                store,
                verb: "omnifs dev",
                configs,
                extras: ContainerExtras { binds },
            },
            dev_workspace.catalog(),
        )
        .await?;

        anstream::println!(
            "✓ {GUEST_FUSE_MOUNT} is mounted inside `{}`",
            docker_target.container_name()
        );
        anstream::println!();
        anstream::println!("Attach a shell with: omnifs shell");
        Ok(())
    }
}

/// Acquire host credentials for the embedded dev mounts into the dev-home
/// credential store and return the mounts to launch. A mount whose provider
/// needs a credential we can't source from the host (no env var, no `gh`/login)
/// is dropped with a warning rather than aborting the whole sandbox; a mount
/// that needs no auth (the `SQLite` fixture) is always kept.
async fn provision_dev_mounts(
    seeds: Vec<(String, DevMountSeed)>,
    templates: &ProviderTemplates,
    credentials_file: &Path,
) -> anyhow::Result<Vec<MountConfig>> {
    let mut ready = Vec::new();
    for (filename, seed) in seeds {
        let provider_name = seed.provider.as_str().to_owned();
        let Some(template) = templates.by_id(&provider_name) else {
            anstream::eprintln!(
                "  ! dev mount `{}` references uninstalled provider `{provider_name}`; skipping",
                seed.mount
            );
            continue;
        };
        // by_id matched, so pin against the same templates succeeds.
        let Some(spec) = seed.pin(templates) else {
            continue;
        };
        let config = MountConfig::from_parsed(
            spec,
            PathBuf::from(format!("embedded dev mount {filename}")),
        )?;

        let default_auth = AuthSelection::from_provider_default(&template.manifest);
        if default_auth.is_none() {
            // No auth scheme declared (e.g. the SQLite fixture): nothing to provision.
            ready.push(config);
            continue;
        }

        // `interactive = true` enables host-credential detection; `yes = true`
        // auto-accepts the first detected credential without prompting.
        let outcome = AuthImportDecision::new(
            default_auth,
            template.auth_manifest.as_ref(),
            &provider_name,
            true,
            true,
        )
        .resolve()?;

        if let (Some(auth), Some(token)) = (outcome.auth, outcome.token) {
            // Skip validation: the sandbox is best-effort, and a token that a
            // provider's validation endpoint rejects (e.g. a CI Actions token
            // against GitHub's `/user`) may still work for data callouts.
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
                "  ! no host credential found for `{provider_name}`; skipping its dev mount (authenticate it and rerun `omnifs dev`)"
            );
        }
    }
    Ok(ready)
}

fn confirm_session(
    dev_home: &Path,
    db_path: &Path,
    image: &str,
    container_name: &str,
) -> anyhow::Result<()> {
    let db_dir = db_path
        .parent()
        .expect("db_path is constructed as <dev_home>/db/test.db and always has a parent");
    anstream::println!();
    anstream::println!("{}", crate::style::bold("omnifs dev session"));
    anstream::println!("  Image       {image}");
    anstream::println!("  Container   {container_name}");
    anstream::println!("  Dev home    {}", dev_home.display());
    anstream::println!();
    anstream::println!("{}", crate::style::bold("Will provision into the dev home"));
    anstream::println!(
        "  Credentials imported from your `gh` CLI / provider env vars → {}",
        dev_home.join("credentials.json").display()
    );
    if !db_path.exists() {
        anstream::println!(
            "  Chinook DB fixture → {} (fetched once)",
            db_path.display()
        );
    }
    anstream::println!();
    anstream::println!("{}", crate::style::bold("Will expose to the container"));
    anstream::println!(
        "  /root/.omnifs               (dev home, read-write)  ← {}",
        dev_home.display()
    );
    anstream::println!(
        "  {GUEST_DB_DIR}                       (read-only)  ← {}",
        db_dir.display()
    );
    anstream::println!("  /ssh-agent                  (host SSH_AUTH_SOCK forward)");
    anstream::println!("  /var/run/docker.sock        (read-only)");
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

/// Persist the synthesized dev mounts so the in-container daemon reconciles them
/// from `mounts/` on start; no spec is pushed over the control API.
fn write_dev_mounts(mounts_dir: &Path, configs: &[MountConfig]) -> anyhow::Result<()> {
    fs::create_dir_all(mounts_dir).with_context(|| format!("create {}", mounts_dir.display()))?;
    for config in configs {
        let path = mounts_dir.join(format!("{}.json", config.name));
        let json = serde_json::to_vec_pretty(&config.config)
            .with_context(|| format!("serialize dev mount `{}`", config.name))?;
        fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

async fn ensure_db_fixture(db_dir: &Path, db_path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(db_dir).with_context(|| format!("create {}", db_dir.display()))?;
    if fs::metadata(db_path).map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(());
    }
    anstream::println!("Downloading Chinook DB fixture → {}", db_path.display());
    let response = reqwest::get(CHINOOK_URL)
        .await
        .context("fetch chinook fixture")?;
    let bytes = response
        .error_for_status()
        .context("chinook fetch returned error")?
        .bytes()
        .await
        .context("read chinook body")?;
    fs::write(db_path, &bytes).with_context(|| format!("write {}", db_path.display()))?;
    Ok(())
}

fn build_image(workspace: &Path, image: &str) -> anyhow::Result<()> {
    anstream::println!("Building image `{image}` (cached layers reused)");
    // Bake the launcher's `CARGO_PKG_VERSION` into the image so the
    // pre-`docker create` handshake in `runtime.rs` can refuse to
    // launch this image with an older `omnifs` on PATH.
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
