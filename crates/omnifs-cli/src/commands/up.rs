//! `omnifs up`: daemon lifecycle start.

use clap::Args;

use crate::config::ConfiguredBackend;
use crate::launch::{LaunchOutcome, Launcher};
use crate::session::GUEST_FUSE_MOUNT;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Runtime for this launch only, overriding the default chosen during
    /// `omnifs setup`. Not persisted: the next bare `omnifs up` uses the
    /// configured default again.
    #[arg(long, value_enum)]
    pub runtime: Option<ConfiguredBackend>,
}

impl UpArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let launcher = Launcher::new(&workspace, "omnifs up").with_runtime_override(self.runtime);
        match launcher.launch().await? {
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
