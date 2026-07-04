use oauth2::ErrorResponse;
use std::fmt::Display;

type HttpRequestTokenError<E> =
    oauth2::RequestTokenError<oauth2::HttpClientError<reqwest::Error>, E>;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    RequestConfig(String),
    #[error("oauth client_id is required")]
    MissingClientId,
    #[error("oauth client_secret is required for this token endpoint auth method")]
    MissingClientSecret,
    #[error("oauth loopback flow requires an opener")]
    MissingOpener,
    #[error("oauth state did not match pending login")]
    StateMismatch,
    #[error("oauth callback did not include authorization code")]
    MissingCode,
    #[error("oauth callback did not include access token")]
    MissingAccessToken,
    #[error("oauth callback did not include state")]
    MissingState,
    #[error("oauth callback request is invalid")]
    InvalidCallback,
    #[error("oauth loopback redirect URI must use 127.0.0.1 or localhost with an explicit port")]
    InvalidRedirectUri,
    #[error("oauth callback returned error {error}: {description:?}")]
    AuthorizationError {
        error: String,
        description: Option<String>,
    },
    #[error("oauth token endpoint error: {error}: {description:?}")]
    TokenEndpoint {
        error: String,
        description: Option<String>,
    },
    #[error("oauth revocation endpoint error: {0}")]
    RevocationEndpoint(String),
    #[error("oauth url error: {0}")]
    Url(#[from] url::ParseError),
    #[error("oauth client configuration error: {0}")]
    Configuration(#[from] oauth2::ConfigurationError),
    #[error("oauth http transport error: {0}")]
    HttpTransport(String),
    #[error("http client error: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("browser open failed: {0}")]
    BrowserOpen(String),
}

trait RequestFailure: Sized {
    fn server_response(response: Self) -> AuthError;
    fn request_failed(description: String) -> AuthError;
}

fn token_endpoint_failure<T>(response: &oauth2::StandardErrorResponse<T>) -> AuthError
where
    T: oauth2::ErrorResponseType + Display,
{
    AuthError::TokenEndpoint {
        error: response.error().to_string(),
        description: response.error_description().map(ToString::to_string),
    }
}

fn token_request_failed(description: String) -> AuthError {
    AuthError::TokenEndpoint {
        error: "request_failed".to_owned(),
        description: Some(description),
    }
}

impl RequestFailure for oauth2::StandardErrorResponse<oauth2::basic::BasicErrorResponseType> {
    fn server_response(response: Self) -> AuthError {
        token_endpoint_failure(&response)
    }

    fn request_failed(description: String) -> AuthError {
        token_request_failed(description)
    }
}

impl RequestFailure for oauth2::DeviceCodeErrorResponse {
    fn server_response(response: Self) -> AuthError {
        token_endpoint_failure(&response)
    }

    fn request_failed(description: String) -> AuthError {
        token_request_failed(description)
    }
}

impl RequestFailure for oauth2::StandardErrorResponse<oauth2::RevocationErrorResponseType> {
    fn server_response(response: Self) -> AuthError {
        AuthError::RevocationEndpoint(response.error().to_string())
    }

    fn request_failed(description: String) -> AuthError {
        AuthError::RevocationEndpoint(description)
    }
}

impl<E> From<HttpRequestTokenError<E>> for AuthError
where
    E: RequestFailure + ErrorResponse + 'static,
    HttpRequestTokenError<E>: Display,
{
    fn from(value: HttpRequestTokenError<E>) -> Self {
        request_token_error(value)
    }
}

fn request_token_error<E>(value: HttpRequestTokenError<E>) -> AuthError
where
    E: RequestFailure + ErrorResponse + 'static,
    HttpRequestTokenError<E>: Display,
{
    match value {
        oauth2::RequestTokenError::ServerResponse(response) => E::server_response(response),
        oauth2::RequestTokenError::Request(err) => err.into(),
        other => E::request_failed(other.to_string()),
    }
}

impl From<oauth2::HttpClientError<reqwest::Error>> for AuthError {
    fn from(err: oauth2::HttpClientError<reqwest::Error>) -> Self {
        match err {
            oauth2::HttpClientError::Reqwest(err) => Self::HttpClient(*err),
            other => Self::HttpTransport(other.to_string()),
        }
    }
}
