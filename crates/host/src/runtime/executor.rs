//! Callout request/response types and HTTP executor.
//!
//! Defines the internal protocol between the host and providers for
//! running a single callout. Only HTTP fetch and git-open-repo are live
//! today; the remaining host-side git operations happen through bind
//! mounts over the cloned repo directory.

use crate::auth::AuthManager;
use crate::runtime::capability::CapabilityChecker;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Network,
    Timeout,
    Denied,
    NotFound,
    RateLimited,
    InvalidInput,
    Internal,
}

#[derive(Debug, Clone)]
pub enum CalloutResponse {
    HttpResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    GitRepoOpened(u64),
    BlobFetched(crate::runtime::blob::BlobRecord),
    ArchiveOpened(u64),
    BlobRead(Vec<u8>),
    Error {
        kind: ErrorKind,
        message: String,
        retryable: bool,
    },
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

    pub async fn execute_fetch(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> CalloutResponse {
        if let Err(e) = self.capability.check_url(url) {
            return CalloutResponse::Error {
                kind: ErrorKind::Denied,
                message: e.to_string(),
                retryable: false,
            };
        }

        let auth_headers = self.auth.headers_for_url(url);
        if auth_headers.is_empty() && self.auth.requires_auth_for_url(url) {
            return CalloutResponse::Error {
                kind: ErrorKind::Denied,
                message: format!("no credentials for {url}"),
                retryable: false,
            };
        }

        let Ok(reqwest_method) = reqwest::Method::from_str(method) else {
            return CalloutResponse::Error {
                kind: ErrorKind::Denied,
                message: format!("unsupported HTTP method: {method}"),
                retryable: false,
            };
        };

        let mut req = self.client.request(reqwest_method, url);
        let header_map = match build_header_map(&auth_headers, headers) {
            Ok(header_map) => header_map,
            Err(message) => {
                return CalloutResponse::Error {
                    kind: ErrorKind::Internal,
                    message,
                    retryable: false,
                };
            },
        };
        req = req.headers(header_map);
        if let Some(body) = body {
            req = req.body(owned_body(body));
        }

        match req.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let resp_headers = response_headers(response.headers());
                match response.bytes().await {
                    Ok(body) => CalloutResponse::HttpResponse {
                        status,
                        headers: resp_headers,
                        body: body.to_vec(),
                    },
                    Err(e) => CalloutResponse::Error {
                        kind: ErrorKind::Network,
                        message: e.to_string(),
                        retryable: true,
                    },
                }
            },
            Err(e) => CalloutResponse::Error {
                kind: ErrorKind::Network,
                message: e.to_string(),
                retryable: true,
            },
        }
    }
}

fn build_header_map(
    auth_headers: &[(String, String)],
    request_headers: &[(String, String)],
) -> Result<HeaderMap, String> {
    let mut header_map = HeaderMap::new();
    append_headers(&mut header_map, auth_headers, "auth")?;
    append_headers(&mut header_map, request_headers, "request")?;
    Ok(header_map)
}

fn append_headers(
    header_map: &mut HeaderMap,
    headers: &[(String, String)],
    source: &str,
) -> Result<(), String> {
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

fn response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
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

fn owned_body(body: &[u8]) -> reqwest::Body {
    // reqwest owns the request body across the async send path, so a borrowed
    // provider slice has to be copied into an owned body here.
    reqwest::Body::from(body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_header_map_rejects_invalid_header_name() {
        let error =
            build_header_map(&[], &[("bad header".to_string(), "value".to_string())]).unwrap_err();
        assert!(error.contains("invalid request header name"));
    }

    #[test]
    fn response_headers_drop_non_utf8_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-valid", HeaderValue::from_static("ok"));
        headers.insert("x-bytes", HeaderValue::from_bytes(b"\x80binary").unwrap());

        let response_headers = response_headers(&headers);

        assert_eq!(
            response_headers,
            vec![("x-valid".to_string(), "ok".to_string())]
        );
    }
}
