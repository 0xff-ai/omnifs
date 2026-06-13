//! Typed outbound endpoints (ADR-0001 §10).
//!
//! A provider declares each upstream host once with `#[derive(Endpoint)]`,
//! lists it in the provider's `resources(endpoints = [..])`, and reaches it
//! through [`Cx::endpoint`]. The returned [`EndpointHandle`] builds requests
//! whose `path` is relative to [`Endpoint::base`]; the SDK forms the URL via
//! [`crate::http::HttpEndpoint`] and lowers the request onto the [`crate::http`]
//! callout machinery, so awaiting a terminal suspends the handler while the
//! host runs the fetch.
//!
//! Pick the terminal by what the response status means for the route:
//!
//! - Object layer, 3-state: [`load`](RequestBuilder::load) and
//!   [`load_with`](RequestBuilder::load_with) map 200 to [`Load::Fresh`],
//!   304 to [`Load::Unchanged`], and 404 to [`Load::NotFound`]. Pair them
//!   with [`maybe_if_none_match`](RequestBuilder::maybe_if_none_match) so the
//!   host-pushed validator ([`Cx::version`]) becomes `If-None-Match` and an
//!   unchanged object costs no body transfer.
//!   [`conditional_json`](RequestBuilder::conditional_json) is the
//!   listing-revalidation analogue (no `NotFound` arm).
//! - Structural layer, 2-state: [`json`](RequestBuilder::json) and
//!   [`send_checked`](RequestBuilder::send_checked) return the value or error
//!   on any 4xx/5xx, including 404. Use these when "missing" is not a
//!   meaningful outcome for the route.
//!
//! Never set `Authorization` or any other credential header here. Auth is
//! host-managed: the provider's `omnifs.provider.json` manifest declares the
//! scheme and inject domains, and the host materializes the header into
//! matching requests after the callout leaves the guest. Endpoint
//! `default_header` attributes are for static metadata such as `User-Agent`
//! and `Accept`.
//!
//! Every send participates in the per-authority rate-limit breaker: a 429
//! arms a cooldown for the URL's authority (honoring `Retry-After`, else the
//! endpoint's [`RateLimitPolicy`]), and later sends to that authority
//! fast-fail in-guest until the window passes.
//!
//! ```ignore
//! #[derive(omnifs_sdk::Endpoint)]
//! #[endpoint(base = "https://export.arxiv.org")]
//! #[endpoint(default_header = "User-Agent: omnifs-provider-arxiv")]
//! struct ArxivApi;
//!
//! let load = cx
//!     .endpoint::<ArxivApi>()
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
use crate::object::{Canonical, Load};
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

/// An outbound host a provider talks to over HTTP. The `#[derive(Endpoint)]`
/// macro generates the impl from the `#[endpoint(base = ..)]`,
/// `#[endpoint(default_header = ..)]`, and `#[endpoint(rate_limit = ..)]`
/// attributes. Credential headers are never declared here; the host injects
/// them from the provider's auth manifest (see the module docs).
pub trait Endpoint {
    /// Base URL request paths resolve against, such as
    /// `https://api.example.com`. A `unix:///absolute/socket/path` base
    /// routes through the host's Unix-socket HTTP executor (see
    /// [`crate::http::HttpEndpoint`]).
    fn base() -> &'static str;

    /// Headers applied to every request to this endpoint before any
    /// per-request header, so a builder's `.header(..)` can still override.
    /// Generated from `#[endpoint(default_header = "Name: Value")]`.
    fn default_headers() -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// How a 429 from this endpoint arms the per-authority breaker.
    /// Generated from `#[endpoint(rate_limit = ..)]`; defaults to honoring
    /// `Retry-After` with the breaker's built-in fallback cooldown.
    fn rate_limit_policy() -> RateLimitPolicy {
        RateLimitPolicy::Default
    }
}

/// A typed handle to a declared [`Endpoint`], obtained from
/// [`Cx::endpoint`]. Usable only if the endpoint is listed in the provider's
/// `resources(endpoints = [..])`.
pub struct EndpointHandle<'a, E, S = ()> {
    cx: &'a Cx<S>,
    _endpoint: core::marker::PhantomData<E>,
}

impl<'a, E: Endpoint, S> EndpointHandle<'a, E, S> {
    pub fn new(cx: &'a Cx<S>) -> Self {
        Self {
            cx,
            _endpoint: core::marker::PhantomData,
        }
    }

