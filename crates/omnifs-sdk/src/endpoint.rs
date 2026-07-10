//! Typed outbound endpoints (ADR-0001 §10).
//!
//! A provider declares each upstream host once with `#[derive(Endpoint)]` and
//! reaches it through [`Cx::endpoint`]. The returned [`EndpointHandle`] builds
//! requests whose `path` is relative to [`Endpoint::base`]; the SDK forms the
//! URL via [`crate::http::HttpEndpoint`] and lowers the request onto the
//! [`crate::http`] callout machinery, so awaiting a terminal suspends the
//! handler while the host runs the fetch.
//!
//! Pick the terminal by what the response status means for the route:
//!
//! - Object layer, 3-state: [`load`](RequestBuilder::load) and
//!   [`load_with`](RequestBuilder::load_with) map 200 to [`Load::Fresh`],
//!   304 to [`Load::Unchanged`], and 404 to [`Load::NotFound`]. Pair them
//!   with [`maybe_if_none_match`](RequestBuilder::maybe_if_none_match) so the
//!   host-pushed validator ([`Cx::version`]) becomes `If-None-Match` and an
//!   unchanged object costs no body transfer.
//! - Structural layer, 2-state: [`json`](RequestBuilder::json) and
//!   [`send_checked`](RequestBuilder::send_checked) return the value or error
//!   on any 4xx/5xx, including 404. Use these when "missing" is not a
//!   meaningful outcome for the route.
//!
//! Never set `Authorization` or any other credential header here. Auth is
//! host-managed: the provider's `auth = ..` argument to `#[omnifs_sdk::provider]`
//! declares the scheme and inject domains, and the host materializes the header
//! into matching requests after the callout leaves the guest. Endpoint
//! `default_header` attributes are for static metadata such as `User-Agent`
//! and `Accept`.
//!
//! Every send participates in the per-authority rate-limit breaker: a 429
//! arms a cooldown for the URL's authority (honoring `Retry-After`, else the
//! endpoint's [`RateLimitPolicy`]), and later sends to that authority
//! fast-fail in-guest until the window passes. An upstream that signals
//! errors off the status line (a 403 rate limit, an error in a 200 body)
//! overrides [`EndpointHooks::classify`] to map the raw response to a typed
//! [`ProviderError`] before any terminal sees it; a classified rate limit
//! arms the breaker just like a native 429.
//!
//! ```ignore
//! #[derive(omnifs_sdk::Endpoint)]
//! #[endpoint(base = "https://export.arxiv.org",
//!            default_header = "User-Agent: omnifs-provider-arxiv")]
//! struct ArxivApi;
//!
//! let load = cx
//!     .endpoint(ArxivApi)
//!     .get("/api/query")
//!     .query("id_list", raw_id)
//!     .maybe_if_none_match(since.as_ref())
//!     .load_with(parse_paper_atom)
//!     .await?;
//! ```

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::VersionToken;
use crate::http::{BlobRequest, HttpEndpoint, Request, ResponseExt};
use crate::object::{Canonical, Load, Object};
use core::fmt::Display;
use http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode};

/// Per-endpoint rate-limit policy, generated from
/// `#[endpoint(rate_limit = ..)]` (`"off"` or a whole number of seconds).
/// `Default` uses the breaker's default cooldown when a 429 carries no
/// `Retry-After`; `Cooldown` overrides that default; `Off` opts the endpoint
/// out of arming the breaker entirely. The pre-send check still runs under
/// `Off`, so a window armed by another endpoint on the same authority still
/// fast-fails this one.
#[derive(Clone, Copy, Debug)]
pub enum RateLimitPolicy {
    Default,
    Cooldown(core::time::Duration),
    Off,
}

/// An outbound host a provider talks to over HTTP, passed by value to
/// [`Cx::endpoint`]. A unit struct works for a fixed upstream; a struct with
/// fields carries a runtime-resolved base (a configured daemon socket, an
/// in-cluster API URL). `#[derive(Endpoint)]` generates this impl from the
/// `#[endpoint(base = .., default_header = .., rate_limit = ..)]` attributes;
/// implement it by hand when the base is dynamic.
///
/// Credential headers are never declared here; the host injects them from the
/// provider's auth manifest (see the module docs).
pub trait Endpoint {
    /// Base URL request paths resolve against, such as
    /// `https://api.example.com`. A `unix:///absolute/socket/path` base
    /// routes through the host's Unix-socket HTTP executor (see
    /// [`crate::http::HttpEndpoint`]).
    fn base(&self) -> &str;

