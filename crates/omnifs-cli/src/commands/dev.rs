//! `omnifs dev` — contributor sandbox launcher.
//!
//! Brings up the full local integration sandbox: builds the canonical
//! omnifs image (always), reuses host credentials (`gh` token, ssh agent),
//! exposes the Docker socket and a Chinook `SQLite` fixture, and starts
//! the container with built-in dev mounts embedded in the CLI.
//! Contributors-only; requires a source checkout.

use anyhow::Context;
use clap::Args;
use omnifs_creds::FileStore;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::app_context::AppContext;
use crate::dev_mounts;
use crate::dev_support::{DevImageTag, WorkspaceRoot, capture_gh_token};
use crate::launch::{LaunchSpec, launch_runtime};
use crate::paths::PathOverrides;
use crate::runtime::ContainerExtras;
use crate::session::{
    CONTAINER_NAME, ENV_CONTAINER_NAME, GUEST_FUSE_MOUNT, env_string, set_private_dir, write_secret,
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

        // Kick off the dev Kubernetes cluster on a blocking thread so k3s
        // pulling and booting overlaps the image build below. It is
        // best-effort (joined further down): a cluster that fails to come up
        // must not abort the whole dev session.
        let cluster_task = {
            let workspace = workspace.path().to_path_buf();
            let sock_dir = secrets_dir.join("k8s");
            tokio::task::spawn_blocking(move || {
                crate::kubernetes_testenv::up(&workspace, &sock_dir)
            })
        };

        build_image(workspace.path(), image.as_str())?;

        let ctx = AppContext::resolve(
            PathOverrides::default(),
            Some(container_name.clone()),
            Some(image.as_str().to_owned()),
        )?;
        let paths = ctx.paths();
        let runtime = ctx.runtime();

        let store = Box::new(FileStore::new(&paths.credentials_file));
        crate::provider_bundle::install_workspace_bundle(workspace.path(), &paths.providers_dir)?;

        let mut configs = dev_mounts::configs()?;
        let mut binds = vec![
            format!("{}:{GUEST_TOKEN_PATH}:ro", token_path.display()),
            format!("{}:{GUEST_DB_DIR}:ro", db_dir.display()),
        ];

        // Join the kubernetes cluster bring-up started before the image build.
        // Best-effort: if it failed, mount everything else without kubernetes
        // and surface the reason rather than failing the command.
        match cluster_task.await {
            Ok(Ok((socket_bind, mount))) => {
                binds.push(socket_bind);
                configs.push(mount);
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

        launch_runtime(
            LaunchSpec {
                runtime,
                runtime_home: &paths.config_dir,
                store,
                verb: "omnifs dev",
                configs,
                extras: ContainerExtras { binds },
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