    /// Begin a GET request to `path` (relative to `E::base()`).
    pub fn get(&self, path: impl Into<String>) -> RequestBuilder<'a, E, S> {
        RequestBuilder::new(self.cx, http::Method::GET, path.into())
    }

    /// Begin a POST request to `path` (relative to `E::base()`).
    pub fn post(&self, path: impl Into<String>) -> RequestBuilder<'a, E, S> {
        RequestBuilder::new(self.cx, http::Method::POST, path.into())
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
    _endpoint: core::marker::PhantomData<E>,
}

impl<'a, E: Endpoint, S> RequestBuilder<'a, E, S> {
    fn new(cx: &'a Cx<S>, method: http::Method, path: String) -> Self {
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
            _endpoint: core::marker::PhantomData,
        }
    }

    /// Append a query pair, stringifying `value` through [`Display`].
    #[must_use]
    pub fn query(mut self, key: &str, value: impl Display) -> Self {
        self.query.push((key.to_string(), value.to_string()));
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

    /// Set the `Accept` header. Invalid values follow the same sticky-error
    /// path as `.header(..)`.
    #[must_use]
    pub fn accept(self, content_type: &str) -> Self {
        self.header("accept", content_type)
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
    /// short-circuits to [`Load::Unchanged`] / [`Revalidate::Unchanged`].
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
        HttpEndpoint::parse(E::base()).build_url(&self.path, &query)
    }

    fn into_http_request(mut self) -> Result<Request<'a, S>> {
        if let Some(error) = self.body_error.take() {
            return Err(error);
        }
        let url = self.url();
        let builder = self.cx.http();
        let mut req = if self.method == http::Method::POST {
            builder.post(url)
        } else {
            builder.get(url)
        };
        // Endpoint defaults first, so a per-request `.header(..)` overrides.
        for (name, value) in E::default_headers() {
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
        Ok(req)
    }

    /// Send through the breaker: the check happens in `Request::send`; here we
    /// arm on a 429 and close on a `status < 400` response so the next request
    /// to a throttled authority fast-fails instead of calling out.
    async fn send_raw(self) -> Result<Response<Vec<u8>>> {
        let authority = crate::rate_limit::authority_of(&self.url());
        let resp = self.into_http_request()?.send().await?;
        if let Some(authority) = authority {
            let status = resp.status();
            let policy = E::rate_limit_policy();
            if !matches!(policy, RateLimitPolicy::Off) {
                crate::rate_limit::with_breaker(|b| {
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        let parsed = resp
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(crate::rate_limit::parse_retry_after_secs);
                        let cooldown = match policy {
                            RateLimitPolicy::Cooldown(d) => Some(d),
                            _ => None,
                        };
                        b.record_429(&authority, parsed.or(cooldown));
                    } else if status.as_u16() < 400 {
                        b.record_success(&authority);
                    }
                });
            }
        }
        Ok(resp)
    }

    /// Object/revalidation terminal: 200 -> `Fresh(serde(T))`, 404 ->
    /// `NotFound`, 304 -> `Unchanged`, other 4xx/5xx -> `Err`. On `Fresh` the
    /// stored [`Canonical`] is the raw response body verbatim with the
    /// response `ETag` as its validator; deserialization only produces the
    /// in-memory value.
    pub async fn load<T: serde::de::DeserializeOwned>(self) -> Result<Load<T>> {
        let resp = self.send_raw().await?;
        load_from_response(resp, |bytes| {
            serde_json::from_slice::<T>(bytes)
                .map_err(|e| ProviderError::invalid_input(format!("json decode: {e}")))
        })
    }

    /// Like [`Self::load`] but parses the canonical with `parse` (non-JSON
    /// canonicals such as arXiv Atom).
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

    /// Listing-revalidation terminal (OPEN-8): 304 -> `Unchanged`, 200 ->
    /// `Fresh { value, validator }` where the validator is the response `ETag`.
    pub async fn conditional_json<T: serde::de::DeserializeOwned>(self) -> Result<Revalidate<T>> {
        let resp = self.send_raw().await?;
        if resp.status() == StatusCode::NOT_MODIFIED {
            return Ok(Revalidate::Unchanged);
        }
        let resp = resp.error_for_status()?;
        let validator = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(VersionToken::from);
        let value = serde_json::from_slice::<T>(resp.body())
            .map_err(|e| ProviderError::invalid_input(format!("json decode: {e}")))?;
        Ok(Revalidate::Fresh { value, validator })
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

/// The listing-revalidation outcome of [`RequestBuilder::conditional_json`].
pub enum Revalidate<T> {
    Unchanged,
    Fresh {
        value: T,
        validator: Option<VersionToken>,
    },
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
            let value = parse(resp.body())?;
            let version = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(VersionToken::from);
            let canonical = Canonical {
                bytes: resp.body().clone(),
                validator: version,
            };
            Ok(Load::fresh_from(value, canonical))
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

impl<'a, E: Endpoint, S> BlobRequestBuilder<'a, E, S> {
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
        let cache_key = self.cache_key.clone();
        let mut blob: BlobRequest<'a, S> = self.inner.into_http_request()?.into_blob();
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
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Poll, Waker};
    use omnifs_wit::provider::types::{CalloutResult, Header, HttpResponse as WitHttpResponse};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    struct TestEp;

    impl Endpoint for TestEp {
        fn base() -> &'static str {
            "https://breaker.test"
        }
    }

    struct TestEpOff;

    impl Endpoint for TestEpOff {
        fn base() -> &'static str {
            "https://off-policy.test"
        }

        fn rate_limit_policy() -> RateLimitPolicy {
            RateLimitPolicy::Off
        }
    }

    struct TestEpCooldown;

    impl Endpoint for TestEpCooldown {
        fn base() -> &'static str {
            "https://cooldown-policy.test"
        }

        fn rate_limit_policy() -> RateLimitPolicy {
            RateLimitPolicy::Cooldown(Duration::from_secs(45))
        }
    }

    fn drive_once<F: Future>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut ctx = core::task::Context::from_waker(waker);
        future.as_mut().poll(&mut ctx)
    }

    fn deliver_response<S>(cx: &Cx<S>, status: u16, headers: &[(&str, &str)]) {
        cx.push_delivered(CalloutResult::HttpResponse(WitHttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| Header {
                    name: (*name).to_string(),
                    value: (*value).to_string(),
                })
                .collect(),
            body: Vec::new(),
        }));
    }

    #[test]
    fn open_endpoint_breaker_fast_fails_without_callout() {
        crate::rate_limit::with_breaker(|b| {
            b.clear();
            b.record_429("https://breaker.test", Some(Duration::from_secs(30)));
        });

        let cx = Cx::new(1, Rc::new(RefCell::new(())));
        let mut fut = Box::pin(
            cx.endpoint::<TestEp>()
                .get("/x")
                .json::<serde_json::Value>(),
        );

        let Poll::Ready(Err(error)) = drive_once(&mut fut) else {
            panic!("expected open breaker to fail immediately");
        };

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        assert!(
            cx.take_yielded_callouts().is_empty(),
            "open breaker must not issue a fetch callout"
        );

        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);
    }

    #[test]
    fn endpoint_off_policy_does_not_arm() {
        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);

        let cx = Cx::new(1, Rc::new(RefCell::new(())));
        let mut fut = Box::pin(cx.endpoint::<TestEpOff>().get("/x").send_checked());

        assert!(matches!(drive_once(&mut fut), Poll::Pending));
        assert_eq!(cx.take_yielded_callouts().len(), 1);
        deliver_response(&cx, 429, &[]);

        let Poll::Ready(Err(error)) = drive_once(&mut fut) else {
            panic!("expected 429 response to surface as an error");
        };

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
        let mut fut = Box::pin(cx.endpoint::<TestEpCooldown>().get("/x").send_checked());

        assert!(matches!(drive_once(&mut fut), Poll::Pending));
        assert_eq!(cx.take_yielded_callouts().len(), 1);
        deliver_response(&cx, 429, &[]);

        let Poll::Ready(Err(error)) = drive_once(&mut fut) else {
            panic!("expected 429 response to surface as an error");
        };

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        let remaining =
            crate::rate_limit::with_breaker(|b| b.check("https://cooldown-policy.test"))
                .expect("breaker should be open");
        assert!(remaining <= Duration::from_secs(45));
        assert!(remaining > Duration::from_secs(30));

        crate::rate_limit::with_breaker(crate::rate_limit::RateLimitBreaker::clear);
    }
}
