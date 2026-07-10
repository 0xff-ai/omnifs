//! Raw HTTP callout builders and the callout future protocol.
//!
//! `cx.http().get(url).send().await` is the untyped layer beneath
//! [`crate::endpoint`]; prefer typed endpoints for upstream APIs and reach
//! for this layer only when the URL is fully dynamic. `send()` resolves to
//! `http::Response<Vec<u8>>` with no status mapping: use
//! [`ResponseExt::error_for_status`] for the default 4xx/5xx mapping or
//! inspect the response directly.
//!
//! Sending never performs I/O in the guest. [`CalloutFuture`] yields the
//! request through a WIT async host import; the component runtime suspends the
//! operation while the host runs the effect and resumes this future with the
//! typed result. Poll several callout futures with [`crate::cx::join_all`] when
//! the provider can issue independent upstream requests concurrently.
//!
//! The per-authority rate-limit breaker is checked here, at the lowest
//! layer, so raw `cx.http()` sends and blob fetches inherit it as well as
//! typed endpoints: an open 429 window fast-fails in-guest without issuing
//! a callout. Arming is not symmetric, though: only typed endpoint sends
//! arm the breaker from a 429 response. A raw send surfaces the 429 as an
//! error without opening a window; call [`crate::note_rate_limited`] if a
//! raw path should arm it.

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
#[cfg(target_arch = "wasm32")]
use omnifs_wit::provider::omnifs::provider::callouts;
use omnifs_wit::provider::types::{Callout, CalloutResult, Header, HttpRequest, HttpResponse};

/// Entry point returned by `cx.http()`. Starts a request against a fully
/// formed URL; there is no base-URL resolution at this layer.
pub struct Builder<'cx, S> {
    cx: &'cx Cx<S>,
}

impl<'cx, S> Builder<'cx, S> {
    pub(crate) fn new(cx: &'cx Cx<S>) -> Self {
        Self { cx }
    }

    pub fn get(self, url: impl Into<String>) -> Request<'cx, S> {
        Request::new(self.cx, Method::GET, url)
    }

    pub fn post(self, url: impl Into<String>) -> Request<'cx, S> {
        Request::new(self.cx, Method::POST, url)
    }
}

/// One raw HTTP request under construction. Builder methods are infallible
/// by design: validation failures are recorded as a sticky first error and
/// surfaced by `send`, so chains never need intermediate `?`.
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

    /// Lower the request into a `fetch` callout. Awaiting the returned
    /// future suspends the operation on a WIT async host import; the host
    /// executes the fetch and resumes the future with the full response (any
    /// status, body buffered in guest memory). For large bodies use
    /// [`Self::into_blob`] so the bytes stay host-side. Fails immediately,
    /// without a callout, on a sticky builder error or an open rate-limit
    /// window for the URL's authority.
    pub fn send(self) -> CalloutFuture<'cx, Response<Vec<u8>>> {
        let cx = self.cx;
        match self.into_fetch_request() {
            Ok(wit_request) => CalloutFuture::new(cx, Callout::Fetch(wit_request), |r| {
                expect_callout(
                    "fetch",
                    |r| match r {
                        CalloutResult::HttpResponse(resp) => Some(response_from_wit(resp)),
                        _ => None,
                    },
                    r,
                )
            }),
            Err(error) => CalloutFuture::ready_error(cx, error),
        }
    }

    pub(crate) fn into_fetch_request(self) -> Result<HttpRequest> {
        if let Some(error) = self.error {
            return Err(error);
        }
        // Proactive breaker: if this authority is in an open 429 window,
        // fast-fail without issuing the callout. This lives at the lowest HTTP
        // layer so raw `cx.http()` users inherit it, not only typed endpoints.
        if let Some(error) = self.open_breaker_error() {
            return Err(error);
        }
        Ok(HttpRequest {
            method: self.method.as_str().to_string(),
            url: self.url,
            headers: WitHeaders(&self.headers).into(),
            body: self.body,
        })
    }

    fn open_breaker_error(&self) -> Option<ProviderError> {
        let authority = crate::rate_limit::authority_of(&self.url)?;
        let remaining = crate::rate_limit::with_breaker(|b| b.check(&authority))?;
        Some(
            ProviderError::rate_limited(format!("endpoint breaker open for {authority}"))
                .with_retry_after(Some(remaining)),
        )
    }

    /// Merge pre-validated headers into this request.
    ///
    /// The caller already holds a `HeaderMap` built through validated
    /// `HeaderName`/`HeaderValue` insertions (e.g. from `endpoint::RequestBuilder`),
    /// so no re-parsing is needed here.
    pub(crate) fn extend_headers(mut self, headers: HeaderMap) -> Self {
        if self.error.is_some() {
            return self;
        }
        self.headers.extend(headers);
        self
    }

    /// Convert this request into a blob fetch. The response body lands in
    /// the host's blob cache rather than crossing the WIT, and the returned
    /// [`crate::blob::BlobRef`] can be handed to
    /// [`crate::projection::FileProjection::blob`], `cx.archives().open(..)`,
    /// or `cx.blob(id).read()`. A cache key is mandatory; chain
    /// [`BlobRequest::with_cache_key`] before `send`.
    pub fn into_blob(self) -> BlobRequest<'cx, S> {
        BlobRequest {
            inner: self,
            cache_key: None,
        }
    }
}

