//! HTTP fetch executor and shared callout error taxonomy.
//!
//! `ErrorKind` is the host-internal classification of callout failures
//! that each executor returns through `wit_types::CalloutResult`. The
//! HTTP executor owns the request/response path for `Callout::Fetch`.

use crate::auth::AuthManager;
use crate::omnifs::provider::types as wit_types;
use crate::runtime::capability::CapabilityChecker;
use crate::runtime::http_headers::{build_header_map, decode_response_headers};
use crate::runtime::{callout_denied, callout_internal, callout_network};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Network,
    Timeout,
    Denied,
    NotFound,
    RateLimited,
    InvalidInput,
    TooLarge,
    Internal,
}

pub struct HttpExecutor {
    client: reqwest::Client,
    auth: Arc<AuthManager>,
    capability: Arc<CapabilityChecker>,
}

impl HttpExecutor {
    pub fn new(
        auth: Arc<AuthManager>,
        capability: Arc<CapabilityChecker>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent("omnifs")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            auth,
            capability,
        })
    }

    pub async fn fetch(&self, req: &wit_types::HttpRequest) -> wit_types::CalloutResult {
        if let Err(e) = self.capability.check_url(&req.url) {
            return callout_denied(e.to_string());
        }

        let auth_headers = self.auth.headers_for_url(&req.url);
        if auth_headers.is_empty() && self.auth.requires_auth_for_url(&req.url) {
            return callout_denied(format!("no credentials for {}", req.url));
        }

        let Ok(reqwest_method) = reqwest::Method::from_str(&req.method) else {
            return callout_denied(format!("unsupported HTTP method: {}", req.method));
        };

        let header_map = match build_header_map(
            auth_headers.iter().map(|(n, v)| (n.as_str(), v.as_str())),
            req.headers
                .iter()
                .map(|h| (h.name.as_str(), h.value.as_str())),
        ) {
            Ok(header_map) => header_map,
            Err(message) => return callout_internal(message),
        };

        let mut request = self.client.request(reqwest_method, &req.url);
        request = request.headers(header_map);
        if let Some(body) = req.body.as_deref() {
            request = request.body(owned_body(body));
        }

        match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let response_headers = decode_response_headers(response.headers());
                match response.bytes().await {
                    Ok(body) => wit_types::CalloutResult::HttpResponse(wit_types::HttpResponse {
                        status,
                        headers: response_headers
                            .into_iter()
                            .map(|(name, value)| wit_types::Header { name, value })
                            .collect(),
                        body: body.to_vec(),
                    }),
                    Err(e) => callout_network(e.to_string()),
                }
            },
            Err(e) => callout_network(e.to_string()),
        }
    }
}

fn owned_body(body: &[u8]) -> reqwest::Body {
    // reqwest owns the request body across the async send path, so a borrowed
    // provider slice has to be copied into an owned body here.
    reqwest::Body::from(body.to_vec())
}
