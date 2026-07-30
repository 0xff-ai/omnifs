//! `omnifs version` — print CLI and daemon version facts.

use anyhow::Result;
use serde::Serialize;

use crate::error::ExitCode;
use crate::image::BUILD_CHANNEL;
use crate::inventory::Inventory;
use crate::ui::output::{Output, ResultVerdict};

pub async fn run(output: Output) -> Result<ExitCode> {
    if output.is_structured() {
        let payload = VersionJson::collect().await?;
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
    async fn collect() -> Result<Self> {
        let inventory = Inventory::collect_rpc().await?;
        let daemon = inventory
            .daemon
            .status
            .as_ref()
            .map(|status| DaemonVersionJson {
                version: status.info.version.clone(),
                pid: status.info.pid,
            });
        Ok(Self {
            cli: env!("CARGO_PKG_VERSION").to_string(),
            channel: BUILD_CHANNEL.word(),
            daemon,
        })
    }
}