/// A `fetch-blob` callout under construction: an HTTP fetch whose response
/// body stays in the host blob cache. Created via [`Request::into_blob`].
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

    /// Provider-scoped deduplication key, required before `send`. Two
    /// requests from the same provider using the same key share one blob and
    /// one upstream fetch; different providers never collide on a key. Embed
    /// everything that distinguishes the content (id, version, variant) in
    /// the key, or stale bytes will be served for the colliding request.
    pub fn with_cache_key(mut self, key: impl Into<String>) -> Self {
        self.cache_key = Some(key.into());
        self
    }

    /// Lower into a `fetch-blob` callout. Resolves to a
    /// [`crate::blob::BlobRef`] carrying metadata only; the body is on the
    /// host's disk. Fails immediately when no cache key was set, on a sticky
    /// builder error, or when the authority's rate-limit window is open.
    /// Note the `BlobRef` carries the upstream status: chain
    /// [`crate::blob::BlobRef::error_for_status`] for the default mapping.
    pub fn send(self) -> CalloutFuture<'cx, crate::blob::BlobRef> {
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
        // Blob fetches are HTTP callouts too, so the endpoint breaker must
        // short-circuit before the host starts fetching bytes into the cache.
        if let Some(error) = self.inner.open_breaker_error() {
            return CalloutFuture::ready_error(self.inner.cx, error);
        }
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

fn response_from_wit(resp: HttpResponse) -> Result<Response<Vec<u8>>> {
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
    // `Retry-After` is either delta-seconds or an HTTP-date. Honor the common
    // delta-seconds form as structured backoff; an HTTP-date stays in the
    // human-readable message only (no guest wall-clock math on the error path).
    let retry_after_secs = retry_after.and_then(crate::rate_limit::parse_retry_after_secs);
    ProviderError::rate_limited(message).with_retry_after(retry_after_secs)
}

/// Unified callout result extractor.
///
/// Maps `CalloutError` to `Err` and routes the accepted variant through
/// `pick`. Returns `Err` with a message naming `kind` when the result is
/// neither `CalloutError` nor the expected variant (`pick` returns `None`).
/// If `pick` returns `Some(Err(...))`, that error propagates as-is so
/// fallible conversions (e.g. status parsing) preserve their message.
///
/// `pick` is a `fn` pointer so non-capturing closures at call sites coerce
/// to it, keeping every extractor a plain `fn(CalloutResult) -> Result<T>`
/// that `CalloutFuture::new` accepts without boxing.
pub(crate) fn expect_callout<T>(
    kind: &'static str,
    pick: fn(CalloutResult) -> Option<Result<T>>,
    result: CalloutResult,
) -> Result<T> {
    match result {
        CalloutResult::CalloutError(e) => Err(e.into()),
        other => match pick(other) {
            Some(outcome) => outcome,
            None => Err(ProviderError::internal(format!(
                "unexpected callout result for {kind}"
            ))),
        },
    }
}

