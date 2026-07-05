//! `omnifs up`: daemon lifecycle start.

use clap::Args;

use crate::config::ConfiguredBackend;
use crate::launch::{LaunchOutcome, Launcher};
use crate::launch_backend::GUEST_MOUNT;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Runtime for this launch only, overriding the default chosen during
    /// `omnifs setup`. Not persisted: the next bare `omnifs up` uses the
    /// configured default again.
    #[arg(long, value_enum)]
    pub runtime: Option<ConfiguredBackend>,
    /// Wait until /v1/ready answers, failing with exit code 3 on timeout.
    #[arg(long, value_name = "DURATION")]
    pub wait: Option<String>,
}

impl UpArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let wait = self
            .wait
            .as_deref()
            .map(crate::stages::parse_wait_duration)
            .transpose()?;
        let launcher = Launcher::new(&workspace, "omnifs up").with_runtime_override(self.runtime);
        match launcher.launch().await? {
            LaunchOutcome::Native { mount_point } => {
                anstream::eprintln!();
                if let Some(mount_point) = mount_point {
                    anstream::eprintln!(
                        "Browse it directly: `{}`",
                        crate::style::bold(format!("ls {}", mount_point.display())),
                    );
                }
            },
            LaunchOutcome::Docker { target } => {
                anstream::eprintln!(
                    "✓ {GUEST_MOUNT} is mounted inside `{}`",
                    target.container_name()
                );
                anstream::eprintln!();
                anstream::eprintln!(
                    "Run `{}` to open a shell inside the container and browse {GUEST_MOUNT}.",
                    crate::style::bold("omnifs shell"),
                );
            },
        }
        if let Some(timeout) = wait {
            crate::stages::wait_until_ready(&workspace, timeout).await?;
            anstream::eprintln!("Daemon is ready.");
        }
        crate::telemetry::maybe_print_health_nudge(&workspace).await;
        Ok(())
    }
}
