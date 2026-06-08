//! Docker daemon endpoint and fetch helpers.
//!
//! The provider talks v1.43 against a unix socket through the typed
//! [`DockerApi`] endpoint. The `unix:` scheme in the endpoint base is decoded
//! by the host's existing callout path; the socket grant itself is
//! manifest-driven (the `unixSocket` capability in `omnifs.provider.json`),
//! not a `resources(..)` declaration.

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;

use crate::Result;

/// Typed outbound endpoint for the Docker daemon. The base carries the
/// `unix://` socket path; every request path is prefixed with the pinned API
/// version (`API_VERSION_PREFIX`).
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "unix:///var/run/docker.sock")]
pub struct DockerApi;

/// Pinned to v1.43 for the Phase 2 slice. v1.43 ships in Docker Engine
/// 24.0+ (mid-2023); macOS/Linux Docker Desktop and most CI runners
/// satisfy this. A daemon advertising a lower `MinAPIVersion` will
/// reject these calls; bump deliberately when raising the floor.
pub(crate) const API_VERSION_PREFIX: &str = "/v1.43";

/// Fetch a JSON document from `path`, prefixing the pinned API version.
pub(crate) async fn fetch_json<T>(cx: &Cx, path: &str, query: &[(&str, &str)]) -> Result<T>
where
    T: DeserializeOwned,
{
    let bytes = fetch_bytes(cx, path, query).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ProviderError::internal(format!("docker JSON parse error: {error}")))
}

/// Fetch the raw response body from `path`, prefixing the pinned API version.
pub(crate) async fn fetch_bytes(cx: &Cx, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut request = cx
        .endpoint::<DockerApi>()
        .get(format!("{API_VERSION_PREFIX}{path}"));
    for (key, value) in query {
        request = request.query(key, value);
    }
    let response = request.send_checked().await?;
    Ok(response.body().to_vec())
}

// Wire DTOs re-exported from `bollard-stubs` (pinned in `Cargo.toml`).

pub use bollard_stubs::models::{
    ContainerInspectResponse, ContainerSummary, SystemDataUsageResponse, SystemInfo, SystemVersion,
};
