//! `omnifs dev` — contributor sandbox launcher.
//!
//! Brings up the full local integration sandbox: reuses host credentials
//! (`gh` token, ssh agent), materializes the Chinook `SQLite` fixture, installs
//! built-in dev mounts embedded in the CLI, and starts the selected runtime.
//! Contributors-only; requires a source checkout.

use anyhow::Context;
use clap::Args;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::app_context::AppContext;
use crate::dev_mounts;
use crate::dev_support::{DevImageTag, WorkspaceRoot, capture_gh_token};
use crate::native_runtime;
use crate::paths::PathOverrides;
use crate::runtime::{ContainerExtras, GUEST_INSPECTOR_PORT};
use crate::runtime_mode::RuntimeMode;
use crate::runtime_target::RuntimeTarget;
use crate::session::{
    CONTAINER_NAME, CredsBackend, ENV_CONTAINER_NAME, HOST_FUSE_MOUNT, Session, env_string,
    set_private_dir, write_secret,
};

const CHINOOK_URL: &str = "https://raw.githubusercontent.com/lerocha/chinook-database/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite";
const GUEST_GITHUB_TOKEN_PATH: &str = "/run/secrets/github/token";
const GUEST_DATA_DIR: &str = "/data";
const DEV_STATE_DIR: &str = ".omnifs-dev";

#[derive(Args, Debug, Clone, Default)]
pub struct DevArgs {
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Runtime launch mode. Defaults to Docker for CI-parity dev sessions.
    #[arg(long, value_enum)]
    pub mode: Option<RuntimeMode>,
    /// Host mount point for native mode.
    #[arg(long)]
    pub mount_point: Option<PathBuf>,
}

