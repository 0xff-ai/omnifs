use omnifs_sdk::prelude::*;

use crate::api::{fetch_bytes, fetch_json};
use crate::wire::{SystemDataUsageResponse, SystemInfo, SystemVersion};
use crate::{Result, State};

pub struct SystemHandlers;

#[handlers]
impl SystemHandlers {
    #[file("/system/info.json")]
    async fn info(cx: &Cx<State>) -> Result<FileContent> {
        let info: SystemInfo = fetch_json(cx, "/info", &[]).await?;
        Ok(FileContent::bytes(pretty_json(&info)?))
    }

    #[file("/system/version.json")]
    async fn version(cx: &Cx<State>) -> Result<FileContent> {
        let version: SystemVersion = fetch_json(cx, "/version", &[]).await?;
        Ok(FileContent::bytes(pretty_json(&version)?))
    }

    #[file("/system/df.json")]
    async fn df(cx: &Cx<State>) -> Result<FileContent> {
        let usage: SystemDataUsageResponse = fetch_json(cx, "/system/df", &[]).await?;
        Ok(FileContent::bytes(pretty_json(&usage)?))
    }

    #[file("/system/ping")]
    async fn ping(cx: &Cx<State>) -> Result<FileContent> {
        // The daemon's `/_ping` returns the literal text "OK" with a
        // 200 status. We pass it through verbatim plus a trailing
        // newline so `cat /docker/system/ping` looks normal.
        let mut bytes = fetch_bytes(cx, "/_ping", &[]).await?;
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        Ok(FileContent::bytes(bytes))
    }
}

pub(crate) fn pretty_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| ProviderError::internal(format!("docker JSON encode error: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}