/// The future shape of every callout (fetch, fetch-blob, read-blob,
/// git-open-repo, open-archive).
///
/// `Pending` wraps the generated async WIT import future. `Ready` short-circuits
/// builder and breaker errors without entering the host.
pub enum CalloutFuture<'cx, T> {
    Pending(Pin<Box<dyn Future<Output = Result<T>> + 'cx>>),
    Ready(Option<Result<T>>),
}

// `Ready` carries `T`, so the auto-derive would gate `Unpin` on
// `T: Unpin`; this future never relies on structural pinning of any
// variant field, so the manual impl is sound regardless of `T`.
impl<T> Unpin for CalloutFuture<'_, T> {}

impl<'cx, T: 'cx> CalloutFuture<'cx, T> {
    pub(crate) fn new<S>(
        cx: &'cx Cx<S>,
        callout: Callout,
        extract: fn(CalloutResult) -> Result<T>,
    ) -> Self {
        Self::Pending(Box::pin(run_callout(cx.id(), callout, extract)))
    }

    pub(crate) fn ready_error<S>(_cx: &'cx Cx<S>, error: ProviderError) -> Self {
        Self::Ready(Some(Err(error)))
    }
}

impl<T> Future for CalloutFuture<'_, T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut *self {
            Self::Ready(slot) => match slot.take() {
                Some(result) => Poll::Ready(result),
                None => Poll::Pending,
            },
            Self::Pending(future) => future.as_mut().poll(ctx),
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn run_callout<T>(
    id: u64,
    callout: Callout,
    extract: fn(CalloutResult) -> Result<T>,
) -> Result<T> {
    let result = match callout {
        Callout::Fetch(req) => callouts::fetch(id, req).await,
        Callout::GitOpenRepo(req) => callouts::git_open_repo(id, req).await,
        Callout::FetchBlob(req) => callouts::fetch_blob(id, req).await,
        Callout::OpenArchive(req) => callouts::open_archive(id, req).await,
        Callout::ReadBlob(req) => callouts::read_blob(id, req).await,
    };
    extract(result)
}

#[cfg(not(target_arch = "wasm32"))]
fn run_callout<T>(
    _id: u64,
    _callout: Callout,
    _extract: fn(CalloutResult) -> Result<T>,
) -> impl Future<Output = Result<T>> {
    core::future::ready(Err(ProviderError::internal(
        "provider callouts require a wasm32 component target",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderErrorKind;
    use core::task::Waker;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    fn drive_once<F: Future>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut ctx = Context::from_waker(waker);
        future.as_mut().poll(&mut ctx)
    }

    #[test]
    fn json_content_type_handling() {
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
        let req = cx
            .http()
            .post("https://example.test/api")
            .json(&payload)
            .into_fetch_request()
            .unwrap();
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

        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(2, state);
        let req = cx
            .http()
            .post("https://example.test/api")
            .header("content-type", "application/vnd.custom+json")
            .json(&serde_json::json!({"k": "v"}))
            .into_fetch_request()
            .unwrap();
        let ct_headers: Vec<&Header> = req
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("content-type"))
            .collect();
        assert_eq!(ct_headers.len(), 1, "Content-Type must not be duplicated");
        assert_eq!(ct_headers[0].value, "application/vnd.custom+json");
    }

    #[test]
    fn raw_http_send_fast_fails_when_breaker_open() {
        crate::rate_limit::with_breaker(|b| {
            b.clear();
            b.record_429("https://raw.test", Some(Duration::from_secs(30)));
        });

        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);
        let mut fut = Box::pin(cx.http().get("https://raw.test/x").send());

        let Poll::Ready(Err(error)) = drive_once(&mut fut) else {
            panic!("expected open breaker to fail immediately");
        };

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);
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
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
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
        let mut url = match self {
            Self::Tcp(base) => base.trim_end_matches('/').to_string(),
            Self::Unix(socket) => {
                // Encode the absolute socket path as hex bytes so the
                // URL has a stable host segment and the host's
                // executor decodes it the same way `hyperlocal` does.
                let socket_str = socket.to_string_lossy();
                let host = hex::encode(socket_str.as_bytes());
                format!("unix://{host}")
            },
        };
        if !path.is_empty() && !path.starts_with('/') {
            url.push('/');
        }
        url.push_str(path);
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs);
        }
        url
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
