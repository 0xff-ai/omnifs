//! HTTP callout result extraction and typed async HTTP builders.

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::omnifs::provider::types::{Callout, CalloutResult, Header, HttpRequest, HttpResponse};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

pub struct Builder<'cx, S> {
    cx: &'cx Cx<S>,
}

impl<'cx, S> Builder<'cx, S> {
    pub fn new(cx: &'cx Cx<S>) -> Self {
        Self { cx }
    }

    pub fn get(self, url: impl Into<String>) -> Request<'cx, S> {
        Request {
            cx: self.cx,
            method: "GET".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(self, url: impl Into<String>) -> Request<'cx, S> {
        Request {
            cx: self.cx,
            method: "POST".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }
}

pub struct Request<'cx, S> {
    cx: &'cx Cx<S>,
    method: String,
    url: String,
    headers: Vec<Header>,
    body: Option<Vec<u8>>,
}

impl<'cx, S> Request<'cx, S> {
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header {
            name: name.into(),
            value: value.into(),
        });
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
    /// a `Content-Type` header (case-insensitive match).
    ///
    /// Returns an error if JSON serialization fails.
    pub fn json<T: serde::Serialize + ?Sized>(mut self, value: &T) -> Result<Self> {
        let bytes = serde_json::to_vec(value).map_err(|e| {
            ProviderError::invalid_input(format!("failed to serialize json body: {e}"))
        })?;
        self.body = Some(bytes);
        if !self
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("content-type"))
        {
            self.headers.push(Header {
                name: "Content-Type".to_string(),
                value: "application/json".to_string(),
            });
        }
        Ok(self)
    }

    pub fn send(self) -> CalloutFuture<'cx, S, HttpResponse> {
        CalloutFuture::new(
            self.cx,
            Callout::Fetch(HttpRequest {
                method: self.method,
                url: self.url,
                headers: self.headers,
                body: self.body,
            }),
            |result| match result {
                CalloutResult::HttpResponse(resp) if resp.status < 400 => Ok(resp),
                CalloutResult::HttpResponse(resp) => {
                    Err(ProviderError::from_http_status(resp.status))
                },
                CalloutResult::CalloutError(e) => Err(ProviderError::from_callout_error(&e)),
                _ => Err(ProviderError::internal("unexpected callout result type")),
            },
        )
    }

    pub fn send_body(self) -> CalloutFuture<'cx, S, Vec<u8>> {
        CalloutFuture::new(
            self.cx,
            Callout::Fetch(HttpRequest {
                method: self.method,
                url: self.url,
                headers: self.headers,
                body: self.body,
            }),
            |result| match result {
                CalloutResult::HttpResponse(resp) if resp.status < 400 => Ok(resp.body),
                CalloutResult::HttpResponse(resp) => {
                    Err(ProviderError::from_http_status(resp.status))
                },
                CalloutResult::CalloutError(e) => Err(ProviderError::from_callout_error(&e)),
                _ => Err(ProviderError::internal("unexpected callout result type")),
            },
        )
    }
}

/// Future that yields a single callout and resolves with a typed result.
pub struct CalloutFuture<'cx, S, T> {
    cx: &'cx Cx<S>,
    callout: Option<Callout>,
    extract: fn(CalloutResult) -> Result<T>,
}

impl<'cx, S, T> CalloutFuture<'cx, S, T> {
    pub(crate) fn new(
        cx: &'cx Cx<S>,
        callout: Callout,
        extract: fn(CalloutResult) -> Result<T>,
    ) -> Self {
        Self {
            cx,
            callout: Some(callout),
            extract,
        }
    }
}

impl<S, T> Future for CalloutFuture<'_, S, T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(callout) = self.callout.take() {
            self.cx.push_yielded(callout);
            return Poll::Pending;
        }

        if let Some(result) = self.cx.pop_delivered() {
            let extract = self.extract;
            return Poll::Ready(extract(result));
        }

        Poll::Pending
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

        let mut fut = Box::pin(cx.http().post("https://example.test/items").send_body());
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
                .send_body(),
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
        let request = cx
            .http()
            .post("https://example.test/api")
            .json(&payload)
            .expect("json serialization should succeed");
        let mut fut = Box::pin(request.send_body());
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

        let request = cx
            .http()
            .post("https://example.test/api")
            .header("content-type", "application/vnd.custom+json")
            .json(&serde_json::json!({"k": "v"}))
            .expect("json serialization should succeed");
        let mut fut = Box::pin(request.send_body());
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
        let req_a = cx_a
            .http()
            .post("https://example.test/a")
            .header("X-Foo", "bar")
            .json(&v)
            .expect("json ok");
        let mut fut_a = Box::pin(req_a.send_body());
        assert!(matches!(drive_once(&mut fut_a), Poll::Pending));
        let fetch_a = take_single_fetch(&cx_a);

        let state_b = Rc::new(RefCell::new(()));
        let cx_b = Cx::new(1, state_b);
        let req_b = cx_b
            .http()
            .post("https://example.test/a")
            .json(&v)
            .expect("json ok")
            .header("X-Foo", "bar");
        let mut fut_b = Box::pin(req_b.send_body());
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
}
