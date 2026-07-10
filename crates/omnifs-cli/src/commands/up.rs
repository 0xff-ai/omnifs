//! `omnifs up`: daemon lifecycle start.

use clap::Args;

use crate::launch::Launcher;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Skip auto-starting the virtualized FUSE frontend on macOS. No effect
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

        // macOS's primary consumption surface is the virtualized FUSE
        // frontend; the native NFS mount above stays available either way.
        // Linux never auto-starts it: the native FUSE host mount is already
        // the primary surface there, and the frontend stays opt-in.
        if cfg!(target_os = "macos") && !self.no_frontend {
            start_frontend(&workspace).await;
        }

        if let Some(timeout) = wait {
            crate::stages::wait_until_ready(&workspace, timeout).await?;
            anstream::eprintln!("Daemon is ready.");
        }
        crate::telemetry::maybe_print_health_nudge(&workspace).await;
        Ok(())
    }
}

/// Auto-start the virtualized FUSE frontend, under the configured `[frontend]
/// driver` (docker by default; `FrontendUpArgs::default()` carries no
/// explicit `--driver` override, so it resolves the same config value
/// `omnifs frontend up` would). The daemon's own native mount has already
/// succeeded by the time this runs, so a failure here (most commonly: the
/// driver's runtime is not available, or an unimplemented driver was
/// configured) is reported plainly and does not fail `omnifs up` — the
/// native mount stays usable, and `omnifs frontend up` can be retried once
/// the driver is available.
async fn start_frontend(workspace: &Workspace) {
    anstream::eprintln!();
    let driver_label = workspace
        .config()
        .map_or("docker", |config| config.frontend.driver.as_via().label());
    anstream::eprintln!("Starting the {driver_label} frontend...");
    if let Err(error) = crate::commands::frontend::up::FrontendUpArgs::default()
        .run()
        .await
    {
        anstream::eprintln!("⚠  Could not start the {driver_label} frontend: {error:#}");
        anstream::eprintln!(
            "{}",
            crate::ui::note(
                "the native mount above is still available; run `omnifs frontend up` to retry, or pass --no-frontend to skip it"
            )
        );
    }
}
