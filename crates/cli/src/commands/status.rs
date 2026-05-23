//! `omnifs status` verb handler.

use crate::app_context::AppContext;
use crate::presentation::{DetailMode, OutputFormat};
use crate::status::{collect_status, resolve_paths};
use anyhow::Context;
use clap::Args;
use std::io::Write as _;

use crate::session::{self, ENV_CONTAINER_NAME};

#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
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

        print_container_status(self).await
    }
}

fn print_local_status(args: StatusArgs) -> anyhow::Result<()> {
    let ctx = AppContext::resolve_default()?;
    let (paths, mount_point) = resolve_paths(args.mount_point, args.config_dir, args.cache_dir);
    let report = collect_status(ctx.catalog(), paths, mount_point)?;
    match OutputFormat::from_json_flag(args.json) {
        OutputFormat::Json => {
            let payload = report.to_json();
            let serialized = serde_json::to_string(&payload).context("serialize status JSON")?;
            anstream::println!("{serialized}");
        },
        OutputFormat::Text => {
            anstream::print!(
                "{}",
                report.render(DetailMode::from_flag(args.detail).is_detail())
            );
        },
    }
    Ok(())
}

async fn print_container_status(args: StatusArgs) -> anyhow::Result<()> {
    use bollard::Docker;
    use bollard::container::LogOutput;
    use bollard::exec::{CreateExecOptions, StartExecResults};
    use futures_util::StreamExt as _;

    let container_name = resolve_container_name(args.container_name.clone());
    let docker = Docker::connect_with_local_defaults()
        .context("connect to Docker daemon (is it running?)")?;

    let fwd = forwarded_status_args(&args);
    let mut cmd = vec!["omnifs", "status"];
    cmd.extend(fwd.iter().map(String::as_str));

    let exec = docker
        .create_exec(
            &container_name,
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

fn resolve_container_name(explicit: Option<String>) -> String {
    explicit
        .or_else(|| session::env_string(ENV_CONTAINER_NAME))
        .or_else(|| {
            crate::paths::Paths::resolve_with_config(crate::paths::PathOverrides::default())
                .ok()
                .and_then(|(_paths, config)| config.container_name)
        })
        .unwrap_or_else(|| session::CONTAINER_NAME.to_string())
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
