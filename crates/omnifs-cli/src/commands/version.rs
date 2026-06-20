//! `omnifs version` — print CLI and daemon version facts.

use anyhow::Result;
use clap::Args;

use crate::catalog::{ProviderCatalog, ProviderDirStatus};
use crate::paths::Paths;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct VersionArgs {
    /// Print extended version detail.
    #[arg(long = "detail")]
    pub detail: bool,
}

impl VersionArgs {
    pub async fn run(self) -> Result<()> {
        if !self.detail {
            anstream::println!("omnifs {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }

        let workspace = Workspace::resolve_default()?;
        let cli = env!("CARGO_PKG_VERSION");
        let daemon = workspace.daemon().status_optional().await?;
        let provider_status = provider_dir_summary(workspace.catalog());

        anstream::println!("CLI:        omnifs {cli}");
        match daemon {
            Some(status) => {
                anstream::println!(
                    "Daemon:     omnifs {} (API {}.{}, pid {})",
                    status.version,
                    status.api_major,
                    status.api_minor,
                    status.pid
                );
            },
            None => anstream::println!("Daemon:     not running"),
        }
        anstream::println!(
            "Store:      file ({})",
            Paths::display(&workspace.paths().credentials_file)
        );
        anstream::println!("Providers:  {provider_status}");
        anstream::println!();
        anstream::println!("Paths:");
        anstream::println!(
            "  config:       {}",
            Paths::display(&workspace.paths().config_dir)
        );
        anstream::println!(
            "  cache:        {}",
            Paths::display(&workspace.paths().cache_dir)
        );
        anstream::println!(
            "  mounts:       {}",
            Paths::display(&workspace.paths().mounts_dir)
        );
        anstream::println!(
            "  providers:    {}",
            Paths::display(&workspace.paths().providers_dir)
        );
        anstream::println!(
            "  credentials:  {}",
            Paths::display(&workspace.paths().credentials_file)
        );
        anstream::println!(
            "  config file:  {}",
            Paths::display(&workspace.paths().config_file)
        );
        Ok(())
    }
}

fn provider_dir_summary(catalog: &ProviderCatalog) -> String {
    match catalog.provider_dir_status() {
        ProviderDirStatus::Missing => "provider dir missing".to_string(),
        ProviderDirStatus::Present { wasm_count } => format!("{wasm_count} on disk"),
        ProviderDirStatus::Unreadable(error) => format!("provider dir unreadable: {error}"),
    }
}
