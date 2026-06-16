//! `omnifs dev` — contributor sandbox launcher.
//!
//! Brings up the full local integration sandbox: builds the canonical
//! omnifs image (always), provisions credentials for the built-in dev mounts
//! from the contributor's host (`gh` CLI, provider env vars) into a dedicated
//! dev home under `~/.omnifs/dev`, exposes the Docker socket and a Chinook
//! `SQLite` fixture, and starts the container. Nothing is written into the
//! source checkout, so a stray `git add` can never leak a token.
//! Contributors-only; requires a source checkout.

use anyhow::Context;
use clap::Args;
use omnifs_creds::FileStore;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::app_context::AppContext;
use crate::auth::AuthSelection;
use crate::catalog::ProviderTemplate;
use crate::commands::init::{AuthImportDecision, run_static_token_init};
use crate::dev_mounts;
use crate::dev_support::{DevImageTag, WorkspaceRoot};
use crate::launch::{LaunchSpec, launch_runtime};
use crate::paths::{PathOverrides, Paths};
use crate::runtime::ContainerExtras;
use crate::session::{
    CONTAINER_NAME, ENV_CONTAINER_NAME, GUEST_FUSE_MOUNT, MountConfig, env_string, set_private_dir,
};

const CHINOOK_URL: &str = "https://raw.githubusercontent.com/lerocha/chinook-database/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite";
const GUEST_DB_DIR: &str = "/data";

#[derive(Args, Debug, Clone, Default)]
pub struct DevArgs {
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

impl DevArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = WorkspaceRoot::discover()?;
        anstream::println!("Workspace: {}", workspace.path().display());

        // Dedicated dev home under the standard omnifs home. Credentials and
        // fixtures live here, never in the repo checkout (no credential-via-git
        // leak) and never mixed into the user's real `~/.omnifs`.
        let dev_home = Paths::resolve(PathOverrides::default())?
            .config_dir
            .join("dev");
        let db_dir = dev_home.join("db");
        let db_path = db_dir.join("test.db");

        let image = DevImageTag::synthesize(&workspace)?;
        let container_name =
            env_string(ENV_CONTAINER_NAME).unwrap_or_else(|| CONTAINER_NAME.to_string());

        if !self.yes {
            confirm_session(&dev_home, &db_path, image.as_str(), container_name.as_str())?;
        }

        fs::create_dir_all(&dev_home).with_context(|| format!("create {}", dev_home.display()))?;
        set_private_dir(&dev_home)?;
        ensure_db_fixture(&db_dir, &db_path).await?;

        build_image(workspace.path(), image.as_str())?;

        let ctx = AppContext::resolve(
            PathOverrides {
                config_dir: Some(dev_home.clone()),
                ..PathOverrides::default()
            },
            Some(container_name.clone()),
            Some(image.as_str().to_owned()),
        )?;
        let paths = ctx.paths();
        let runtime = ctx.runtime();

        crate::provider_bundle::install_workspace_bundle(workspace.path(), &paths.providers_dir)?;

        // Provision each dev mount's credential from the host into the dev-home
        // store, then launch only the mounts we could provision (plus the ones
        // that need no auth). Same detect→validate→store path as `omnifs init`.
        let templates = ctx.catalog().provider_templates()?;
        let configs =
            provision_dev_mounts(dev_mounts::configs()?, &templates, &paths.credentials_file)
                .await?;

        let store = Box::new(FileStore::new(&paths.credentials_file));
        launch_runtime(
            LaunchSpec {
                runtime,
                runtime_home: &paths.config_dir,
                store,
                verb: "omnifs dev",
                configs,
                extras: ContainerExtras {
                    binds: vec![format!("{}:{GUEST_DB_DIR}:ro", db_dir.display())],
                },
            },
            ctx.catalog(),
        )
        .await?;

        anstream::println!(
            "✓ {GUEST_FUSE_MOUNT} is mounted inside `{}`",
            runtime.container_name()
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
    configs: Vec<MountConfig>,
    templates: &BTreeMap<String, ProviderTemplate>,
    credentials_file: &Path,
) -> anyhow::Result<Vec<MountConfig>> {
    let mut ready = Vec::new();
    for config in configs {
        let provider_file = config.config.provider.clone();
        let Some((provider_id, template)) = templates
            .iter()
            .find(|(_, template)| template.manifest.provider == provider_file)
        else {
            anstream::eprintln!(
                "  ! mount `{}` references unknown provider `{provider_file}`; skipping",
                config.name
            );
            continue;
        };

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
            provider_id,
            true,
            true,
        )
        .resolve()?;

        if let (Some(auth), Some(token)) = (outcome.auth, outcome.token) {
            run_static_token_init(&template.manifest, &auth, token, credentials_file).await?;
            ready.push(config);
        } else {
            anstream::eprintln!(
                "  ! no host credential found for `{provider_id}`; skipping its dev mount (authenticate it and rerun `omnifs dev`)"
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
