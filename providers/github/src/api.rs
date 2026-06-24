//! GitHub REST transport (`GithubRest`) and the typed `GitHubApi` endpoint.

#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "https://api.github.com")]
#[endpoint(default_header = "X-GitHub-Api-Version: 2022-11-28")]
#[endpoint(default_header = "Accept: application/vnd.github+json")]
pub struct GitHubApi;

use core::time::Duration;
use http::{Response, StatusCode};
use omnifs_sdk::Cx;
use omnifs_sdk::error::ProviderError;
use omnifs_sdk::http::{Request, ResponseExt};
use omnifs_sdk::object::{Canonical, Load, Validator};
use serde::de::DeserializeOwned;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::{API_BASE, parse_model};

pub(crate) trait GithubRest<S> {
    fn github_get(&self, path: impl AsRef<str>) -> Request<'_, S>;
    fn github_json_request(&self, path: impl AsRef<str>) -> Request<'_, S>;
    fn github_json<T>(
        &self,
        path: impl AsRef<str>,
    ) -> impl core::future::Future<Output = Result<T>>
    where
        T: DeserializeOwned;
    fn github_load<T>(
        &self,
        path: impl AsRef<str>,
        since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<T>>>
    where
        T: DeserializeOwned;
}

impl<S> GithubRest<S> for Cx<S> {
    fn github_get(&self, path: impl AsRef<str>) -> Request<'_, S> {
        self.http()
            .get(format!("{API_BASE}{}", path.as_ref()))
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn github_json_request(&self, path: impl AsRef<str>) -> Request<'_, S> {
        self.github_get(path)
            .header("Accept", "application/vnd.github+json")
    }

    async fn github_json<T>(&self, path: impl AsRef<str>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let resp = self.github_json_request(path).send().await?;
        let resp = github_check_status(resp)?;
        parse_model(resp.body())
    }

    async fn github_load<T>(
        &self,
        path: impl AsRef<str>,
        since: Option<Validator>,
    ) -> Result<Load<T>>
    where
        T: DeserializeOwned,
    {
        let mut request = self.github_json_request(path);
        if let Some(validator) = &since {
            request = request.header("if-none-match", validator.as_str());
        }
        let resp = request.send().await?;
        match resp.status() {
            StatusCode::NOT_MODIFIED => Ok(Load::Unchanged),
            StatusCode::NOT_FOUND => Ok(Load::NotFound),
            _ => {
                let resp = github_check_status(resp)?;
                let value = parse_model(resp.body())?;
                let validator = resp
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(Validator::from);
                Ok(Load::fresh(
                    value,
                    Canonical {
                        bytes: resp.body().clone(),
                        validator,
                    },
                ))
            },
        }
    }
}

/// Map a GitHub HTTP response to a typed `ProviderError`. Distinguishes
/// rate-limit signals (`x-ratelimit-remaining: 0`, secondary-rate-limit
/// or abuse-detection bodies) from generic 4xx/5xx so callers can retry
/// rate-limited responses.
pub(crate) fn github_check_status(resp: Response<Vec<u8>>) -> Result<Response<Vec<u8>>> {
    if is_rate_limited(&resp) {
        let retry_after = parse_github_retry(&resp);
        // GitHub's 403-style limits are not generic 429s, so the SDK send path
        // cannot classify them. Arm the shared breaker before returning.
        omnifs_sdk::note_rate_limited(API_BASE, retry_after);
        return Err(
            ProviderError::rate_limited(rate_limit_message(&resp)).with_retry_after(retry_after)
        );
    }
    resp.error_for_status()
}

/// Structured backoff window for a github rate-limit response. Prefers an
/// explicit `Retry-After` (secondary limits / abuse detection); falls back to
/// the primary limit's `x-ratelimit-reset` epoch minus now. Without this the
/// host applies its short default cooldown and re-hammers github every few
/// seconds for the whole (up to ~1h) primary-limit window instead of waiting
/// out the real reset.
fn parse_github_retry(resp: &Response<Vec<u8>>) -> Option<Duration> {
    if let Some(secs) = header_u64(resp, "retry-after") {
        return Some(Duration::from_secs(secs));
    }
    let reset_epoch = header_u64(resp, "x-ratelimit-reset")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    reset_epoch.checked_sub(now).map(Duration::from_secs)
}

fn header_u64(resp: &Response<Vec<u8>>, header: &str) -> Option<u64> {
    resp.headers()
        .get(header)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn is_rate_limited(resp: &Response<Vec<u8>>) -> bool {
    if resp.status() == StatusCode::TOO_MANY_REQUESTS {
        return true;
    }
    if resp.status() != StatusCode::FORBIDDEN {
        return false;
    }
    let zero_remaining = resp.headers().iter().any(|(name, value)| {
        name.as_str().eq_ignore_ascii_case("x-ratelimit-remaining") && value == "0"
    });
    if zero_remaining {
        return true;
    }
    let body = String::from_utf8_lossy(resp.body()).to_ascii_lowercase();
    body.contains("rate limit") || body.contains("abuse detection")
}

fn rate_limit_message(resp: &Response<Vec<u8>>) -> String {
    let mut message = format!("GitHub API rate limited: HTTP {}", resp.status().as_u16());
    append_header_hint(resp, &mut message, "retry-after", "retry_after");
    append_header_hint(resp, &mut message, "x-ratelimit-reset", "reset_epoch");
    append_header_hint(resp, &mut message, "x-ratelimit-resource", "resource");
    message
}

fn append_header_hint(resp: &Response<Vec<u8>>, message: &mut String, header: &str, label: &str) {
    if let Some(value) = resp
        .headers()
        .get(header)
        .and_then(|value| value.to_str().ok())
    {
        message.push_str("; ");
        message.push_str(label);
        message.push('=');
        message.push_str(value);
    }
}