impl DevArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = WorkspaceRoot::discover()?;
        anstream::println!("Workspace: {}", workspace.path().display());

        confirm_gh_token_capture(self.yes)?;

        let gh_token = capture_gh_token()?;

        let dev_dir = workspace.path().join(DEV_STATE_DIR);
        let fixtures_dir = dev_dir.join("fixtures");
        let db_dir = fixtures_dir.join("db");
        let db_path = db_dir.join("test.db");

        let image = DevImageTag::synthesize(&workspace)?;
        let container_name =
            env_string(ENV_CONTAINER_NAME).unwrap_or_else(|| CONTAINER_NAME.to_string());
        let initial_ctx = AppContext::resolve_dev(
            PathOverrides::default(),
            Some(container_name.clone()),
            Some(image.as_str().to_owned()),
            self.mode,
            self.mount_point.clone(),
        )?;
        let initial_target = initial_ctx.runtime();

        if !self.yes {
            confirm_session(&db_path, &fixtures_dir, initial_target, image.as_str())?;
        }

        ensure_db_fixture(&db_dir, &db_path).await?;

        let provider_artifacts = workspace.path().join("target/wasm32-wasip2/release");
        match initial_target {
            RuntimeTarget::Docker(_) => build_image(workspace.path(), image.as_str())?,
            RuntimeTarget::Native(_) => build_providers(workspace.path())?,
        }

        let path_overrides = PathOverrides {
            providers_dir: matches!(initial_target, RuntimeTarget::Native(_))
                .then_some(provider_artifacts),
            ..PathOverrides::default()
        };
        let ctx = AppContext::resolve_dev(
            path_overrides,
            Some(container_name.clone()),
            Some(image.as_str().to_owned()),
            self.mode,
            self.mount_point,
        )?;
        let paths = ctx.paths();
        let runtime = ctx.runtime();
        let catalog = ctx.catalog();

        let session = Session::prepare(runtime.session_name(), &paths.credentials_file)?;
        let mut cleanup = session.cleanup_on_drop();
        let token_path = session.secret_file("github", "token");
        if let Some(parent) = token_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
            set_private_dir(parent)?;
        }
        write_secret(&token_path, &gh_token)?;
        anstream::println!("Preparing session at {}", session.root().display());
        anstream::println!("Installing built-in dev mount configs");
        let configs = dev_mounts::install(&session)?;

        // Use the same JSON credential store as the normal CLI path so
        // contributor builds never trigger platform keychain prompts.
        let store = CredsBackend::file(&paths.credentials_file, true);
        anstream::println!("Materializing mount configs and credentials");
        session.populate(&configs, catalog, store.as_ref())?;
        if matches!(runtime, RuntimeTarget::Native(_)) {
            add_native_dev_preopens(&session, &fixtures_dir)?;
        }
        anstream::println!("✓ Materialized {} mount(s)", configs.len());

        match runtime {
            RuntimeTarget::Docker(target) => {
                let rt = target.connect_ready("omnifs dev").await?;
                let extras = ContainerExtras {
                    binds: vec![format!("{}:{GUEST_DATA_DIR}:ro", fixtures_dir.display())],
                    env: vec![format!(
                        "OMNIFS_INSPECTOR_ADDR=0.0.0.0:{GUEST_INSPECTOR_PORT}"
                    )],
                    tcp_ports: vec![GUEST_INSPECTOR_PORT],
                };
                rt.launch_container(&session, extras).await?;

                rt.wait_for_fuse_mount().await?;
                rt.verify_status(&configs).await?;
                cleanup.disarm();
                anstream::println!(
                    "✓ {HOST_FUSE_MOUNT} is mounted inside `{}`",
                    target.container_name()
                );
            },
            RuntimeTarget::Native(target) => {
                native_runtime::launch(paths, target, &session)?;
                cleanup.disarm();
            },
        }
        anstream::println!();
        anstream::println!(
            "Attach a shell with: omnifs shell --mode {}",
            runtime_mode_arg(runtime)
        );
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
    anstream::println!("  The token is materialized only for this launch and mounted read-only");
    anstream::println!("  into the provider sandbox at `{GUEST_GITHUB_TOKEN_PATH}`.");
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
    db_path: &Path,
    fixtures_dir: &Path,
    target: &RuntimeTarget,
    image: &str,
) -> anyhow::Result<()> {
    anstream::println!();
    anstream::println!("{}", crate::style::bold("omnifs dev session"));
    anstream::println!("  Runtime     {}", target.runtime_label());
    if matches!(target, RuntimeTarget::Docker(_)) {
        anstream::println!("  Image       {image}");
    }
    anstream::println!();
    anstream::println!("{}", crate::style::bold("Will write to disk"));
    anstream::println!("  Dev fixtures  {}", fixtures_dir.display());
    if !db_path.exists() {
        anstream::println!(
            "  DB fixture    {} (fetched from chinook)",
            db_path.display()
        );
    }
    anstream::println!();
    anstream::println!("{}", crate::style::bold("Will expose to the runtime"));
    anstream::println!("  {GUEST_GITHUB_TOKEN_PATH}      (read-only)  ← ephemeral launch secret");
    anstream::println!(
        "  {GUEST_DATA_DIR}                         (read-only)  ← {}",
        fixtures_dir.display()
    );
    if matches!(target, RuntimeTarget::Docker(_)) {
        anstream::println!("  /ssh-agent                  (host SSH_AUTH_SOCK forward)");
        anstream::println!("  /var/run/docker.sock        (read-only)");
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

fn build_providers(workspace: &Path) -> anyhow::Result<()> {
    anstream::println!("Building provider WASM artifacts");
    let status = Command::new("just")
        .arg("providers-build")
        .current_dir(workspace)
        .status()
        .context("invoke `just providers-build`")?;
    if !status.success() {
        anyhow::bail!("provider build failed");
    }
    Ok(())
}

fn add_native_dev_preopens(session: &Session, fixtures_dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(session.mounts_dir())
        .with_context(|| format!("read {}", session.mounts_dir().display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut value: Value =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if raw.contains("/run/secrets/") {
            add_preopen(&mut value, session.secrets_dir(), "/run/secrets")?;
        }
        if raw.contains(GUEST_DATA_DIR) {
            add_preopen(&mut value, fixtures_dir, GUEST_DATA_DIR)?;
        }
        let pretty = serde_json::to_string_pretty(&value)
            .with_context(|| format!("serialize {}", path.display()))?;
        fs::write(&path, format!("{pretty}\n"))
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn add_preopen(value: &mut Value, host: &Path, guest: &str) -> anyhow::Result<()> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("mount config is not a JSON object"))?;
    let capabilities = obj
        .entry("capabilities")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let capabilities = capabilities
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("mount config capabilities is not a JSON object"))?;
    let preopens = capabilities
        .entry("preopened_paths")
        .or_insert_with(|| Value::Array(Vec::new()));
    let preopens = preopens
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("preopened_paths is not an array"))?;

    let host = host.to_string_lossy().to_string();
    if preopens
        .iter()
        .any(|entry| entry.get("guest").and_then(Value::as_str) == Some(guest))
    {
        return Ok(());
    }
    preopens.push(serde_json::json!({
        "host": host,
        "guest": guest,
        "mode": "ro"
    }));
    Ok(())
}

fn runtime_mode_arg(target: &RuntimeTarget) -> &'static str {
    match target {
        RuntimeTarget::Docker(_) => "docker",
        RuntimeTarget::Native(_) => "native",
    }
}