    /// Headers applied to every request to this endpoint before any
    /// per-request header, so a builder's `.header(..)` can still override.
    /// Generated from `#[endpoint(default_header = "Name: Value")]`.
    fn default_headers(&self) -> &[(&str, &str)] {
        &[]
    }

    /// How a 429 from this endpoint arms the per-authority breaker.
    /// Generated from `#[endpoint(rate_limit = ..)]`; defaults to honoring
    /// `Retry-After` with the breaker's built-in fallback cooldown.
    fn rate_limit(&self) -> RateLimitPolicy {
        RateLimitPolicy::Default
    }
}

/// Overridable response and error behavior for an [`Endpoint`]. Every method
/// is defaulted; `#[derive(Endpoint)]` supplies the defaults for the easy
/// case, and a provider that needs to override marks the derive with
/// `#[endpoint(hooks)]` and writes its own `impl EndpointHooks`, overriding
/// only the relevant methods. Future cross-cutting behavior lands here as a
/// new defaulted method, not a new attribute.
pub trait EndpointHooks: Endpoint {
    /// Classify a raw response into a typed [`ProviderError`] before the
    /// default status mapping runs, for upstreams that signal errors in a
    /// shape `error_for_status` cannot see: a rate limit returned as `403`
    /// with `x-ratelimit-remaining: 0` instead of `429`, or an error encoded
    /// in the body of a `200`. Return `Some(err)` to fail every terminal
    /// ([`load`](RequestBuilder::load), [`json`](RequestBuilder::json), ...)
    /// with that error; `None` falls through to the default per-status
    /// handling. A returned [`RateLimited`](crate::error::ProviderError::rate_limited)
    /// error arms the per-authority breaker from its `retry_after`, exactly
    /// as a native `429` would, so a later request to the same host
    /// fast-fails in-guest. Defaults to no classification.
    fn classify(&self, _response: &Response<Vec<u8>>) -> Option<ProviderError> {
        None
    }
}

/// A typed handle to an [`Endpoint`] value, obtained from [`Cx::endpoint`].
pub struct EndpointHandle<'a, E, S = ()> {
    cx: &'a Cx<S>,
    endpoint: E,
}

impl<'a, E: EndpointHooks, S> EndpointHandle<'a, E, S> {
    pub(crate) fn new(cx: &'a Cx<S>, endpoint: E) -> Self {
        Self { cx, endpoint }
    }

    /// Begin a GET request to `path` (relative to the endpoint's base).
    pub fn get(self, path: impl Into<String>) -> RequestBuilder<'a, E, S> {
        RequestBuilder::new(self.cx, self.endpoint, http::Method::GET, path.into())
    }

    /// Begin a POST request to `path` (relative to the endpoint's base).
    pub fn post(self, path: impl Into<String>) -> RequestBuilder<'a, E, S> {
        RequestBuilder::new(self.cx, self.endpoint, http::Method::POST, path.into())
    }
}

/// A builder for one request against an [`Endpoint`]. The URL is formed at a
/// terminal by combining `E::base()`, the relative `path`, and the collected
/// query pairs through [`HttpEndpoint::build_url`].
pub struct RequestBuilder<'a, E, S = ()> {
    cx: &'a Cx<S>,
    method: http::Method,
    path: String,
    query: Vec<(String, String)>,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    body_is_json: bool,
    /// Deferred header- or body-serialization failure. Neither `.header(..)`
    /// nor `.body_json(..)` can return `Result` without breaking the builder
    /// chain, so they record the first error here and the terminal surfaces it
    /// instead of sending a malformed request.
    body_error: Option<ProviderError>,
    if_none_match: Option<String>,
    endpoint: E,
}

impl<'a, E: EndpointHooks, S> RequestBuilder<'a, E, S> {
    fn new(cx: &'a Cx<S>, endpoint: E, method: http::Method, path: String) -> Self {
        Self {
            cx,
            method,
            path,
            query: Vec::new(),
            headers: HeaderMap::new(),
            body: None,
            body_is_json: false,
            body_error: None,
            if_none_match: None,
            endpoint,
        }
    }

    /// Append a query pair, stringifying `value` through [`Display`].
    #[must_use]
    pub fn query(mut self, key: &str, value: impl Display) -> Self {
        self.query.push((key.to_string(), value.to_string()));
        self
    }

    /// Append several query pairs, stringifying each value through [`Display`].
    #[must_use]
    pub fn query_pairs<I, K, V>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Display,
    {
        self.query.extend(
            pairs
                .into_iter()
                .map(|(key, value)| (key.as_ref().to_string(), value.to_string())),
        );
        self
    }

