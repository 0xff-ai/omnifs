//! Docker daemon API helpers.
//!
//! The provider talks v1.43 against a unix socket through the
//! capability-checked `unix:` URL scheme. `ApiBase` carries the parsed
//! endpoint and exposes a single `url(path, query)` helper so callers
//! never build the encoded unix-URL by hand.

use omnifs_sdk::http::{HttpEndpoint, ResponseExt};
use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;

use crate::State;

/// Pinned to v1.43 for the Phase 2 slice. v1.43 ships in Docker Engine
/// 24.0+ (mid-2023); macOS/Linux Docker Desktop and most CI runners
/// satisfy this. A daemon advertising a lower `MinAPIVersion` will
/// reject these calls; bump deliberately when raising the floor.
pub(crate) const API_VERSION_PREFIX: &str = "/v1.43";

#[derive(Clone, Debug)]
pub struct ApiBase {
    endpoint: HttpEndpoint,
}

impl ApiBase {
    pub fn new(endpoint: HttpEndpoint) -> Self {
        Self { endpoint }
    }

    /// Build a callout URL, prefixing every path with the pinned API
    /// version. `path` should start with `/`.
    pub fn url(&self, path: &str, query: &[(&str, &str)]) -> String {
        let prefixed = format!("{API_VERSION_PREFIX}{path}");
        self.endpoint.build_url(&prefixed, query)
    }
}

pub(crate) async fn fetch_json<T>(cx: &Cx<State>, path: &str, query: &[(&str, &str)]) -> Result<T>
where
    T: DeserializeOwned,
{
    let bytes = fetch_bytes(cx, path, query).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ProviderError::internal(format!("docker JSON parse error: {error}")))
}

pub(crate) async fn fetch_bytes(
    cx: &Cx<State>,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Vec<u8>> {
    let url = cx.state(|state| state.api.url(path, query));
    let response = cx.http().get(url).send().await?;
    let response = response.error_for_status()?;
    Ok(response.into_body())
}
