#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! github-provider: GitHub virtual filesystem provider for omnifs.
//!
//! Exposes GitHub resources (issues, PRs, actions, repository contents)
//! as a virtual filesystem using the omnifs provider WIT interface.

use crate::types::RepoId;
use omnifs_sdk::prelude::ProviderError;
pub(crate) use omnifs_sdk::prelude::Result;

mod actions;
mod events;
mod http_ext;
mod issues;
mod numbered;
mod owners;
mod provider;
mod pulls;
mod repo;
mod root;
pub(crate) mod types;

/// Base URL for the GitHub REST API. Compose with a leading-slash path.
pub(crate) const API_BASE: &str = "https://api.github.com";

/// Parse a JSON API response body into a model type.
pub(crate) fn parse_model<T>(body: &[u8]) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_slice(body)
        .map_err(|error| ProviderError::invalid_input(format!("JSON parse error: {error}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerKind {
    User,
    Org,
}

#[derive(Clone)]
#[omnifs_sdk::config]
pub struct Config {}

/// Cap on how many `on_event` outcomes the provider keeps in its
/// in-memory tail buffer. Each entry is a small NDJSON line so the
/// total memory cost stays bounded.
pub(crate) const EVENT_LOG_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct State {
    pub(crate) event_etags: hashbrown::HashMap<RepoId, String>,
    pub(crate) event_log: std::collections::VecDeque<String>,
}