    /// Append a request header. Invalid names or values are recorded as a
    /// sticky error so the chainable builder stays infallible; the error is
    /// surfaced by the terminal.
    #[must_use]
    pub fn header(mut self, key: &str, value: impl Into<String>) -> Self {
        if self.body_error.is_some() {
            return self;
        }
        let name = match HeaderName::try_from(key) {
            Ok(n) => n,
            Err(e) => {
                self.body_error = Some(ProviderError::invalid_input(format!(
                    "invalid header name: {e}"
                )));
                return self;
            },
        };
        let val_str = value.into();
        let value = match HeaderValue::try_from(val_str.as_str()) {
            Ok(v) => v,
            Err(e) => {
                self.body_error = Some(ProviderError::invalid_input(format!(
                    "invalid header value: {e}"
                )));
                return self;
            },
        };
        self.headers.append(name, value);
        self
    }

    /// Serialize `value` as the JSON request body and set `Content-Type:
    /// application/json`. A serialization failure is recorded and surfaced by
    /// the terminal (no request is sent), rather than smuggled through a header.
    #[must_use]
    pub fn body_json<T: serde::Serialize + ?Sized>(mut self, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(bytes) => {
                self.body = Some(bytes);
                self.body_is_json = true;
            },
            Err(e) => {
                self.body_error = Some(ProviderError::invalid_input(format!(
                    "json body serialize: {e}"
                )));
            },
        }
        self
    }

    /// Map the host-pushed validator to `If-None-Match` when present, so a 304
    /// short-circuits to [`Load::Unchanged`].
    /// Take the validator from [`Cx::version`] or the `since` argument of
    /// `Object::load`; passing `None` is a plain unconditional fetch.
    #[must_use]
    pub fn maybe_if_none_match(mut self, version: Option<&VersionToken>) -> Self {
        if let Some(v) = version {
            self.if_none_match = Some(v.as_str().to_string());
        }
        self
    }

    fn url(&self) -> String {
        let query: Vec<(&str, &str)> = self
            .query
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        HttpEndpoint::parse(self.endpoint.base()).build_url(&self.path, &query)
    }

    /// Lower to an HTTP request and the endpoint response policy that must be
    /// applied after the host returns the raw response.
    fn into_http_request(mut self) -> Result<(Request<'a, S>, ResponsePolicy<E>)> {
        if let Some(error) = self.body_error.take() {
            return Err(error);
        }
        let url = self.url();
        let rate_limit = self.endpoint.rate_limit();
        let policy = ResponsePolicy::new(
            self.endpoint,
            crate::rate_limit::authority_of(&url),
            rate_limit,
        );
        let builder = self.cx.http();
        let mut req = if self.method == http::Method::POST {
            builder.post(url)
        } else {
            builder.get(url)
        };
        // Endpoint defaults first, so a per-request `.header(..)` overrides.
        for (name, value) in policy.endpoint.default_headers() {
            req = req.header(name, value);
        }
        // Merge the pre-parsed HeaderMap; extend_headers skips re-parsing.
        req = req.extend_headers(self.headers);
        if let Some(v) = &self.if_none_match {
            req = req.header("if-none-match", v);
        }
        if let Some(body) = self.body {
            if self.body_is_json {
                req = req.header("content-type", "application/json");
            }
            req = req.body(body);
        }
        Ok((req, policy))
    }

    /// Send through the breaker: the check happens in `Request::send`; here we
    /// arm on a 429 (or an [`EndpointHooks::classify`] rate limit) and close
    /// on a `status < 400` response so the next request to a throttled
    /// authority fast-fails instead of calling out.
    async fn send_raw(self) -> Result<Response<Vec<u8>>> {
        let (request, policy) = self.into_http_request()?;
        let resp = request.send().await?;
        policy.apply(&resp)?;
        Ok(resp)
    }

    /// Object/revalidation terminal: 200 -> `Fresh(O::decode(body))`, 404 ->
    /// `NotFound`, 304 -> `Unchanged`, other 4xx/5xx -> `Err`. On `Fresh` the
    /// stored [`Canonical`] is the raw response body verbatim with the response
    /// `ETag` as its validator; [`Object::decode`] only produces the in-memory
    /// value. Use [`Self::load_with`] for a one-off parse fn when no `Object`
    /// type applies.
    pub async fn load<O: Object>(self) -> Result<Load<O>> {
        let resp = self.send_raw().await?;
        load_from_response(resp, O::decode)
    }

    /// Like [`Self::load`] but parses the canonical with `parse` (non-`Object`
    /// structural DTOs, or a custom decode).
    pub async fn load_with<T>(self, parse: impl Fn(&[u8]) -> Result<T>) -> Result<Load<T>> {
        let resp = self.send_raw().await?;
        load_from_response(resp, parse)
    }

    /// Structural terminal: deserialize the body into `T`. 4xx/5xx -> `Err`;
    /// there is no `Load` wrapper.
    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T> {
        let resp = self.send_raw().await?.error_for_status()?;
        serde_json::from_slice::<T>(resp.body())
            .map_err(|e| ProviderError::invalid_input(format!("json decode: {e}")))
    }

    /// A checked structural fetch returning the raw body.
    pub async fn send_checked(self) -> Result<HttpResponse> {
        let resp = self.send_raw().await?.error_for_status()?;
        Ok(HttpResponse { inner: resp })
    }

    /// Convert into a blob fetch: the response body lands in the host's
    /// blob cache and only a [`BlobHandle`] crosses back. A cache key is
    /// mandatory; chain [`BlobRequestBuilder::cache_key`] before
    /// [`BlobRequestBuilder::fetch`].
    pub fn into_blob(self) -> BlobRequestBuilder<'a, E, S> {
        BlobRequestBuilder {
            inner: self,
            cache_key: None,
        }
    }
}

