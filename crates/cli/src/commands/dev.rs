//! `omnifs dev` — contributor sandbox launcher.
//!
//! Brings up the full local integration sandbox: builds the canonical
//! omnifs image (always), reuses host credentials (`gh` token, ssh agent),
//! exposes the Docker socket and a Chinook `SQLite` fixture, and starts
//! the container with built-in dev mounts embedded in the CLI.
//! Contributors-only; requires a source checkout.

use anyhow::Context;
use clap::Args;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::app_context::AppContext;
use crate::dev_mounts;
use crate::dev_support::{DevImageTag, WorkspaceRoot, capture_gh_token};
use crate::paths::PathOverrides;
use crate::runtime::{ContainerExtras, Runtime};
use crate::session::{
    CONTAINER_NAME, ENV_CONTAINER_NAME, HOST_FUSE_MOUNT, Session, env_string, open_store,
    set_private_dir, write_secret,
};

const CHINOOK_URL: &str = "https://raw.githubusercontent.com/lerocha/chinook-database/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite";
const GUEST_TOKEN_PATH: &str = "/run/secrets/github_token";
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

        confirm_gh_token_capture(self.yes)?;

        let gh_token = capture_gh_token()?;

        let secrets_dir = workspace.path().join(".secrets");
        let token_path = secrets_dir.join("github_token");
        let db_dir = secrets_dir.join("db");
        let db_path = db_dir.join("test.db");

        let image = DevImageTag::synthesize(&workspace)?;
        let container_name =
            env_string(ENV_CONTAINER_NAME).unwrap_or_else(|| CONTAINER_NAME.to_string());

        if !self.yes {
            confirm_session(
                &token_path,
                &db_path,
                image.as_str(),
                container_name.as_str(),
            )?;
        }

        fs::create_dir_all(&secrets_dir)
            .with_context(|| format!("create {}", secrets_dir.display()))?;
        set_private_dir(&secrets_dir)?;
        write_secret(&token_path, &gh_token)?;
        ensure_db_fixture(&db_dir, &db_path).await?;

        build_image(workspace.path(), image.as_str())?;

        let ctx = AppContext::resolve(
            PathOverrides::default(),
            Some(container_name.clone()),
            Some(image.as_str().to_owned()),
        )?;
        let paths = ctx.paths();
        let runtime = ctx.runtime();

        let session = Session::prepare(runtime.container_name(), &paths.credentials_file)?;
        let mut cleanup = session.cleanup_on_drop();
        anstream::println!("Preparing session at {}", session.root().display());
        anstream::println!("Installing built-in dev mount configs");
        let configs = dev_mounts::install(&session)?;

        let store = open_store(&paths.credentials_file, true);
        anstream::println!("Materializing mount configs and credentials");
        session.populate(&configs, ctx.catalog(), store.as_ref())?;
        anstream::println!("✓ Materialized {} mount(s)", configs.len());

        let rt = Runtime::connect_ready(runtime, "omnifs dev").await?;

        let extras = ContainerExtras {
            binds: vec![
                format!("{}:{GUEST_TOKEN_PATH}:ro", token_path.display()),
                format!("{}:{GUEST_DB_DIR}:ro", db_dir.display()),
            ],
            ..Default::default()
        };
        rt.launch_container(&session, extras).await?;

        rt.wait_for_fuse_mount().await?;
        rt.verify_status(&configs).await?;
        cleanup.disarm();
        anstream::println!(
            "✓ {HOST_FUSE_MOUNT} is mounted inside `{}`",
            runtime.container_name()
        );
        anstream::println!();
        anstream::println!("Attach a shell with: omnifs shell");
        Ok(())
    }
}

fn confirm_gh_token_capture(skip_confirm: bool) -> anyhow::Result<()> {
    if skip_confirm {
        return Ok(());
    }
    anstream::println!();
    anstream::println!("{}", crate::style::bold("GitHub token"));
    anstream::println!("  omnifs dev will run `gh auth token` to read your GitHub CLI credential.");
    anstream::println!("  The token is written to `.secrets/github_token` and mounted read-only");
    anstream::println!("  into the container at `{GUEST_TOKEN_PATH}`.");
    anstream::println!();
    let proceed = inquire::Confirm::new("Run `gh auth token`?")
        .with_default(true)
        .prompt()
        .map_err(|e| anyhow::anyhow!("confirm prompt: {e}"))?;
    if !proceed {
        anyhow::bail!("aborted by user");
    }
    Ok(())
}

fn confirm_session(
    token_path: &Path,
    db_path: &Path,
    image: &str,
    container_name: &str,
) -> anyhow::Result<()> {
    let db_dir = db_path
        .parent()
        .expect("db_path is constructed as <secrets>/db/test.db and always has a parent");
    anstream::println!();
    anstream::println!("{}", crate::style::bold("omnifs dev session"));
    anstream::println!("  Image       {image}");
    anstream::println!("  Container   {container_name}");
    anstream::println!();
    anstream::println!("{}", crate::style::bold("Will write to disk"));
    anstream::println!("  GitHub token  {}", token_path.display());
    if !db_path.exists() {
        anstream::println!(
            "  DB fixture    {} (fetched from chinook)",
            db_path.display()
        );
    }
    anstream::println!();
    anstream::println!("{}", crate::style::bold("Will expose to the container"));
    anstream::println!(
        "  {GUEST_TOKEN_PATH}  (read-only)  ← {}",
        token_path.display()
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
    let status = Command::new("docker")
        .args(["build", "-t", image, "."])
        .current_dir(workspace)
        .status()
        .context("invoke docker build")?;
    if !status.success() {
        anyhow::bail!("docker build failed");
    }
    Ok(())
}
