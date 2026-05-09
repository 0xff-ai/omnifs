//! Callout request/response types and HTTP executor.
//!
//! Defines the internal protocol between the host and providers for
//! running a single callout. Only HTTP fetch and git-open-repo are live
//! today; the remaining host-side git operations happen through bind
//! mounts over the cloned repo directory.

use crate::auth::AuthManager;
use crate::runtime::capability::CapabilityChecker;
use crate::runtime::http_headers::{build_header_map, decode_response_headers};
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
                let resp_headers = decode_response_headers(response.headers());
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

fn owned_body(body: &[u8]) -> reqwest::Body {
    // reqwest owns the request body across the async send path, so a borrowed
    // provider slice has to be copied into an owned body here.
    reqwest::Body::from(body.to_vec())
}
