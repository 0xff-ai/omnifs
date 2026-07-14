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

use crate::auth::www_authenticate;
use crate::cache::identity::BlobRequestId;
use crate::callouts::{callout_denied, callout_internal, callout_network, record_outcome};
use crate::capability::CapabilityChecker;
use crate::log_redaction::{LogUrl, WitHeaders};
use dashmap::DashMap;
use omnifs_auth::{AuthBinding, RefreshOutcome};
use omnifs_wit::provider::types as wit_types;
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub struct HttpStack {
    https_client: reqwest::Client,
    unix_clients: DashMap<PathBuf, reqwest::Client>,
    auth: Option<Arc<AuthBinding>>,
    capability: Arc<CapabilityChecker>,
}

/// Request after URL, method, capability, and provider-header validation.
/// Host-injected auth is added only when dispatching and is not identity data.
pub(crate) struct ValidatedRequest {
    method: reqwest::Method,
    url: Url,
    original_url: String,
    canonical_url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl ValidatedRequest {
    pub(crate) fn blob_request_id(&self, auth: Option<&AuthBinding>) -> BlobRequestId {
        BlobRequestId::new(
            auth.map(|binding| binding.credential_id()),
            self.method.as_str(),
            &self.canonical_url,
            &self.headers,
            self.body.as_deref(),
        )
    }

    pub(crate) fn original_url(&self) -> &str {
        &self.original_url
    }
}

impl HttpStack {
    pub(crate) fn auth_binding_for_url(&self, url: &str) -> Option<&AuthBinding> {
        self.auth
            .as_deref()
            .filter(|binding| binding.applies_to_url(url))
    }

    pub fn new(
        auth: Option<Arc<AuthBinding>>,
        capability: Arc<CapabilityChecker>,
    ) -> Result<Self, reqwest::Error> {
        let https_client = Self::client_builder().build()?;
        Ok(Self::with_https_client(auth, capability, https_client))
    }

    fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .user_agent("omnifs")
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
    }

    #[doc(hidden)]
    pub fn with_https_client(
        auth: Option<Arc<AuthBinding>>,
        capability: Arc<CapabilityChecker>,
        https_client: reqwest::Client,
    ) -> Self {
        Self {
            https_client,
            unix_clients: DashMap::new(),
            auth,
            capability,
        }
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
        let request = self.validate(method, url, headers, body)?;
        self.send_validated(&request, timeout).await
    }

