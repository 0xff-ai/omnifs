//! Docker daemon endpoint and fetch helpers.
//!
//! The provider talks v1.43 against the configured endpoint. `unix:` endpoint
//! bases are decoded by the host's existing callout path; the socket grant
//! itself is manifest-driven (the `unixSocket` capability in
//! `omnifs.provider.json`), not a `resources(..)` declaration.

use omnifs_sdk::http::{HttpEndpoint, ResponseExt};
use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;

use crate::Result;
use crate::State;

/// Pinned to v1.43 for the Phase 2 slice. v1.43 ships in Docker Engine
/// 24.0+ (mid-2023); macOS/Linux Docker Desktop and most CI runners
/// satisfy this. A daemon advertising a lower `MinAPIVersion` will
/// reject these calls; bump deliberately when raising the floor.
pub(crate) const API_VERSION_PREFIX: &str = "/v1.43";

/// Fetch a JSON document from `path`, prefixing the pinned API version.
pub(crate) async fn fetch_json<T>(cx: &Cx<State>, path: &str, query: &[(&str, &str)]) -> Result<T>
where
    T: DeserializeOwned,
{
    let bytes = fetch_bytes(cx, path, query).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ProviderError::internal(format!("docker JSON parse error: {error}")))
}

/// Fetch the raw response body from `path`, prefixing the pinned API version.
pub(crate) async fn fetch_bytes(
    cx: &Cx<State>,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Vec<u8>> {
    let endpoint = cx.state(|state| state.endpoint.clone());
    let url = docker_url(&endpoint, path, query);
    let response = cx.http().get(url).send().await?.error_for_status()?;
    Ok(response.into_body())
}

fn docker_url(endpoint: &HttpEndpoint, path: &str, query: &[(&str, &str)]) -> String {
    endpoint.build_url(&format!("{API_VERSION_PREFIX}{path}"), query)
}

// Wire DTOs re-exported from `bollard-stubs` (pinned in `Cargo.toml`).

pub use bollard_stubs::models::{
    ContainerInspectResponse, ContainerSummary, SystemDataUsageResponse, SystemInfo, SystemVersion,
};
