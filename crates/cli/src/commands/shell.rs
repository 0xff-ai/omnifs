//! `omnifs shell` — open a shell in the running container.

use anyhow::Context;
use clap::Args;
use std::process::Command;

use crate::session::{self, ENV_CONTAINER_NAME};

#[derive(Args, Debug, Clone, Default)]
pub struct ShellArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    /// Optional command to run instead of the default `/bin/zsh`.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

impl ShellArgs {
    pub fn run(self) -> anyhow::Result<()> {
        use crate::paths::{PathOverrides, Paths};
        use std::io::IsTerminal as _;

        let (_paths, config) = Paths::resolve_with_config(PathOverrides::default())?;
        let container_name = self
            .container_name
            .or_else(|| session::env_string(ENV_CONTAINER_NAME))
            .or(config.container_name)
            .unwrap_or_else(|| session::CONTAINER_NAME.to_string());
        let mut cmd = Command::new("docker");
        cmd.arg("exec").arg("-i");
        if std::io::stdin().is_terminal() {
            cmd.arg("-t");
        }
        cmd.arg(&container_name);
        if self.command.is_empty() {
            cmd.arg("/bin/zsh");
        } else {
            cmd.args(&self.command);
        }
        let status = cmd.status().context("spawn `docker exec`")?;
        if !status.success() {
            anyhow::bail!("docker exec exited with {status}");
        }
        Ok(())
    }
}
