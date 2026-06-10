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
    /// Print the provider auth manifest for a mount (formerly `auth schemes`).
    AuthManifest { mount: String },
    /// Push the compile-time embedded dev mount specs to the running
    /// daemon. Used by the CI smoke harness, which runs the bare runtime
    /// image without a host-side `omnifs dev` to seed mounts.
    PushDevMounts,
}

impl DebugArgs {
    pub async fn run(self) -> anyhow::Result<()> {
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
                let data = crate::mount_tree::read_from_wasm(&path)?;
                anstream::print!("{}", data.render(views));
                Ok(())
            },
            DebugCommand::AuthManifest { mount } => {
                let ctx = crate::app_context::AppContext::resolve_default()?;
                let mounts = ctx.workspace().mounts()?;
                crate::commands::auth::run_auth_manifest(ctx.catalog(), &mounts, &mount)
            },
            DebugCommand::PushDevMounts => push_dev_mounts().await,
        }
    }
}

async fn push_dev_mounts() -> anyhow::Result<()> {
    use anyhow::Context as _;

    let client = crate::client::DaemonClient::new();
    client.require_compatible().await?;
    for config in crate::dev_mounts::configs()? {
        client
            .add_mount(&config.config)
            .await
            .with_context(|| format!("load dev mount `{}`", config.name))?;
        anstream::println!("✓ Loaded `{}`", config.name);
    }
    Ok(())
}
