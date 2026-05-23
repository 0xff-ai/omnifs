//! HTTP callout result extraction and typed async HTTP builders.
//!
//! `send()` returns `http::Response<Vec<u8>>`; use
//! `ResponseExt::error_for_status` for default 4xx/5xx mapping or
//! inspect the response directly for custom handling.

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::omnifs::provider::types::{Callout, CalloutResult, Header, HttpRequest, HttpResponse};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};

pub struct Builder<'cx, S> {
    cx: &'cx Cx<S>,
}

impl<'cx, S> Builder<'cx, S> {
    pub fn new(cx: &'cx Cx<S>) -> Self {
        Self { cx }
    }

    pub fn get(self, url: impl Into<String>) -> Request<'cx, S> {
        Request::new(self.cx, Method::GET, url)
    }

    pub fn post(self, url: impl Into<String>) -> Request<'cx, S> {
        Request::new(self.cx, Method::POST, url)
    }
}

pub struct Request<'cx, S> {
    cx: &'cx Cx<S>,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    error: Option<ProviderError>,
}

impl<'cx, S> Request<'cx, S> {
    fn new(cx: &'cx Cx<S>, method: Method, url: impl Into<String>) -> Self {
        Self {
            cx,
            method,
            url: url.into(),
            headers: HeaderMap::new(),
            body: None,
            error: None,
        }
    }

    /// Append a header. Invalid names or values are recorded as a sticky
    /// error so the chainable builder stays infallible; the error is
    /// surfaced from `send`.
    #[must_use]
    pub fn header(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        if self.error.is_some() {
            return self;
        }
        let name = match HeaderName::try_from(name.as_ref()) {
            Ok(n) => n,
            Err(e) => {
                self.error = Some(ProviderError::invalid_input(format!(
                    "invalid header name: {e}"
                )));
                return self;
            },
        };
        let value = match HeaderValue::try_from(value.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(ProviderError::invalid_input(format!(
                    "invalid header value: {e}"
                )));
                return self;
            },
        };
        self.headers.append(name, value);
        self
    }

    /// Set the raw request body bytes.
    #[must_use]
    pub fn body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.body = Some(bytes.into());
        self
    }

    /// Serialize `value` as JSON, use it as the body, and set
    /// `Content-Type: application/json` unless the caller has already set
    /// a `Content-Type` header. Serialization failures are stored as a
    /// sticky error and surfaced from `send`.
    #[must_use]
    pub fn json<T: serde::Serialize + ?Sized>(mut self, value: &T) -> Self {
        if self.error.is_some() {
            return self;
        }
        match serde_json::to_vec(value) {
            Ok(bytes) => {
                self.body = Some(bytes);
                if !self.headers.contains_key(http::header::CONTENT_TYPE) {
                    self.headers.insert(
                        http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                }
            },
            Err(e) => {
                self.error = Some(ProviderError::invalid_input(format!(
                    "failed to serialize json body: {e}"
                )));
            },
        }
        self
    }

    pub fn send(self) -> CalloutFuture<'cx, S, Response<Vec<u8>>> {
        if let Some(error) = self.error {
            return CalloutFuture::ready_error(self.cx, error);
        }
        let wit_request = HttpRequest {
            method: self.method.as_str().to_string(),
            url: self.url,
            headers: WitHeaders(&self.headers).into(),
            body: self.body,
        };
        CalloutFuture::new(
            self.cx,
            Callout::Fetch(wit_request),
            |result| match result {
                CalloutResult::HttpResponse(resp) => resp.try_into(),
                CalloutResult::CalloutError(e) => Err(e.into()),
                _ => Err(ProviderError::internal("unexpected callout result type")),
            },
        )
    }

    /// Convert this request into a blob-fetch. The response body lands
    /// in the host's blob cache rather than crossing the WIT, and the
    /// returned [`crate::blob::BlobRef`] can be handed to
    /// `FileContent::blob`, `cx.archives().open(...)`, or
    /// `cx.blob(id).read()`.
    pub fn into_blob(self) -> BlobRequest<'cx, S> {
        BlobRequest {
            inner: self,
            cache_key: None,
        }
    }
}

#[must_use]
pub struct BlobRequest<'cx, S> {
    inner: Request<'cx, S>,
    cache_key: Option<String>,
}

