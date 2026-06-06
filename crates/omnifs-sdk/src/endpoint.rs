//! typed outbound endpoints (ADR-0001 §10).
//!
//! A provider declares an outbound host with `#[derive(Endpoint)]` and reaches
//! it through `cx.endpoint::<E>()`. The returned [`EndpointHandle`] builds
//! requests whose `path` is relative to `E::base()`; the SDK forms the URL via
//! [`crate::http::HttpEndpoint`] and lowers the request onto the existing
//! [`crate::http`] callout machinery. The terminals split the object-path
//! 3-state ([`load`](RequestBuilder::load), [`conditional_json`](RequestBuilder::conditional_json))
//! from the structural 2-state ([`json`](RequestBuilder::json),
//! [`send_checked`](RequestBuilder::send_checked)).

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::VersionToken;
use crate::http::{BlobRequest, HttpEndpoint, Request, ResponseExt};
use crate::object::{Canonical, Load};
use core::fmt::Display;
use http::{Response, StatusCode};

/// Per-endpoint rate-limit policy. `Default` uses the breaker's default
/// cooldown when a 429 carries no `Retry-After`; `Cooldown` overrides that
/// default; `Off` opts the endpoint out of arming the breaker entirely. The
/// check still runs, but this endpoint never arms a window.
#[derive(Clone, Copy, Debug)]
pub enum RateLimitPolicy {
    Default,
    Cooldown(core::time::Duration),
    Off,
}

/// An outbound host a provider talks to over HTTP. The `#[derive(Endpoint)]`
/// macro generates the impl from the `#[endpoint(base = ..)]` and
/// `#[endpoint(default_header = ..)]` attributes.
pub trait Endpoint {
    fn base() -> &'static str;

    /// Headers applied to every request to this endpoint before any
    /// per-request header, so a builder's `.header(..)` can still override.
    /// Generated from `#[endpoint(default_header = "Name: Value")]`.
    fn default_headers() -> &'static [(&'static str, &'static str)] {
        &[]
    }

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
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    body_is_json: bool,
    /// A deferred body-serialization failure. `body_json` cannot return a
    /// `Result` without breaking the builder chain, so it records the error
    /// here and the terminal surfaces it instead of sending a malformed body.
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
            headers: Vec::new(),
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

    /// Append a request header.
    #[must_use]
    pub fn header(mut self, key: &str, value: impl Into<String>) -> Self {
        self.headers.push((key.to_string(), value.into()));
        self
    }

    /// Set the `Accept` header.
    #[must_use]
    pub fn accept(mut self, content_type: &str) -> Self {
        self.headers
            .push(("accept".to_string(), content_type.to_string()));
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
    /// short-circuits to [`Load::Unchanged`] / [`Revalidate::Unchanged`].
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
        for (name, value) in &self.headers {
            req = req.header(name, value);
        }
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
    /// `NotFound`, 304 -> `Unchanged`, other 4xx/5xx -> `Err`.
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

    /// Fetch the URL into the blob store; the bytes never cross back.
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
            Ok(Load::Fresh { value, canonical })
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
    #[must_use]
    pub fn cache_key(mut self, key: impl Into<String>) -> Self {
        self.cache_key = Some(key.into());
        self
    }

    /// Fetch the URL into the blob store and return its handle.
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

/// A handle to a host-resident blob fetched through an [`Endpoint`].
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
