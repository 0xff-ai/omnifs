//! `omnifs shell` — open a shell in the selected runtime.

use anyhow::Context;
use clap::Args;
use std::path::PathBuf;
use std::process::Command;

use crate::app_context::AppContext;
use crate::paths::PathOverrides;
use crate::runtime_mode::RuntimeMode;
use crate::runtime_target::RuntimeTarget;

#[derive(Args, Debug, Clone, Default)]
pub struct ShellArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    /// Runtime launch mode.
    #[arg(long, value_enum)]
    pub mode: Option<RuntimeMode>,
    /// Host mount point for native mode.
    #[arg(long)]
    pub mount_point: Option<PathBuf>,
    /// Optional command to run instead of the default `/bin/zsh`.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

impl ShellArgs {
    pub fn run(self) -> anyhow::Result<()> {
        use std::io::IsTerminal as _;

        let ctx = AppContext::resolve_with_runtime(
            PathOverrides::default(),
            self.container_name,
            None,
            self.mode,
            self.mount_point,
        )?;
        match ctx.runtime() {
            RuntimeTarget::Docker(target) => {
                let container_name = target.container_name();
                let mut cmd = Command::new("docker");
                cmd.arg("exec").arg("-i");
                if std::io::stdin().is_terminal() {
                    cmd.arg("-t");
                }
                cmd.arg(container_name.as_str());
                if self.command.is_empty() {
                    cmd.arg("/bin/zsh");
                } else {
                    cmd.args(&self.command);
                }
                let status = cmd.status().context("spawn `docker exec`")?;
                if !status.success() {
                    anyhow::bail!("docker exec exited with {status}");
                }
            },
            RuntimeTarget::Native(target) => {
                run_native_shell(&self.command, target.mount_point())?;
            },
        }
        Ok(())
    }
}

fn run_native_shell(command: &[String], mount_point: &std::path::Path) -> anyhow::Result<()> {
    let mut cmd = if command.is_empty() {
        Command::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string()))
    } else {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..]);
        cmd
    };
    let status = cmd
        .current_dir(mount_point)
        .status()
        .with_context(|| format!("spawn shell in {}", mount_point.display()))?;
    if !status.success() {
        anyhow::bail!("native shell exited with {status}");
    }
    Ok(())
}
