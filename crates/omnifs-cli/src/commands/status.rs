//! `omnifs status` verb handler.

use crate::app_context::AppContext;
use crate::catalog::ProviderCatalog;
use crate::cli::OutputFormat;
use crate::client::{DaemonClient, DaemonProbe};
use crate::paths::{PathOverrides, Paths};
use crate::status::collect_status;
use anyhow::Context as _;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
    /// Reveal provider runtime detail.
    #[arg(long = "detail")]
    pub detail: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

impl StatusArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let overrides = PathOverrides {
            config_dir: self.config_dir.clone(),
            ..Default::default()
        };
        // A malformed config.toml shouldn't crash `omnifs status`; fall back
        // to pure flag + env + platform-default resolution so the user can
        // still see what's broken.
        let (paths, catalog, mounts) = match AppContext::resolve(overrides.clone(), None, None) {
            Ok(ctx) => {
                let mounts = ctx.workspace().mounts()?;
                (ctx.paths().clone(), ctx.catalog().clone(), mounts)
            },
            Err(error) => {
                anstream::eprintln!("warning: {error:#}");
                let paths = Paths::resolve(overrides)?;
                let catalog = ProviderCatalog::for_dirs(&paths.mounts_dir, &paths.providers_dir);
                (paths, catalog, Vec::new())
            },
        };
        let client = DaemonClient::new();
        let runtime = match client.probe().await? {
            DaemonProbe::Unreachable => None,
            DaemonProbe::Compatible(_) => Some(client.status().await?),
        };
        let report = collect_status(&catalog, paths, runtime, mounts);
        match OutputFormat::from(self.json) {
            OutputFormat::Json => {
                let payload = report.to_json();
                let serialized =
                    serde_json::to_string(&payload).context("serialize status JSON")?;
                anstream::println!("{serialized}");
            },
            OutputFormat::Text => {
                anstream::print!("{}", report.render(self.detail));
            },
        }
        Ok(())
    }
}
