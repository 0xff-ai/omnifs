//! Linear GraphQL transport extension over `Cx`.
//!
//! Linear PATs go in `Authorization` *without* the `Bearer ` prefix; that
//! is wired up at the host's `api-key-header` injector based on the mount
//! config. Providers never see the credential.

use omnifs_sdk::Cx;
use omnifs_sdk::error::ProviderError;
use omnifs_sdk::http::ResponseExt;
use serde::de::DeserializeOwned;

use crate::API_ENDPOINT;
use crate::Result;
use crate::State;
use crate::graphql::{GqlResponse, gql_body};

pub(crate) trait LinearHttpExt {
    fn graphql<T, V>(
        &self,
        query: &'static str,
        variables: &V,
    ) -> impl core::future::Future<Output = Result<T>>
    where
        T: DeserializeOwned,
        V: serde::Serialize;
}

impl LinearHttpExt for Cx<State> {
    async fn graphql<T, V>(&self, query: &'static str, variables: &V) -> Result<T>
    where
        T: DeserializeOwned,
        V: serde::Serialize,
    {
        let body = gql_body(query, variables);
        let resp = self
            .http()
            .post(API_ENDPOINT)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let parsed: GqlResponse<T> = serde_json::from_slice(resp.body())
            .map_err(|e| ProviderError::internal(format!("linear: parse GraphQL response: {e}")))?;
        if !parsed.errors.is_empty() {
            let msg = parsed
                .errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ProviderError::internal(format!(
                "linear: GraphQL errors: {msg}"
            )));
        }
        parsed
            .data
            .ok_or_else(|| ProviderError::internal("linear: GraphQL response missing data field"))
    }
}