impl<'cx, S> BlobRequest<'cx, S> {
    pub fn header(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.inner = self.inner.header(name, value);
        self
    }

    pub fn body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.inner = self.inner.body(bytes);
        self
    }

    /// Provider-supplied cache key. Two callers using the same key
    /// share the same blob; the host fetches once.
    pub fn with_cache_key(mut self, key: impl Into<String>) -> Self {
        self.cache_key = Some(key.into());
        self
    }

    pub fn send(self) -> CalloutFuture<'cx, S, crate::blob::BlobRef> {
        if let Some(error) = self.inner.error {
            return CalloutFuture::ready_error(self.inner.cx, error);
        }
        let Some(cache_key) = self.cache_key else {
            return CalloutFuture::ready_error(
                self.inner.cx,
                ProviderError::invalid_input(
                    "blob fetch requires a cache key (call .with_cache_key)",
                ),
            );
        };
        let callout = crate::blob::blob_fetch_callout(
            self.inner.method.as_str().to_string(),
            self.inner.url,
            WitHeaders(&self.inner.headers).into(),
            self.inner.body,
            cache_key,
        );
        CalloutFuture::new(self.inner.cx, callout, crate::blob::extract_blob)
    }
}

struct WitHeaders<'a>(&'a HeaderMap);

impl From<WitHeaders<'_>> for Vec<Header> {
    fn from(WitHeaders(map): WitHeaders<'_>) -> Self {
        map.iter()
            .map(|(name, value)| Header {
                name: name.as_str().to_string(),
                // HeaderValue is opaque bytes; `try_from(&str)` at insert
                // time rejects non-visible-ASCII, so any value reaching
                // this point is a valid &str by builder invariant.
                value: value
                    .to_str()
                    .expect("HeaderValue inserted via builder is visible ASCII")
                    .to_string(),
            })
            .collect()
    }
}

impl TryFrom<HttpResponse> for Response<Vec<u8>> {
    type Error = ProviderError;

    fn try_from(resp: HttpResponse) -> std::result::Result<Self, Self::Error> {
        let status = StatusCode::from_u16(resp.status)
            .map_err(|e| ProviderError::internal(format!("invalid response status: {e}")))?;
        let mut builder = Response::builder().status(status);
        // Drop malformed headers from the host rather than fail the whole
        // response; status and body are the load-bearing fields.
        for hdr in &resp.headers {
            let (Ok(name), Ok(value)) = (
                HeaderName::try_from(&hdr.name),
                HeaderValue::try_from(&hdr.value),
            ) else {
                continue;
            };
            builder = builder.header(name, value);
        }
        builder
            .body(resp.body)
            .map_err(|e| ProviderError::internal(format!("response builder: {e}")))
    }
}

/// Default 4xx/5xx mapping to `ProviderError`.
pub trait ResponseExt: Sized {
    fn error_for_status(self) -> Result<Self>;
    /// Same check as `error_for_status` without consuming the response.
    /// Use when custom mapping (rate-limit detection, retry signals)
    /// needs to inspect the body or headers on the error path.
    fn error_for_status_ref(&self) -> Result<()>;
}

impl ResponseExt for Response<Vec<u8>> {
    fn error_for_status(self) -> Result<Self> {
        self.error_for_status_ref()?;
        Ok(self)
    }

    fn error_for_status_ref(&self) -> Result<()> {
        let status = self.status();
        if status.is_client_error() || status.is_server_error() {
            Err(status_error(self))
        } else {
            Ok(())
        }
    }
}

fn status_error(resp: &Response<Vec<u8>>) -> ProviderError {
    let status = resp.status();
    if status != StatusCode::TOO_MANY_REQUESTS {
        return ProviderError::from_http_status(status.as_u16());
    }

    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok());
    let message = match retry_after {
        Some(value) => format!("HTTP 429; retry_after={value}"),
        None => "HTTP 429".to_string(),
    };
    ProviderError::rate_limited(message)
}

pub enum CalloutFuture<'cx, S, T> {
    Pending {
        cx: &'cx Cx<S>,
        callout: Option<Callout>,
        extract: fn(CalloutResult) -> Result<T>,
    },
    Ready(Option<Result<T>>),
}

