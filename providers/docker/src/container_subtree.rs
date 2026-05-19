//! Per-container subtree: `/inspect.json`, `/summary.json`,
//! `/summary.txt`, `/state`.
//!
//! Mounted from `containers.rs` and `compose.rs` via `#[bind(...)]`,
//! keyed by name or id (encoded in `ContainerKey`).

use omnifs_sdk::prelude::*;

use crate::api::fetch_json;
use crate::system::pretty_json;
use crate::wire::{ContainerInspectResponse, ContainerSummary};
use crate::{Result, State};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ContainerKey {
    Name(String),
    Id(String),
}

impl ContainerKey {
    /// The opaque token Docker accepts in path positions for any
    /// container reference (a name, full id, or short-id prefix).
    pub fn as_path(&self) -> &str {
        match self {
            Self::Name(s) | Self::Id(s) => s,
        }
    }
}

impl std::fmt::Display for ContainerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_path())
    }
}

pub struct ContainerSubtree {
    pub key: ContainerKey,
}

#[subtree]
impl ContainerSubtree {
    #[dir("/")]
    fn root(cx: &BindCtx<'_, State, ContainerSubtree>) -> Result<Projection> {
        let _ = cx;
        // The sibling `#[file]` handlers auto-derive the listing; we
        // only declare it exhaustive so the host can satisfy negative
        // lookups without a provider call.
        let mut p = Projection::new();
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[file("/inspect.json")]
    async fn inspect(cx: &BindCtx<'_, State, ContainerSubtree>) -> Result<FileContent> {
        let inspect = inspect_container(cx, &cx.bindings().key).await?;
        Ok(FileContent::bytes(pretty_json(&inspect)?))
    }

    #[file("/summary.json")]
    async fn summary_json(cx: &BindCtx<'_, State, ContainerSubtree>) -> Result<FileContent> {
        let summary = lookup_summary(cx, &cx.bindings().key).await?;
        Ok(FileContent::bytes(pretty_json(&summary)?))
    }

    #[file("/summary.txt")]
    async fn summary_txt(cx: &BindCtx<'_, State, ContainerSubtree>) -> Result<FileContent> {
        let summary = lookup_summary(cx, &cx.bindings().key).await?;
        Ok(FileContent::bytes(render_summary_text(&summary)))
    }

    #[file("/state")]
    async fn state(cx: &BindCtx<'_, State, ContainerSubtree>) -> Result<FileContent> {
        let inspect = inspect_container(cx, &cx.bindings().key).await?;
        let status = inspect.state.and_then(|state| state.status).map_or_else(
            || "unknown".to_string(),
            |status| status.as_ref().to_string(),
        );
        let mut bytes = status.into_bytes();
        bytes.push(b'\n');
        Ok(FileContent::bytes(bytes))
    }
}

async fn inspect_container(
    cx: &BindCtx<'_, State, ContainerSubtree>,
    key: &ContainerKey,
) -> Result<ContainerInspectResponse> {
    fetch_json(cx, &format!("/containers/{}/json", key.as_path()), &[]).await
}

async fn lookup_summary(
    cx: &BindCtx<'_, State, ContainerSubtree>,
    key: &ContainerKey,
) -> Result<ContainerSummary> {
    // The `/containers/json` endpoint is the only daemon route that
    // returns the canonical summary record. We post-filter to the one
    // we want; for the Phase 2 slice this keeps the dependency on
    // bollard-stubs narrow (no per-call list of summaries cached at
    // the provider) at the cost of one extra request.
    let summaries: Vec<ContainerSummary> =
        fetch_json(cx, "/containers/json", &[("all", "true")]).await?;
    summaries
        .into_iter()
        .find(|summary| summary_matches(summary, key))
        .ok_or_else(|| ProviderError::not_found(format!("container not found: {key}")))
}

pub(crate) fn summary_matches(summary: &ContainerSummary, key: &ContainerKey) -> bool {
    match key {
        ContainerKey::Id(id) => summary
            .id
            .as_deref()
            .is_some_and(|sid| sid.eq_ignore_ascii_case(id) || sid.starts_with(id)),
        ContainerKey::Name(name) => summary
            .names
            .iter()
            .flatten()
            .any(|raw| strip_leading_slash(raw) == name),
    }
}

pub(crate) fn strip_leading_slash(raw: &str) -> &str {
    raw.strip_prefix('/').unwrap_or(raw)
}

fn render_summary_text(summary: &ContainerSummary) -> Vec<u8> {
    use std::fmt::Write;

    let id = summary.id.as_deref().unwrap_or("");
    let short_id = id.get(..12).unwrap_or(id);
    let image = summary.image.as_deref().unwrap_or("");
    let names = summary
        .names
        .iter()
        .flatten()
        .map(|n| strip_leading_slash(n))
        .collect::<Vec<_>>()
        .join(", ");
    let state = summary
        .state
        .map(|s| s.as_ref().to_string())
        .unwrap_or_default();
    let status = summary.status.as_deref().unwrap_or("");
    let mut text = String::new();
    let _ = writeln!(text, "id     {short_id}");
    let _ = writeln!(text, "name   {names}");
    let _ = writeln!(text, "image  {image}");
    let _ = writeln!(text, "state  {state}");
    let _ = writeln!(text, "status {status}");
    text.into_bytes()
}
