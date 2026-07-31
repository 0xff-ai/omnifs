//! `omnifs version` — print CLI and daemon version facts.

use anyhow::Result;
use serde::Serialize;

use crate::error::ExitCode;
use crate::ui::output::{Output, ResultVerdict};
use omnifs_bootstrap::BUILD_CHANNEL;

pub async fn run(output: Output) -> Result<ExitCode> {
    if output.is_structured() {
        let payload = VersionJson::collect().await?;
        output.emit_result(ResultVerdict::Ok, payload)?;
        return Ok(ExitCode::Success);
    }
    output.report(format!(
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
    /// Version and pid need one `GetInventory` RPC and nothing else; skip
    /// `Inventory::collect_rpc`'s local filesystem-registry reads and status
    /// derivations, which this command has no use for. A daemon that cannot
    /// be reached (not running, or any other resolve/RPC failure) reports the
    /// CLI's own version with a null `daemon` section, exactly as before.
    async fn collect() -> Result<Self> {
        let daemon = match crate::rpc::RpcClient::resolve() {
            Ok(rpc) => rpc
                .inventory()
                .await
                .ok()
                .map(|inventory| DaemonVersionJson {
                    version: inventory.info.version,
                    pid: inventory.info.pid,
                }),
            Err(_) => None,
        };
        Ok(Self {
            cli: env!("CARGO_PKG_VERSION").to_string(),
            channel: BUILD_CHANNEL.word(),
            daemon,
        })
    }
}
