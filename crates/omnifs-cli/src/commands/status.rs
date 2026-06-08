//! `omnifs status` verb handler.

use crate::app_context::AppContext;
use crate::catalog::ProviderCatalog;
use crate::native_runtime;
use crate::paths::PathOverrides;
use crate::presentation::OutputFormat;
use crate::runtime_mode::RuntimeMode;
use crate::runtime_target::RuntimeTarget;
use crate::status::{collect_status, resolve_paths};
use anyhow::Context;
use clap::Args;
use std::io::Write as _;
use std::path::PathBuf;

#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    /// Runtime launch mode.
    #[arg(long, value_enum)]
    pub mode: Option<RuntimeMode>,
    #[arg(long)]
    pub mount_point: Option<String>,
    #[arg(long)]
    pub config_dir: Option<String>,
    #[arg(long)]
    pub cache_dir: Option<String>,
    /// Reveal provider runtime detail.
    #[arg(long = "detail")]
    pub detail: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

impl StatusArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        if should_read_local_runtime() {
            return print_local_status(self);
        }

        let ctx = AppContext::resolve_with_runtime(
            PathOverrides::default(),
            self.container_name.clone(),
            None,
            self.mode,
            self.mount_point.as_ref().map(PathBuf::from),
        )?;
        if matches!(ctx.runtime(), RuntimeTarget::Native(_)) {
            return print_native_status(self, &ctx);
        }

        print_container_status(self, &ctx).await
    }
}

fn print_local_status(args: StatusArgs) -> anyhow::Result<()> {
    let ctx = AppContext::resolve_default()?;
    let (paths, mount_point) = resolve_paths(args.mount_point, args.config_dir, args.cache_dir);
    let report = collect_status(ctx.catalog(), paths, mount_point)?;
    match OutputFormat::from(args.json) {
        OutputFormat::Json => {
            let payload = report.to_json();
            let serialized = serde_json::to_string(&payload).context("serialize status JSON")?;
            anstream::println!("{serialized}");
        },
        OutputFormat::Text => {
            anstream::print!("{}", report.render(args.detail));
        },
    }
    Ok(())
}

fn print_native_status(args: StatusArgs, ctx: &AppContext) -> anyhow::Result<()> {
    let paths = ctx.paths();
    let RuntimeTarget::Native(target) = ctx.runtime() else {
        anyhow::bail!("native status requested for docker runtime");
    };
    let states = omnifs_nfs::read_mount_states(&native_runtime::state_dir(paths))?;
    let matching = states
        .iter()
        .find(|state| state.mount_point == target.mount_point());

    let mut status_paths = paths.clone();
    let mount_point = matching
        .map(|state| state.mount_point.clone())
        .or_else(|| args.mount_point.map(PathBuf::from))
        .unwrap_or_else(|| target.mount_point().to_path_buf());
    if let Some(state) = matching {
        if let Some(config_dir) = &state.config_dir {
            status_paths.config_dir.clone_from(config_dir);
            status_paths.mounts_dir = config_dir.join("mounts");
            status_paths.config_file = config_dir.join("config.toml");
        }
        if let Some(cache_dir) = &state.cache_dir {
            status_paths.cache_dir.clone_from(cache_dir);
        }
    }

    let config = crate::config::Config::load(&status_paths.config_file)?;
    let catalog = ProviderCatalog::with_config(
        &status_paths.mounts_dir,
        &status_paths.providers_dir,
        &status_paths.config_file,
        config.mounts,
    );
    let report = collect_status(&catalog, status_paths, mount_point)?;
    match OutputFormat::from(args.json) {
        OutputFormat::Json => {
            let payload = report.to_json();
            let serialized = serde_json::to_string(&payload).context("serialize status JSON")?;
            anstream::println!("{serialized}");
        },
        OutputFormat::Text => {
            anstream::print!("{}", report.render(args.detail));
        },
    }
    Ok(())
}

async fn print_container_status(args: StatusArgs, ctx: &AppContext) -> anyhow::Result<()> {
    use bollard::Docker;
    use bollard::container::LogOutput;
    use bollard::exec::{CreateExecOptions, StartExecResults};
    use futures_util::StreamExt as _;

    let RuntimeTarget::Docker(target) = ctx.runtime() else {
        anyhow::bail!("container status requested for native runtime");
    };
    let container_name = target.container_name();
    let docker = Docker::connect_with_local_defaults()
        .context("connect to Docker daemon (is it running?)")?;

    let fwd = forwarded_status_args(&args);
    let mut cmd = vec!["omnifs", "status"];
    cmd.extend(fwd.iter().map(String::as_str));

    let exec = docker
        .create_exec(
            container_name.as_str(),
            CreateExecOptions::<&str> {
                cmd: Some(cmd),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("run `omnifs status` inside `{container_name}`"))?;

    let StartExecResults::Attached { mut output, .. } = docker
        .start_exec(&exec.id, None)
        .await
        .with_context(|| format!("run `omnifs status` inside `{container_name}`"))?
    else {
        anyhow::bail!("expected attached exec output");
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(msg) = output.next().await {
        match msg.context("read exec output")? {
            LogOutput::StdOut { message } => stdout.extend_from_slice(&message),
            LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
            _ => {},
        }
    }

    std::io::stdout()
        .write_all(&stdout)
        .context("write container status stdout")?;
    std::io::stderr()
        .write_all(&stderr)
        .context("write container status stderr")?;

    let info = docker
        .inspect_exec(&exec.id)
        .await
        .context("inspect exec")?;
    if info.exit_code.unwrap_or(-1) != 0 {
        anyhow::bail!(
            "`omnifs status` inside `{container_name}` exited with {}",
            info.exit_code.unwrap_or(-1)
        );
    }
    Ok(())
}

fn should_read_local_runtime() -> bool {
    std::path::Path::new("/.dockerenv").exists()
}

fn forwarded_status_args(args: &StatusArgs) -> Vec<String> {
    let mut forwarded = Vec::new();
    if let Some(mount_point) = &args.mount_point {
        forwarded.push("--mount-point".to_string());
        forwarded.push(mount_point.clone());
    }
    if let Some(config_dir) = &args.config_dir {
        forwarded.push("--config-dir".to_string());
        forwarded.push(config_dir.clone());
    }
    if let Some(cache_dir) = &args.cache_dir {
        forwarded.push("--cache-dir".to_string());
        forwarded.push(cache_dir.clone());
    }
    if args.detail {
        forwarded.push("--detail".to_string());
    }
    if args.json {
        forwarded.push("--json".to_string());
    }
    forwarded
}
