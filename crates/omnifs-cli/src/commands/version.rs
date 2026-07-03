//! `omnifs version` — print CLI and daemon version facts.

use anyhow::Result;
use clap::Args;

use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::{Catalog, DirStatus};

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

        let workspace = Workspace::resolve()?;
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
            WorkspaceLayout::display(&workspace.layout().credentials_file)
        );
        anstream::println!("Providers:  {provider_status}");
        anstream::println!();
        anstream::println!("Paths:");
        anstream::println!(
            "  config:       {}",
            WorkspaceLayout::display(&workspace.layout().config_dir)
        );
        anstream::println!(
            "  cache:        {}",
            WorkspaceLayout::display(&workspace.layout().cache_dir)
        );
        anstream::println!(
            "  mounts:       {}",
            WorkspaceLayout::display(&workspace.layout().mounts_dir)
        );
        anstream::println!(
            "  providers:    {}",
            WorkspaceLayout::display(&workspace.layout().providers_dir)
        );
        anstream::println!(
            "  credentials:  {}",
            WorkspaceLayout::display(&workspace.layout().credentials_file)
        );
        anstream::println!(
            "  config file:  {}",
            WorkspaceLayout::display(&workspace.layout().config_file)
        );
        Ok(())
    }
}

fn provider_dir_summary(catalog: &Catalog) -> String {
    match catalog.dir_status() {
        DirStatus::Missing => "provider dir missing".to_string(),
        DirStatus::Present { wasm_count } => format!("{wasm_count} on disk"),
        DirStatus::Unreadable(error) => format!("provider dir unreadable: {error}"),
    }
}
