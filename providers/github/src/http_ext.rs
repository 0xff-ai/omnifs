use omnifs_sdk::Cx;
use omnifs_sdk::error::ProviderError;
use omnifs_sdk::http::Request;
use omnifs_sdk::omnifs::provider::types::HttpResponse;
use serde::de::DeserializeOwned;

use crate::Result;
use crate::State;
use crate::{API_BASE, parse_model};

pub(crate) trait GithubHttpExt {
    fn github_get(&self, path: impl AsRef<str>) -> GithubRequest<'_>;
    fn github_json_request(&self, path: impl AsRef<str>) -> GithubRequest<'_>;
    fn github_json<T>(
        &self,
        path: impl AsRef<str>,
    ) -> impl core::future::Future<Output = Result<T>>
    where
        T: DeserializeOwned;
}

pub(crate) struct GithubRequest<'cx> {
    request: Request<'cx, State>,
}

impl<'cx> GithubRequest<'cx> {
    fn new(request: Request<'cx, State>) -> Self {
        Self { request }
    }

    #[must_use]
    pub(crate) fn header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            request: self.request.header(name, value),
        }
    }

    pub(crate) async fn send(self) -> Result<HttpResponse> {
        let response = self.request.send_raw().await?;
        if response.status < 400 {
            Ok(response)
        } else {
            Err(GithubHttpError(&response).into())
        }
    }

    pub(crate) async fn send_body(self) -> Result<Vec<u8>> {
        Ok(self.send().await?.body)
    }
}

impl GithubHttpExt for Cx<State> {
    fn github_get(&self, path: impl AsRef<str>) -> GithubRequest<'_> {
        GithubRequest::new(
            self.http()
                .get(format!("{API_BASE}{}", path.as_ref()))
                .header("X-GitHub-Api-Version", "2022-11-28"),
        )
    }

    fn github_json_request(&self, path: impl AsRef<str>) -> GithubRequest<'_> {
        self.github_get(path)
            .header("Accept", "application/vnd.github+json")
    }

    async fn github_json<T>(&self, path: impl AsRef<str>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let body = self.github_json_request(path).send_body().await?;
        parse_model(&body)
    }
}

struct GithubHttpError<'a>(&'a HttpResponse);

impl From<GithubHttpError<'_>> for ProviderError {
    fn from(error: GithubHttpError<'_>) -> Self {
        let response = error.0;
        if GithubRateLimit::try_from(response).is_ok() {
            return ProviderError::rate_limited(format!("HTTP {}", response.status));
        }
        ProviderError::from_http_status(response.status)
    }
}

struct GithubRateLimit;

impl TryFrom<&HttpResponse> for GithubRateLimit {
    type Error = ();

    fn try_from(response: &HttpResponse) -> core::result::Result<Self, Self::Error> {
        if response.status != 403 {
            return Err(());
        }
        let rate_remaining_zero = response.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("x-ratelimit-remaining") && header.value == "0"
        });
        let body = String::from_utf8_lossy(&response.body).to_ascii_lowercase();
        if rate_remaining_zero || body.contains("rate limit") || body.contains("abuse detection") {
            Ok(Self)
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_sdk::omnifs::provider::types::Header;

    fn response(status: u16, headers: Vec<Header>, body: &'static [u8]) -> HttpResponse {
        HttpResponse {
            status,
            headers,
            body: body.to_vec(),
        }
    }

    #[test]
    fn zero_remaining_http_403_is_rate_limited_for_github() {
        let response = response(
            403,
            vec![Header {
                name: "x-ratelimit-remaining".to_string(),
                value: "0".to_string(),
            }],
            br#"{"message":"forbidden"}"#,
        );
        let error = ProviderError::from(GithubHttpError(&response));
        assert_eq!(
            error.kind(),
            omnifs_sdk::error::ProviderErrorKind::RateLimited
        );
        assert!(error.is_retryable());
    }

    #[test]
    fn secondary_rate_limit_body_is_retryable_without_headers() {
        let response = response(
            403,
            vec![],
            br#"{"message":"You have exceeded a secondary rate limit"}"#,
        );
        let error = ProviderError::from(GithubHttpError(&response));
        assert_eq!(
            error.kind(),
            omnifs_sdk::error::ProviderErrorKind::RateLimited
        );
        assert!(error.is_retryable());
    }

    #[test]
    fn abuse_detection_body_is_retryable() {
        let response = response(
            403,
            vec![],
            br#"{"message":"You have triggered an abuse detection mechanism"}"#,
        );
        let error = ProviderError::from(GithubHttpError(&response));
        assert_eq!(
            error.kind(),
            omnifs_sdk::error::ProviderErrorKind::RateLimited
        );
        assert!(error.is_retryable());
    }
}
