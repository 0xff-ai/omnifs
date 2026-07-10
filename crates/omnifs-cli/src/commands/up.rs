//! `omnifs up`: daemon lifecycle start.

use clap::Args;

use crate::launch::Launcher;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Skip auto-starting the Docker-hosted FUSE frontend on macOS. No effect
    /// on Linux, where the frontend stays manual (`omnifs frontend up`).
    #[arg(long)]
    pub no_frontend: bool,
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
        let outcome = Launcher::new(&workspace, "omnifs up").launch().await?;
        anstream::eprintln!();
        if let Some(mount_point) = &outcome.mount_point {
            anstream::eprintln!(
                "Browse it directly: `{}`",
                crate::style::bold(format!("ls {}", mount_point.display())),
            );
        }

        // macOS's primary consumption surface is the Docker-hosted FUSE
        // frontend; the native NFS mount above stays available either way.
        // Linux never auto-starts it: the native FUSE host mount is already
        // the primary surface there, and the frontend stays opt-in.
        if cfg!(target_os = "macos") && !self.no_frontend {
            start_frontend().await;
        }

        if let Some(timeout) = wait {
            crate::stages::wait_until_ready(&workspace, timeout).await?;
            anstream::eprintln!("Daemon is ready.");
        }
        crate::telemetry::maybe_print_health_nudge(&workspace).await;
        Ok(())
    }
}

/// Auto-start the Docker-hosted FUSE frontend. The daemon's own native mount
/// has already succeeded by the time this runs, so a failure here (most
/// commonly: Docker is not running) is reported plainly and does not fail
/// `omnifs up` — the native mount stays usable, and `omnifs frontend up` can
/// be retried once Docker is available.
async fn start_frontend() {
    anstream::eprintln!();
    anstream::eprintln!("Starting the Docker-hosted FUSE frontend...");
    if let Err(error) = crate::commands::frontend::up::FrontendUpArgs::default()
        .run()
        .await
    {
        anstream::eprintln!("⚠  Could not start the Docker-hosted FUSE frontend: {error:#}");
        anstream::eprintln!(
            "{}",
            crate::ui::note(
                "the native mount above is still available; run `omnifs frontend up` to retry, or pass --no-frontend to skip it"
            )
        );
    }
}