struct ResponsePolicy<E> {
    endpoint: E,
    authority: Option<String>,
    rate_limit: RateLimitPolicy,
}

impl<E: EndpointHooks> ResponsePolicy<E> {
    fn new(endpoint: E, authority: Option<String>, rate_limit: RateLimitPolicy) -> Self {
        Self {
            endpoint,
            authority,
            rate_limit,
        }
    }

    fn apply(&self, resp: &Response<Vec<u8>>) -> Result<()> {
        // Provider-specific classification runs first: it can detect error
        // signals the default status mapping misses (e.g. a 403 rate limit),
        // and a classified rate limit arms the breaker just like a native 429.
        if let Some(error) = self.endpoint.classify(resp) {
            if error.kind() == crate::error::ProviderErrorKind::RateLimited {
                self.record_rate_limit(error.retry_after());
            }
            return Err(error);
        }

        match resp.status() {
            StatusCode::TOO_MANY_REQUESTS => self.record_rate_limit(self.retry_after(resp)),
            status if status.as_u16() < 400 => self.record_success(),
            _ => {},
        }
        Ok(())
    }

    fn retry_after(&self, resp: &Response<Vec<u8>>) -> Option<core::time::Duration> {
        let parsed = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::rate_limit::parse_retry_after_secs);
        let cooldown = match self.rate_limit {
            RateLimitPolicy::Cooldown(d) => Some(d),
            _ => None,
        };
        parsed.or(cooldown)
    }

    fn record_rate_limit(&self, retry_after: Option<core::time::Duration>) {
        if matches!(self.rate_limit, RateLimitPolicy::Off) {
            return;
        }
        if let Some(authority) = &self.authority {
            crate::rate_limit::with_breaker(|b| {
                b.record_429(authority, retry_after);
            });
        }
    }

    fn record_success(&self) {
        if matches!(self.rate_limit, RateLimitPolicy::Off) {
            return;
        }
        if let Some(authority) = &self.authority {
            crate::rate_limit::with_breaker(|b| {
                b.record_success(authority);
            });
        }
    }
}

fn load_from_response<T>(
    resp: Response<Vec<u8>>,
    parse: impl Fn(&[u8]) -> Result<T>,
) -> Result<Load<T>> {
    match resp.status() {
        StatusCode::NOT_MODIFIED => Ok(Load::Unchanged),
        StatusCode::NOT_FOUND => Ok(Load::NotFound),
        status if status.is_client_error() || status.is_server_error() => {
            Err(resp.error_for_status().unwrap_err())
        },
        _ => {
            // The canonical is the raw response body verbatim (JSON, Atom, XML,
            // or opaque). `parse` only produces the in-memory value for
            // rendering; it does not define the stored bytes.
            let version = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(VersionToken::from);
            let bytes = resp.into_body();
            let value = parse(&bytes)?;
            let canonical = Canonical {
                bytes,
                validator: version,
            };
            Ok(Load::fresh(value, canonical))
        },
    }
}

/// A provider-facing wrapper over a raw HTTP response.
pub struct HttpResponse {
    inner: Response<Vec<u8>>,
}

impl HttpResponse {
    pub fn status(&self) -> u16 {
        self.inner.status().as_u16()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner.headers().get(name).and_then(|v| v.to_str().ok())
    }

    pub fn body(&self) -> &[u8] {
        self.inner.body()
    }
}

