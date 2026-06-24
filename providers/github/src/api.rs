//! The typed `GitHubApi` endpoint and its rate-limit classifier.
//!
//! GitHub signals rate limits off the status line (a `403` with
//! `x-ratelimit-remaining: 0`, or secondary-limit / abuse-detection bodies),
//! which the SDK's default `429` handling cannot see. [`GitHubApi`] overrides
//! [`EndpointHooks::classify`] to map those to a typed `RateLimited` error, so
//! every `cx.endpoint(GitHubApi)` terminal arms the breaker and surfaces the
//! retry window. Every other status falls through to the SDK's default
//! `error_for_status` mapping.

use core::time::Duration;
use http::{Response, StatusCode};
use omnifs_sdk::endpoint::EndpointHooks;
use omnifs_sdk::error::ProviderError;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(omnifs_sdk::Endpoint)]
#[endpoint(
    base = "https://api.github.com",
    default_header = "X-GitHub-Api-Version: 2022-11-28",
    default_header = "Accept: application/vnd.github+json",
    hooks
)]
pub struct GitHubApi;

impl EndpointHooks for GitHubApi {
    /// Map a GitHub rate-limit response to a typed `RateLimited` error so the
    /// endpoint breaker arms on it; `None` for everything else, leaving the
    /// SDK's default 4xx/5xx mapping in charge.
    fn classify(&self, resp: &Response<Vec<u8>>) -> Option<ProviderError> {
        if !is_rate_limited(resp) {
            return None;
        }
        let retry_after = parse_github_retry(resp);
        Some(ProviderError::rate_limited(rate_limit_message(resp)).with_retry_after(retry_after))
    }
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