// `Ready` carries `T`, so the auto-derive would gate `Unpin` on
// `T: Unpin`; this future never relies on structural pinning of any
// variant field, so the manual impl is sound regardless of `T`.
impl<S, T> Unpin for CalloutFuture<'_, S, T> {}

impl<'cx, S, T> CalloutFuture<'cx, S, T> {
    pub(crate) fn new(
        cx: &'cx Cx<S>,
        callout: Callout,
        extract: fn(CalloutResult) -> Result<T>,
    ) -> Self {
        Self::Pending {
            cx,
            callout: Some(callout),
            extract,
        }
    }

    pub(crate) fn ready_error(_cx: &'cx Cx<S>, error: ProviderError) -> Self {
        Self::Ready(Some(Err(error)))
    }
}

impl<S, T> Future for CalloutFuture<'_, S, T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut *self {
            Self::Ready(slot) => match slot.take() {
                Some(result) => Poll::Ready(result),
                None => Poll::Pending,
            },
            Self::Pending {
                cx,
                callout,
                extract,
            } => {
                if let Some(callout) = callout.take() {
                    cx.push_yielded(callout);
                    return Poll::Pending;
                }
                if let Some(result) = cx.pop_delivered() {
                    return Poll::Ready(extract(result));
                }
                Poll::Pending
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::task::Waker;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn drive_once<F: Future>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut ctx = Context::from_waker(waker);
        future.as_mut().poll(&mut ctx)
    }

    fn take_single_fetch<S>(cx: &Cx<S>) -> HttpRequest {
        let mut yielded = cx.take_yielded_callouts();
        assert_eq!(yielded.len(), 1, "expected exactly one yielded callout");
        match yielded.remove(0) {
            Callout::Fetch(req) => req,
            other => panic!("expected Callout::Fetch, got {other:?}"),
        }
    }

    #[test]
    fn json_serializes_value_and_sets_content_type_exactly_once() {
        #[derive(serde::Serialize)]
        struct Payload<'a> {
            name: &'a str,
            count: u32,
        }

        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let payload = Payload {
            name: "alice",
            count: 3,
        };
        let mut fut = Box::pin(
            cx.http()
                .post("https://example.test/api")
                .json(&payload)
                .send(),
        );
        assert!(matches!(drive_once(&mut fut), Poll::Pending));

        let req = take_single_fetch(&cx);
        assert_eq!(req.method, "POST");
        let body = req.body.expect("json() must set a body");
        assert_eq!(body, br#"{"name":"alice","count":3}"#.to_vec());

        let ct_headers: Vec<&Header> = req
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("content-type"))
            .collect();
        assert_eq!(ct_headers.len(), 1, "Content-Type must be set exactly once");
        assert_eq!(ct_headers[0].value, "application/json");
    }

    #[test]
    fn json_respects_caller_set_content_type() {
        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let mut fut = Box::pin(
            cx.http()
                .post("https://example.test/api")
                .header("content-type", "application/vnd.custom+json")
                .json(&serde_json::json!({"k": "v"}))
                .send(),
        );
        assert!(matches!(drive_once(&mut fut), Poll::Pending));

        let req = take_single_fetch(&cx);
        let ct_headers: Vec<&Header> = req
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("content-type"))
            .collect();
        assert_eq!(ct_headers.len(), 1, "Content-Type must not be duplicated");
        assert_eq!(ct_headers[0].value, "application/vnd.custom+json");
    }

    #[test]
    fn invalid_header_name_surfaces_at_send() {
        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let mut fut = Box::pin(
            cx.http()
                .get("https://example.test/api")
                .header("invalid name with space", "v")
                .send(),
        );

        match drive_once(&mut fut) {
            Poll::Ready(Err(_)) => {},
            other => panic!("expected immediate error, got {other:?}"),
        }
        assert!(
            cx.take_yielded_callouts().is_empty(),
            "no callout should be issued when builder failed"
        );
    }

    #[test]
    fn first_sticky_error_wins() {
        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let mut fut = Box::pin(
            cx.http()
                .post("https://example.test/api")
                .header("invalid name with space", "v")
                .header("X-Bad", "value\nwith newline")
                .send(),
        );

        let Poll::Ready(Err(error)) = drive_once(&mut fut) else {
            panic!("expected immediate error");
        };
        assert_eq!(error.kind(), crate::error::ProviderErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("invalid header name"),
            "expected first failure (header name), got {error}"
        );
    }

    #[test]
    fn response_ext_maps_429_to_retryable_rate_limit() {
        let resp = Response::builder()
            .status(429)
            .header("retry-after", "3")
            .body(Vec::new())
            .expect("response builder");

        let error = resp.error_for_status_ref().unwrap_err();

        assert_eq!(error.kind(), crate::error::ProviderErrorKind::RateLimited);
        assert!(error.is_retryable());
        assert_eq!(error.message(), "HTTP 429; retry_after=3");
    }
}

