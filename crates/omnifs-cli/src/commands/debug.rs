#![allow(clippy::disallowed_macros)] // migrates in wave 5 (cli-redesign)
//! Hidden debug commands for inspecting generated provider metadata.

use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct DebugArgs {
    #[command(subcommand)]
    pub command: DebugCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DebugCommand {
    /// Print the normalized mount graph for a provider .wasm.
    MountTree {
        path: PathBuf,
        #[arg(long)]
        tree: bool,
        #[arg(long)]
        paths: bool,
        #[arg(long)]
        by_type: bool,
    },
}

impl DebugArgs {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            DebugCommand::MountTree {
                path,
                tree,
                paths,
                by_type,
            } => {
                let views = crate::mount_tree::Views {
                    tree,
                    paths,
                    by_type,
                };
                let data = crate::mount_tree::MountTreeData::read_from_wasm(&path)?;
                anstream::print!("{}", data.render(views));
                Ok(())
            },
        }
    }
}
