//! Shared HTTP transport for provider callouts.
//!
//! `HttpStack` owns the reqwest client, auth resolver, and capability
//! checker. It is the single place where method parsing, header
//! construction, body handing, and network error mapping happen for
//! provider HTTP traffic. The HTTP and blob callout paths both build on
//! this so their auth and capability semantics cannot drift.
//!
//! Encapsulation contract: `reqwest::Client`, `reqwest::Method`,
//! `reqwest::header::HeaderMap`, and `reqwest::RequestBuilder` stay
//! hidden inside this module. `reqwest::Response` intentionally crosses
//! the boundary so the blob executor can stream the body to disk
//! without re-buffering. That is the only reqwest type any caller ever
//! sees.

use crate::auth::AuthManager;
use crate::omnifs::provider::types as wit_types;
use crate::runtime::callouts::{callout_denied, callout_internal, callout_network, record_outcome};
use crate::runtime::capability::CapabilityChecker;
use crate::runtime::http_headers::{build_header_map, decode_response_headers};
use crate::runtime::log_redaction::{LogUrl, WitHeaders};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

pub struct HttpStack {
    client: reqwest::Client,
    auth: Arc<AuthManager>,
    capability: Arc<CapabilityChecker>,
}

impl HttpStack {
    pub fn new(
        auth: Arc<AuthManager>,
        capability: Arc<CapabilityChecker>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent("omnifs")
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            client,
            auth,
            capability,
        })
    }

    /// Authorize, build, and dispatch a request. Returns the in-flight
    /// response on success or a fully-formed `CalloutResult` on any
    /// pre-flight or network failure. `reqwest::Response` is the only
    /// reqwest type that crosses this boundary; everything else stays
    /// hidden inside the stack.
    pub async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[wit_types::Header],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<reqwest::Response, wit_types::CalloutResult> {
        if let Err(e) = self.capability.check_url(url) {
            return Err(callout_denied(e.to_string()));
        }

        let auth_headers = self.auth.headers_for_url(url);
        if auth_headers.is_empty() && self.auth.requires_auth_for_url(url) {
            return Err(callout_denied(format!("no credentials for {url}")));
        }

        let Ok(reqwest_method) = reqwest::Method::from_str(method) else {
            return Err(callout_denied(format!("unsupported HTTP method: {method}")));
        };

        let header_map = match build_header_map(
            auth_headers.iter().map(|(n, v)| (n.as_str(), v.as_str())),
            headers.iter().map(|h| (h.name.as_str(), h.value.as_str())),
        ) {
            Ok(header_map) => header_map,
            Err(message) => return Err(callout_internal(message)),
        };

        let mut request = self
            .client
            .request(reqwest_method, url)
            .headers(header_map)
            .timeout(timeout);
        if let Some(body) = body {
            request = request.body(body.to_vec());
        }

        request
            .send()
            .await
            .map_err(|e| callout_network(e.to_string()))
    }

    /// In-memory HTTP fetch: dispatch the request and decode the
    /// response body into a `CalloutResult::HttpResponse`.
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        method = req.method.as_str(),
        url = %LogUrl(&req.url),
        request_headers = %WitHeaders(&req.headers),
        request_body_bytes = req.body.as_ref().map_or(0, Vec::len),
        status = tracing::field::Empty,
        response_headers = tracing::field::Empty,
        response_body_bytes = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub async fn fetch(
        &self,
        req: &wit_types::HttpRequest,
        timeout: Duration,
    ) -> wit_types::CalloutResult {
        let result = match self
            .send(
                &req.method,
                &req.url,
                &req.headers,
                req.body.as_deref(),
                timeout,
            )
            .await
        {
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
            Err(early) => early,
        };
        record_outcome(&result);
        result
    }
}
