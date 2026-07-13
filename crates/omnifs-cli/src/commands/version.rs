//! `omnifs version` — print CLI and daemon version facts.

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::error::ExitCode;
use crate::launch_backend::BUILD_CHANNEL;
use crate::workspace::Workspace;
use omnifs_workspace::provider::{Catalog, DirStatus};

#[derive(Args, Debug, Clone, Default)]
pub struct VersionArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

impl VersionArgs {
    pub async fn run(self) -> Result<ExitCode> {
        if self.json {
            let workspace = Workspace::resolve()?;
            let payload = VersionJson::collect(&workspace).await?;
            crate::ui::print_json(&payload)?;
            return Ok(ExitCode::Success);
        }
        crate::ui::print_raw(&format!(
            "omnifs {}{}\n",
            env!("CARGO_PKG_VERSION"),
            BUILD_CHANNEL.version_suffix()
        ));
        Ok(ExitCode::Success)
    }
}

#[derive(Serialize)]
struct VersionJson {
    cli: String,
    daemon: Option<DaemonVersionJson>,
    channel: &'static str,
    providers: ProvidersJson,
}

#[derive(Serialize)]
struct DaemonVersionJson {
    version: String,
    api_major: u16,
    api_minor: u16,
    pid: u32,
}

#[derive(Serialize)]
struct ProvidersJson {
    state: &'static str,
    count: usize,
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
        Ok(Self {
            cli: env!("CARGO_PKG_VERSION").to_string(),
            channel: BUILD_CHANNEL.word(),
            daemon,
            providers: provider_summary(workspace.catalog()),
        })
    }
}

fn provider_summary(catalog: &Catalog) -> ProvidersJson {
    match catalog.dir_status() {
        DirStatus::Missing => ProvidersJson {
            state: "missing",
            count: 0,
        },
        DirStatus::Present { wasm_count } => ProvidersJson {
            state: "present",
            count: wasm_count,
        },
        DirStatus::Unreadable(_) => ProvidersJson {
            state: "unreadable",
            count: 0,
        },
    }
}