    pub(crate) fn validate(
        &self,
        method: &str,
        url: &str,
        headers: &[wit_types::Header],
        body: Option<&[u8]>,
    ) -> Result<ValidatedRequest, wit_types::CalloutResult> {
        let parsed = Url::parse(url).map_err(|_| callout_denied("invalid URL"))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(callout_denied("URL credentials are not allowed"));
        }
        if let Err(e) = self.capability.check_url(url) {
            let _ = e;
            return Err(callout_denied("URL is not allowed"));
        }
        let method = reqwest::Method::from_str(method)
            .map_err(|_| callout_denied(format!("unsupported HTTP method: {method}")))?;
        let mut normalized = Vec::with_capacity(headers.len());
        for header in headers {
            let name = HeaderName::from_str(&header.name).map_err(|error| {
                callout_internal(format!(
                    "invalid request header name `{}`: {error}",
                    header.name
                ))
            })?;
            HeaderValue::from_str(&header.value).map_err(|error| {
                callout_internal(format!(
                    "invalid request header value for `{}`: {error}",
                    name.as_str()
                ))
            })?;
            normalized.push((name.as_str().to_string(), header.value.clone()));
        }
        normalized.sort_by(|left, right| left.0.cmp(&right.0));
        let canonical_url = parsed.to_string();
        Ok(ValidatedRequest {
            method,
            url: parsed,
            original_url: url.to_string(),
            canonical_url,
            headers: normalized,
            body: body.map(ToOwned::to_owned),
        })
    }

    pub(crate) async fn send_validated(
        &self,
        request: &ValidatedRequest,
        timeout: Duration,
    ) -> Result<reqwest::Response, wit_types::CalloutResult> {
        let response = self
            .send_once(
                request.method.clone(),
                &request.url,
                &request.original_url,
                &request.headers,
                request.body.as_deref(),
                timeout,
            )
            .await?;
        let rejection = match &self.auth {
            Some(auth) => {
                auth.report_rejected_for_response(
                    &request.original_url,
                    response.status().as_u16(),
                    www_authenticate(response.headers()),
                )
                .await
            },
            None => RefreshOutcome::NotApplicable,
        };
        match rejection {
            RefreshOutcome::Refreshed => {
                self.send_once(
                    request.method.clone(),
                    &request.url,
                    &request.original_url,
                    &request.headers,
                    request.body.as_deref(),
                    timeout,
                )
                .await
            },
            RefreshOutcome::NoCredential | RefreshOutcome::NotApplicable => Ok(response),
            RefreshOutcome::RefreshFailed(error) => {
                Err(callout_denied(format!("auth refresh failed: {error}")))
            },
        }
    }

    async fn send_once(
        &self,
        reqwest_method: reqwest::Method,
        parsed: &Url,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<reqwest::Response, wit_types::CalloutResult> {
        let auth_header = match &self.auth {
            Some(auth) => auth
                .authorization_for(url)
                .await
                .map_err(|error| callout_denied(format!("auth authorization failed: {error}")))?,
            None => None,
        };

        let header_map = match build_header_map(
            auth_header
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        ) {
            Ok(header_map) => header_map,
            Err(message) => return Err(callout_internal(message)),
        };

        let (client, request_url) = self.client_and_url_for(parsed)?;
        let mut request = client
            .request(reqwest_method, request_url)
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

    fn client_and_url_for(
        &self,
        parsed: &Url,
    ) -> Result<(reqwest::Client, Url), wit_types::CalloutResult> {
        match parsed.scheme() {
            "https" => Ok((self.https_client.clone(), parsed.clone())),
            "unix" => {
                let socket = CapabilityChecker::decode_unix_socket(parsed.as_str())
                    .map_err(|e| callout_denied(e.to_string()))?;
                let client = self.unix_client_for(&socket).map_err(|e| {
                    callout_internal(format!(
                        "failed to build unix client for {}: {e}",
                        socket.display()
                    ))
                })?;
                Ok((client, unix_request_url(parsed)?))
            },
            other => Err(callout_denied(format!("unsupported URL scheme `{other}`"))),
        }
    }

    fn unix_client_for(&self, socket: &Path) -> Result<reqwest::Client, reqwest::Error> {
        if let Some(existing) = self.unix_clients.get(socket) {
            return Ok(existing.clone());
        }
        let client = Self::client_builder()
            .unix_socket(socket.to_path_buf())
            .build()?;
        Ok(self
            .unix_clients
            .entry(socket.to_path_buf())
            .or_insert(client)
            .clone())
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

fn unix_request_url(parsed: &Url) -> Result<Url, wit_types::CalloutResult> {
    let mut rewritten = String::from("http://localhost");
    rewritten.push_str(parsed.path());
    if let Some(query) = parsed.query() {
        rewritten.push('?');
        rewritten.push_str(query);
    }
    Url::parse(&rewritten).map_err(|e| {
        callout_internal(format!(
            "could not rewrite unix URL {parsed} to http form: {e}"
        ))
    })
}

/// Combine auth and request headers into a single `HeaderMap`. Invalid
/// header names or values are reported with their source so the
/// caller's diagnostics distinguish a bad provider header from a bad
/// host-injected auth header.
fn build_header_map<'a, A, R>(auth_headers: A, request_headers: R) -> Result<HeaderMap, String>
where
    A: IntoIterator<Item = (&'a str, &'a str)>,
    R: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut header_map = HeaderMap::new();
    append_headers(&mut header_map, auth_headers, "auth")?;
    append_headers(&mut header_map, request_headers, "request")?;
    Ok(header_map)
}

fn append_headers<'a, I>(header_map: &mut HeaderMap, headers: I, source: &str) -> Result<(), String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    for (name, value) in headers {
        let header_name = HeaderName::from_str(name)
            .map_err(|error| format!("invalid {source} header name `{name}`: {error}"))?;
        let header_value = HeaderValue::from_str(value).map_err(|error| {
            format!(
                "invalid {source} header value for `{}`: {error}",
                header_name.as_str()
            )
        })?;
        header_map.append(header_name, header_value);
    }
    Ok(())
}

/// Decode a `HeaderMap` from reqwest into the WIT-friendly `(name,
/// value)` shape, dropping non-UTF8 values rather than failing the
/// whole response (provider headers are UTF-8 only by contract).
pub(crate) fn decode_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| match value.to_str() {
            Ok(value) => Some((name.as_str().to_string(), value.to_string())),
            Err(error) => {
                warn!(
                    header = %name,
                    err = %error,
                    "dropping non-UTF8 response header because provider headers are UTF-8 only"
                );
                None
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::HttpStack;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn client_builder_returns_redirect_response_without_following_it() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let client = HttpStack::client_builder().build().unwrap();
        let response = client
            .get(format!("http://{address}/start"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        server.await.unwrap();
    }
}
