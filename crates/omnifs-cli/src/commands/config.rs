//! `omnifs config` — inspect and update global CLI config.

use clap::{Args, Subcommand};
use std::path::PathBuf;

use crate::config::ConfigFile;
use crate::paths::{PathOverrides, Paths};
use crate::runtime_mode::RuntimeMode;

#[derive(Args, Debug, Clone)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCommand {
    /// Inspect or update runtime defaults.
    Runtime(RuntimeConfigArgs),
}

#[derive(Args, Debug, Clone, Default)]
pub struct RuntimeConfigArgs {
    /// Persist the default runtime mode.
    #[arg(long, value_enum)]
    pub mode: Option<RuntimeMode>,
    /// Persist the default native mount point.
    #[arg(long)]
    pub mount_point: Option<PathBuf>,
    /// Remove the persisted native mount point.
    #[arg(long)]
    pub clear_mount_point: bool,
}

impl ConfigArgs {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            ConfigCommand::Runtime(args) => args.run(),
        }
    }
}

impl RuntimeConfigArgs {
    fn run(self) -> anyhow::Result<()> {
        let (paths, config) = Paths::resolve_with_config(PathOverrides::default())?;
        if self.mode.is_none() && self.mount_point.is_none() && !self.clear_mount_point {
            anstream::println!("Config file: {}", Paths::display(&paths.config_file));
            anstream::println!(
                "Runtime mode: {}",
                config.runtime.mode.map_or("auto", RuntimeMode::as_str)
            );
            anstream::println!(
                "Native mount: {}",
                config
                    .runtime
                    .mount_point
                    .as_deref()
                    .map_or_else(|| "~/OmniFS".to_string(), Paths::display)
            );
            return Ok(());
        }

        let mut file = ConfigFile::load(&paths.config_file)?;
        file.set_runtime(self.mode, self.mount_point, self.clear_mount_point)?;
        file.save()?;
        anstream::println!("✓ Updated {}", Paths::display(&paths.config_file));
        Ok(())
    }
}
