use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::flows::{credential_entry_from_token, scopes};
use crate::request::DeviceCodeLoginRequest;
use omnifs_workspace::authn::DevicePollCompat;
use omnifs_workspace::creds::CredentialEntry;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct DeviceCodePrompt {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_in: Duration,
}

impl DeviceCodePrompt {
    fn from_response(response: &oauth2::StandardDeviceAuthorizationResponse) -> DeviceCodePrompt {
        DeviceCodePrompt {
            verification_uri: response.verification_uri().to_string(),
            verification_uri_complete: response
                .verification_uri_complete()
                .map(|uri| uri.secret().to_owned()),
            user_code: response.user_code().secret().to_owned(),
            expires_in: response.expires_in(),
        }
    }
}

impl OAuthClient {
    pub async fn login_device_code<F, Fut>(
        &self,
        request: DeviceCodeLoginRequest,
        present: F,
    ) -> Result<CredentialEntry, AuthError>
    where
        F: FnOnce(DeviceCodePrompt) -> Fut,
        Fut: Future<Output = Result<(), AuthError>>,
    {
        let client = request.oauth.device_client(&request.flow)?;
        let mut exchange = client
            .exchange_device_code()
            .add_scopes(scopes(&request.oauth.scheme));
        for param in &request.oauth.scheme.extra_authorize_params {
            exchange = exchange.add_extra_param(&param.key, &param.value);
        }

        let details = exchange.request_async(&self.http).await?;
        present(DeviceCodePrompt::from_response(&details)).await?;

        let mut token_request = client.exchange_device_access_token(&details);
        for param in &request.oauth.scheme.extra_token_params {
            token_request = token_request.add_extra_param(&param.key, &param.value);
        }
        let polling_http = device_polling_http(&self.http, request.flow.device_poll_compat);
        let token = token_request
            .request_async(
                &polling_http,
                tokio::time::sleep,
                Some(details.expires_in()),
            )
            .await?;
        let entry = credential_entry_from_token(&token);
        Ok(entry)
    }
}

/// Wraps a [`reqwest::Client`] for device-code polling and, when the scheme
/// declares [`DevicePollCompat::ErrorInOkBody`], rewrites a non-RFC-8628
/// pending response: the token endpoint returns 200 OK with an error JSON
/// body (`{"error":"authorization_pending",...}`) while the user is still
/// approving the device. Without this shim oauth2 5.x parses the 200 as a
/// success token response, fails the JSON schema check, and the entire poll
/// loop bails on the first iteration with `Failed to parse server response`.
/// A no-op for [`DevicePollCompat::Rfc8628`] (the default), since a
/// conformant token endpoint never returns 200 while pending.
fn device_polling_http(http: &reqwest::Client, compat: DevicePollCompat) -> DevicePollingHttp {
    DevicePollingHttp(http.clone(), compat)
}

struct DevicePollingHttp(reqwest::Client, DevicePollCompat);

impl<'c> oauth2::AsyncHttpClient<'c> for DevicePollingHttp {
    type Error = oauth2::HttpClientError<reqwest::Error>;
    type Future =
        Pin<Box<dyn Future<Output = Result<oauth2::HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: oauth2::HttpRequest) -> Self::Future {
        let inner = self.0.clone();
        let compat = self.1;
        Box::pin(async move {
            let response =
                <reqwest::Client as oauth2::AsyncHttpClient<'_>>::call(&inner, request).await?;
            Ok(match compat {
                DevicePollCompat::Rfc8628 => response,
                DevicePollCompat::ErrorInOkBody => rewrite_pending_to_error_status(response),
            })
        })
    }
}

/// If the response is 200 OK but the JSON body has an `error` field, rewrite
/// to 400 so oauth2's response handling routes it through the error-response
/// path (where `authorization_pending` and `slow_down` continue the poll).
/// Only called when the scheme declares `DevicePollCompat::ErrorInOkBody`;
/// conformant providers (`Rfc8628`) never reach this function.
pub(crate) fn rewrite_pending_to_error_status(
    response: oauth2::HttpResponse,
) -> oauth2::HttpResponse {
    if response.status() != http::StatusCode::OK {
        return response;
    }
    let Some(content_type) = response.headers().get(http::header::CONTENT_TYPE) else {
        return response;
    };
    let ct = content_type.to_str().unwrap_or("").to_ascii_lowercase();
    if !ct.starts_with("application/json") {
        return response;
    }
    let body = response.body();
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        return response;
    };
    if parsed
        .get("error")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts.status = http::StatusCode::BAD_REQUEST;
    http::Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_pending_status_handling() {
        let pending_body =
            br#"{"error":"authorization_pending","error_description":"..."}"#.to_vec();
        let pending = http::Response::builder()
            .status(http::StatusCode::OK)
            .header(
                http::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .body(pending_body.clone())
            .unwrap();
        let rewritten = rewrite_pending_to_error_status(pending);
        assert_eq!(rewritten.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(rewritten.body(), &pending_body);

        let token_body = br#"{"access_token":"x","token_type":"bearer"}"#.to_vec();
        let token = http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(token_body.clone())
            .unwrap();
        let rewritten = rewrite_pending_to_error_status(token);
        assert_eq!(rewritten.status(), http::StatusCode::OK);
        assert_eq!(rewritten.body(), &token_body);

        let form_body = b"error=authorization_pending".to_vec();
        let form = http::Response::builder()
            .status(http::StatusCode::OK)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body.clone())
            .unwrap();
        let rewritten = rewrite_pending_to_error_status(form);
        assert_eq!(rewritten.status(), http::StatusCode::OK);
    }
}
