//! `omnifs up`: daemon lifecycle start.

use clap::Args;

use crate::launch::{LaunchOutcome, Launcher};
use crate::session::GUEST_FUSE_MOUNT;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {}

impl UpArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        match Launcher::new(&workspace, "omnifs up").launch().await? {
            LaunchOutcome::Native { mount_point } => {
                anstream::println!();
                if let Some(mount_point) = mount_point {
                    anstream::println!(
                        "Browse it directly: `{}`",
                        crate::style::bold(format!("ls {}", mount_point.display())),
                    );
                }
            },
            LaunchOutcome::Docker { target } => {
                anstream::println!(
                    "✓ {GUEST_FUSE_MOUNT} is mounted inside `{}`",
                    target.container_name()
                );
                anstream::println!();
                anstream::println!(
                    "Run `{}` to open a shell inside the container and browse {GUEST_FUSE_MOUNT}.",
                    crate::style::bold("omnifs shell"),
                );
            },
        }
        Ok(())
    }
}
