//! Docker daemon endpoint and fetch helpers.
//!
//! The provider talks v1.43 against the configured endpoint. `unix:` endpoint
//! bases are decoded by the host's existing callout path; the socket grant
//! itself is manifest-driven (the `unix_socket` capability declared via
//! `#[omnifs_sdk::provider(capabilities(..))]`), not a `resources(..)` declaration.

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;

use crate::Result;
use crate::State;

/// Pinned to v1.43, which ships in Docker Engine 24.0+ (mid-2023).
/// macOS/Linux Docker Desktop and most CI runners satisfy this. A daemon
/// advertising a lower `MinAPIVersion` will reject these calls; bump
/// deliberately when raising the floor.
pub(crate) const API_VERSION_PREFIX: &str = "/v1.43";

/// The Docker daemon endpoint. Its base is the configured daemon address (a
/// `unix://` socket or a TCP host), resolved from provider state at call time,
/// so it carries a field instead of a `#[derive(Endpoint)]` constant base.
struct DockerApi {
    base: String,
}

impl Endpoint for DockerApi {
    fn base(&self) -> &str {
        &self.base
    }
}
impl EndpointHooks for DockerApi {}

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
    let base = cx.state(|state| state.endpoint.clone());
    let request = cx
        .endpoint(DockerApi { base })
        .get(format!("{API_VERSION_PREFIX}{path}"))
        .query_pairs(query.iter().copied());
    let response = request.send_checked().await?;
    Ok(response.body().to_vec())
}

// Wire DTOs re-exported from `bollard-stubs` (pinned in `Cargo.toml`).

pub use bollard_stubs::models::{
    ContainerInspectResponse, ContainerSummary, SystemDataUsageResponse, SystemInfo, SystemVersion,
};
