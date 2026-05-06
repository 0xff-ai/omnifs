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
            headers: header_map_to_wit(&self.headers),
            body: self.body,
        };
        CalloutFuture::new(
            self.cx,
            Callout::Fetch(wit_request),
            |result| match result {
                CalloutResult::HttpResponse(resp) => wit_response_to_http(resp),
                CalloutResult::CalloutError(e) => Err(ProviderError::from_callout_error(&e)),
                _ => Err(ProviderError::internal("unexpected callout result type")),
            },
        )
    }
}

fn header_map_to_wit(map: &HeaderMap) -> Vec<Header> {
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

fn wit_response_to_http(resp: HttpResponse) -> Result<Response<Vec<u8>>> {
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
            Err(ProviderError::from_http_status(status.as_u16()))
        } else {
            Ok(())
        }
    }
}

pub enum CalloutFuture<'cx, S, T> {
    Pending {
        cx: &'cx Cx<S>,
        callout: Option<Callout>,
        extract: fn(CalloutResult) -> Result<T>,
    },
    Ready(Option<Result<T>>),
}

// No self-referential state; pinned only for Future signature
// compatibility.
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
    fn post_builder_produces_request_with_post_method() {
        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let mut fut = Box::pin(cx.http().post("https://example.test/items").send());
        assert!(matches!(drive_once(&mut fut), Poll::Pending));

        let req = take_single_fetch(&cx);
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://example.test/items");
        assert!(req.body.is_none());
    }

    #[test]
    fn body_passes_raw_bytes_through_to_fetch_callout() {
        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let raw = b"raw-bytes".to_vec();
        let mut fut = Box::pin(
            cx.http()
                .post("https://example.test/upload")
                .body(raw.clone())
                .send(),
        );
        assert!(matches!(drive_once(&mut fut), Poll::Pending));

        let req = take_single_fetch(&cx);
        assert_eq!(req.method, "POST");
        assert_eq!(req.body, Some(raw));
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
    fn json_and_header_chaining_order_is_equivalent() {
        #[derive(serde::Serialize)]
        struct V {
            x: i32,
        }
        let v = V { x: 42 };

        let state_a = Rc::new(RefCell::new(()));
        let cx_a = Cx::new(1, state_a);
        let mut fut_a = Box::pin(
            cx_a.http()
                .post("https://example.test/a")
                .header("X-Foo", "bar")
                .json(&v)
                .send(),
        );
        assert!(matches!(drive_once(&mut fut_a), Poll::Pending));
        let fetch_a = take_single_fetch(&cx_a);

        let state_b = Rc::new(RefCell::new(()));
        let cx_b = Cx::new(1, state_b);
        let mut fut_b = Box::pin(
            cx_b.http()
                .post("https://example.test/a")
                .json(&v)
                .header("X-Foo", "bar")
                .send(),
        );
        assert!(matches!(drive_once(&mut fut_b), Poll::Pending));
        let fetch_b = take_single_fetch(&cx_b);

        assert_eq!(fetch_a.method, fetch_b.method);
        assert_eq!(fetch_a.url, fetch_b.url);
        assert_eq!(fetch_a.body, fetch_b.body);

        let sorted = |req: &HttpRequest| -> Vec<(String, String)> {
            let mut h: Vec<(String, String)> = req
                .headers
                .iter()
                .map(|h| (h.name.to_ascii_lowercase(), h.value.clone()))
                .collect();
            h.sort();
            h
        };
        assert_eq!(sorted(&fetch_a), sorted(&fetch_b));
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
    fn invalid_header_value_surfaces_at_send() {
        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let mut fut = Box::pin(
            cx.http()
                .get("https://example.test/api")
                .header("X-Bad", "value\nwith newline")
                .send(),
        );

        assert!(matches!(drive_once(&mut fut), Poll::Ready(Err(_))));
        assert!(cx.take_yielded_callouts().is_empty());
    }

    #[test]
    fn json_serialization_failure_surfaces_at_send() {
        struct AlwaysFails;
        impl serde::Serialize for AlwaysFails {
            fn serialize<S: serde::Serializer>(
                &self,
                _: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("nope"))
            }
        }

        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let mut fut = Box::pin(
            cx.http()
                .post("https://example.test/api")
                .json(&AlwaysFails)
                .send(),
        );

        assert!(matches!(drive_once(&mut fut), Poll::Ready(Err(_))));
        assert!(cx.take_yielded_callouts().is_empty());
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
        assert!(
            error.to_string().contains("invalid header name"),
            "expected first failure (header name), got {error}"
        );
    }
}
