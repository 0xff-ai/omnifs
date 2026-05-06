use http::{Response, StatusCode};
use omnifs_sdk::Cx;
use omnifs_sdk::error::ProviderError;
use omnifs_sdk::http::{Request, ResponseExt};
use serde::de::DeserializeOwned;

use crate::Result;
use crate::State;
use crate::{API_BASE, parse_model};

pub(crate) trait GithubHttpExt {
    fn github_get(&self, path: impl AsRef<str>) -> Request<'_, State>;
    fn github_json_request(&self, path: impl AsRef<str>) -> Request<'_, State>;
    fn github_json<T>(
        &self,
        path: impl AsRef<str>,
    ) -> impl core::future::Future<Output = Result<T>>
    where
        T: DeserializeOwned;
}

impl GithubHttpExt for Cx<State> {
    fn github_get(&self, path: impl AsRef<str>) -> Request<'_, State> {
        self.http()
            .get(format!("{API_BASE}{}", path.as_ref()))
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn github_json_request(&self, path: impl AsRef<str>) -> Request<'_, State> {
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
}

/// Map a GitHub HTTP response to a typed `ProviderError`. Distinguishes
/// rate-limit signals (`x-ratelimit-remaining: 0`, secondary-rate-limit
/// or abuse-detection bodies) from generic 4xx/5xx so callers can retry
/// rate-limited responses.
pub(crate) fn github_check_status(resp: Response<Vec<u8>>) -> Result<Response<Vec<u8>>> {
    if is_rate_limited(&resp) {
        return Err(ProviderError::rate_limited(format!(
            "HTTP {}",
            resp.status().as_u16()
        )));
    }
    resp.error_for_status()
}

fn is_rate_limited(resp: &Response<Vec<u8>>) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_sdk::error::ProviderErrorKind;

    fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Response<Vec<u8>> {
        let mut builder = Response::builder().status(status);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(body.to_vec()).expect("response builder")
    }

    fn err_kind(resp: Response<Vec<u8>>) -> ProviderErrorKind {
        github_check_status(resp).unwrap_err().kind()
    }

    #[test]
    fn zero_remaining_http_403_is_rate_limited() {
        let resp = response(
            403,
            &[("x-ratelimit-remaining", "0")],
            br#"{"message":"forbidden"}"#,
        );
        let error = github_check_status(resp).unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        assert!(error.is_retryable());
    }

    #[test]
    fn secondary_rate_limit_body_is_retryable_without_headers() {
        let resp = response(
            403,
            &[],
            br#"{"message":"You have exceeded a secondary rate limit"}"#,
        );
        assert_eq!(err_kind(resp), ProviderErrorKind::RateLimited);
    }

    #[test]
    fn abuse_detection_body_is_retryable() {
        let resp = response(
            403,
            &[],
            br#"{"message":"You have triggered an abuse detection mechanism"}"#,
        );
        assert_eq!(err_kind(resp), ProviderErrorKind::RateLimited);
    }

    #[test]
    fn passes_through_2xx_responses() {
        let resp = response(200, &[], b"ok");
        assert!(github_check_status(resp).is_ok());
    }
}