/// A builder for a blob fetch against an [`Endpoint`], lowering onto
/// [`crate::http::BlobRequest`].
pub struct BlobRequestBuilder<'a, E, S = ()> {
    inner: RequestBuilder<'a, E, S>,
    cache_key: Option<String>,
}

impl<'a, E: EndpointHooks, S> BlobRequestBuilder<'a, E, S> {
    /// Set the provider-scoped deduplication key. Required: [`Self::fetch`]
    /// errors without one. Repeating the key from the same provider returns
    /// the cached blob instead of refetching, so embed everything that
    /// distinguishes the content (id, version, variant) in the key.
    #[must_use]
    pub fn cache_key(mut self, key: impl Into<String>) -> Self {
        self.cache_key = Some(key.into());
        self
    }

    /// Fetch the URL into the blob store and return its handle. Applies the
    /// same 4xx/5xx mapping as the structural terminals, so a `BlobHandle`
    /// always refers to a successful response body.
    pub async fn fetch(self) -> Result<BlobHandle> {
        let cache_key = self.cache_key;
        let (request, _policy) = self.inner.into_http_request()?;
        let mut blob: BlobRequest<'a, S> = request.into_blob();
        if let Some(key) = cache_key {
            blob = blob.with_cache_key(key);
        }
        let blob_ref = blob.send().await?.error_for_status()?;
        Ok(BlobHandle {
            id: blob_ref.id,
            size: blob_ref.size,
        })
    }
}

/// A handle to a host-resident blob fetched through an [`Endpoint`]. Hand
/// `id` to `FileProjection::blob` (with `size` as the exact size), to
/// `cx.archives().open(id)`, or to `cx.blob(id)`; the bytes themselves stay
/// host-side.
pub struct BlobHandle {
    pub id: crate::blob::BlobId,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderErrorKind;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    struct TestEp;

    impl Endpoint for TestEp {
        fn base(&self) -> &'static str {
            "https://breaker.test"
        }
    }
    impl EndpointHooks for TestEp {}

    struct TestEpOff;

    impl Endpoint for TestEpOff {
        fn base(&self) -> &'static str {
            "https://off-policy.test"
        }

        fn rate_limit(&self) -> RateLimitPolicy {
            RateLimitPolicy::Off
        }
    }
    impl EndpointHooks for TestEpOff {}

    struct TestEpCooldown;

    impl Endpoint for TestEpCooldown {
        fn base(&self) -> &'static str {
            "https://cooldown-policy.test"
        }

        fn rate_limit(&self) -> RateLimitPolicy {
            RateLimitPolicy::Cooldown(Duration::from_secs(45))
        }
    }
    impl EndpointHooks for TestEpCooldown {}

    fn response(status: u16, headers: &[(&str, &str)]) -> Response<Vec<u8>> {
        let mut builder = Response::builder().status(status);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Vec::new()).expect("test response")
    }

    #[test]
    fn open_endpoint_breaker_fast_fails_without_callout() {
        crate::rate_limit::with_breaker(|b| {
            b.clear();
            b.record_429("https://breaker.test", Some(Duration::from_secs(30)));
        });

        let cx = Cx::new(1, Rc::new(RefCell::new(())));
        let (request, _policy) = cx.endpoint(TestEp).get("/x").into_http_request().unwrap();
        let error = request.into_fetch_request().unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);

        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);
    }

    #[test]
    fn endpoint_off_policy_does_not_arm() {
        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);

        let cx = Cx::new(1, Rc::new(RefCell::new(())));
        let (_request, policy) = cx
            .endpoint(TestEpOff)
            .get("/x")
            .into_http_request()
            .unwrap();
        let resp = response(429, &[]);
        policy.apply(&resp).unwrap();
        let error = resp.error_for_status_ref().unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        assert_eq!(
            crate::rate_limit::with_breaker(|b| b.check("https://off-policy.test")),
            None
        );

        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);
    }

    #[test]
    fn endpoint_cooldown_policy_overrides_default() {
        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);

        let cx = Cx::new(1, Rc::new(RefCell::new(())));
        let (_request, policy) = cx
            .endpoint(TestEpCooldown)
            .get("/x")
            .into_http_request()
            .unwrap();
        let resp = response(429, &[]);
        policy.apply(&resp).unwrap();
        let error = resp.error_for_status_ref().unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        let remaining =
            crate::rate_limit::with_breaker(|b| b.check("https://cooldown-policy.test"))
                .expect("breaker should be open");
        assert!(remaining <= Duration::from_secs(45));
        assert!(remaining > Duration::from_secs(30));

        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);
    }
}