/// Endpoint a provider talks to via HTTP callouts.
///
/// `Tcp` is a normal HTTP base URL (host plus optional port and a
/// stable scheme: `https://api.example.com`). `Unix` carries an
/// absolute socket path; the SDK encodes it into the `unix://` URL
/// shape the host's executor recognises, so the provider author
/// never builds the encoded URL by hand.
#[derive(Clone, Debug)]
pub enum HttpEndpoint {
    Tcp(String),
    Unix(std::path::PathBuf),
}

impl HttpEndpoint {
    /// Parse an endpoint from a string. `unix:///absolute/path`
    /// produces `HttpEndpoint::Unix`; anything else is treated as a
    /// TCP base URL.
    pub fn parse(s: impl AsRef<str>) -> Self {
        let s = s.as_ref();
        if let Some(rest) = s.strip_prefix("unix://") {
            return Self::Unix(std::path::PathBuf::from(rest));
        }
        Self::Tcp(s.to_string())
    }

    /// Build a callout URL by combining the endpoint with an HTTP
    /// `path` and an optional query, ready to hand to `cx.http().get(..)`.
    ///
    /// `path` should start with `/`. `query` is a slice of `(name, value)`
    /// pairs that are percent-encoded as a standard `application/x-www-form-urlencoded`
    /// query string (no leading `?`; the helper adds it).
    pub fn build_url(&self, path: &str, query: &[(&str, &str)]) -> String {
        let qs = render_query(query);
        match self {
            Self::Tcp(base) => {
                let mut url = base.trim_end_matches('/').to_string();
                if !path.starts_with('/') {
                    url.push('/');
                }
                url.push_str(path);
                if !qs.is_empty() {
                    url.push('?');
                    url.push_str(&qs);
                }
                url
            },
            Self::Unix(socket) => {
                // Encode the absolute socket path as hex bytes so the
                // URL has a stable host segment and the host's
                // executor decodes it the same way `hyperlocal` does.
                let socket_str = socket.to_string_lossy();
                let host = hex::encode(socket_str.as_bytes());
                let mut url = format!("unix://{host}");
                if !path.starts_with('/') {
                    url.push('/');
                }
                url.push_str(path);
                if !qs.is_empty() {
                    url.push('?');
                    url.push_str(&qs);
                }
                url
            },
        }
    }
}

fn render_query(query: &[(&str, &str)]) -> String {
    // Standard `application/x-www-form-urlencoded` encoding via the
    // `url` crate's serializer: the unreserved set passes through,
    // ` ` becomes `+`, everything else is percent-encoded.
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in query {
        serializer.append_pair(k, v);
    }
    serializer.finish()
}

#[cfg(test)]
mod endpoint_tests {
    use super::HttpEndpoint;

    #[test]
    fn unix_endpoint_hex_encodes_socket_in_host_segment() {
        let ep = HttpEndpoint::parse("unix:///var/run/docker.sock");
        let url = ep.build_url("/v1.43/containers/json", &[("all", "true")]);
        assert!(
            url.starts_with("unix://"),
            "url should keep unix scheme: {url}"
        );
        let after = &url["unix://".len()..];
        let (host, rest) = after.split_once('/').expect("host and path");
        let path_bytes = hex::decode(host).expect("unix URL host must be hex-decodable");
        let path = String::from_utf8(path_bytes).expect("socket path should be UTF-8");
        assert_eq!(path, "/var/run/docker.sock");
        assert_eq!(rest, "v1.43/containers/json?all=true");
    }
}
