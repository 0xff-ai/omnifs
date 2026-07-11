#![allow(clippy::disallowed_macros)] // migrates in wave 4 (cli-redesign)
//! `omnifs version` — print CLI and daemon version facts.

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::error::ExitCode;
use crate::launch_backend::BUILD_CHANNEL;
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::{Catalog, DirStatus};

#[derive(Args, Debug, Clone, Default)]
pub struct VersionArgs {
    /// Print extended version detail.
    #[arg(long = "detail")]
    pub detail: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

impl VersionArgs {
    pub async fn run(self) -> Result<ExitCode> {
        if self.json {
            let workspace = Workspace::resolve()?;
            let payload = VersionJson::collect(&workspace).await?;
            anstream::println!("{}", serde_json::to_string(&payload)?);
            return Ok(ExitCode::Success);
        }
        if !self.detail {
            anstream::println!(
                "omnifs {}{}",
                env!("CARGO_PKG_VERSION"),
                BUILD_CHANNEL.version_suffix()
            );
            return Ok(ExitCode::Success);
        }

        let workspace = Workspace::resolve()?;
        let cli = env!("CARGO_PKG_VERSION");
        let daemon = workspace.daemon().status_optional().await?;
        let provider_status = provider_dir_summary(workspace.catalog());

        anstream::println!("CLI:        omnifs {cli}{}", BUILD_CHANNEL.version_suffix());
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
        Ok(ExitCode::Success)
    }
}

#[derive(Serialize)]
struct VersionJson {
    cli: String,
    channel: &'static str,
    daemon: Option<DaemonVersionJson>,
    store: String,
    providers: String,
    paths: VersionPathsJson,
}

#[derive(Serialize)]
struct DaemonVersionJson {
    version: String,
    api_major: u16,
    api_minor: u16,
    pid: u32,
}

#[derive(Serialize)]
struct VersionPathsJson {
    config: std::path::PathBuf,
    cache: std::path::PathBuf,
    mounts: std::path::PathBuf,
    providers: std::path::PathBuf,
    credentials: std::path::PathBuf,
    config_file: std::path::PathBuf,
}

impl VersionJson {
    async fn collect(workspace: &Workspace) -> Result<Self> {
        let daemon = workspace
            .daemon()
            .status_optional()
            .await?
            .map(|status| DaemonVersionJson {
                version: status.version,
                api_major: status.api_major,
                api_minor: status.api_minor,
                pid: status.pid,
            });
        let paths = workspace.layout();
        Ok(Self {
            cli: env!("CARGO_PKG_VERSION").to_string(),
            channel: BUILD_CHANNEL.word(),
            daemon,
            store: "file".to_string(),
            providers: provider_dir_summary(workspace.catalog()),
            paths: VersionPathsJson {
                config: paths.config_dir.clone(),
                cache: paths.cache_dir.clone(),
                mounts: paths.mounts_dir.clone(),
                providers: paths.providers_dir.clone(),
                credentials: paths.credentials_file.clone(),
                config_file: paths.config_file.clone(),
            },
        })
    }
}

fn provider_dir_summary(catalog: &Catalog) -> String {
    match catalog.dir_status() {
        DirStatus::Missing => "provider dir missing".to_string(),
        DirStatus::Present { wasm_count } => format!("{wasm_count} on disk"),
        DirStatus::Unreadable(error) => format!("provider dir unreadable: {error}"),
    }
}
