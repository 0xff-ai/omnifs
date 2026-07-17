//! `omnifs version` — print CLI and daemon version facts.

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::error::ExitCode;
use crate::image::BUILD_CHANNEL;
use crate::inventory::Inventory;
use crate::ui::output::{Output, ResultVerdict};
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct VersionArgs {}

impl VersionArgs {
    pub async fn run(self, output: Output) -> Result<ExitCode> {
        if output.is_structured() {
            let workspace = Workspace::resolve()?;
            let payload = VersionJson::collect(&workspace).await?;
            output.emit_result(ResultVerdict::Ok, payload)?;
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
}

#[derive(Serialize)]
struct DaemonVersionJson {
    version: String,
    pid: u32,
}

impl VersionJson {
    async fn collect(workspace: &Workspace) -> Result<Self> {
        let inventory = Inventory::collect(workspace).await?;
        let daemon = inventory
            .daemon
            .status
            .as_ref()
            .map(|status| DaemonVersionJson {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                pid: status.pid,
            });
        Ok(Self {
            cli: env!("CARGO_PKG_VERSION").to_string(),
            channel: BUILD_CHANNEL.word(),
            daemon,
        })
    }
}
