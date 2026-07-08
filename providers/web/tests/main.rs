#![cfg(not(target_os = "wasi"))]

//! Hand-written web tests kept alongside the data-driven `scenarios`.
//!
//! The happy-path projection behavior (markdown extraction and raw pass-through
//! against real upstreams, including the empty-rest -> site-root fetch) is
//! covered by the recorded scenarios in `scenarios.rs`. What remains here are
//! the cases a scenario cannot express: unit-shaped projection-contract tests
//! that drive the routes with controlled fixture HTML so the transform and the
//! content types are asserted directly, and the adversarial domain-denial test
//! that runs real callouts against a denied host.

mod scenarios;

use omnifs_core::path::Path;
use omnifs_engine::EngineError;
use omnifs_itest::{CalloutSetup, RuntimeHarness, TestOpExt, into_inline};
use omnifs_wit::provider::types::{CalloutResult, ErrorKind, Header, HttpResponse};

fn p(path: &str) -> Path {
    Path::parse(path).unwrap()
}

fn web_harness() -> RuntimeHarness {
    RuntimeHarness::new(
        r#"
        {
            "provider": "omnifs_provider_web.wasm",
            "mount": "web",
            "capabilities": {
                "domains": { "dynamic": true }
            },
            "config": {
                "domains": ["example.test"]
            }
        }
    "#,
    )
    .unwrap()
}

fn html_response(body: &[u8]) -> CalloutResult {
    CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: vec![Header {
            name: "content-type".to_string(),
            value: "text/html; charset=utf-8".to_string(),
        }],
        body: body.to_vec(),
    })
}

/// Unit-shaped projection-contract tests. These drive the two routes with
/// controlled fixture HTML rather than a real upstream, so they can assert the
/// exact projection contract a scenario snapshot does not capture: the
/// readability transform's structural decisions (headline and body kept,
/// navigation dropped) and each route's declared content type. Not adversarial;
/// they are the transform's characterization tests.
mod projection {
    use super::*;

    #[test]
    fn markdown_route_extracts_html_and_drops_navigation() {
        let harness = web_harness();
        let mut op = harness
            .read("/https/example.test/articles/readable")
            .unwrap();
        let fetch = op.expect_single_fetch();
        assert_eq!(fetch.url, "https://example.test/articles/readable");

        op.answer_callouts(vec![html_response(
            br#"
            <!doctype html>
            <html>
              <head><title>Fixture article</title></head>
              <body>
                <nav>Navigation should not be readable content.</nav>
                <main>
                  <article>
                    <h1>Fixture headline</h1>
                    <p>Readable paragraph with <a href="/ref">a useful link</a>.</p>
                  </article>
                </main>
              </body>
            </html>
            "#,
        )])
        .unwrap();

        let result = op.into_read_file().unwrap();
        assert_eq!(result.content_type.as_deref(), Some("text/markdown"));
        let markdown = String::from_utf8(into_inline(result)).expect("markdown utf8");
        assert!(
            markdown.contains("Fixture headline"),
            "unexpected markdown: {markdown}"
        );
        assert!(
            markdown.contains("Readable paragraph"),
            "unexpected markdown: {markdown}"
        );
        assert!(
            !markdown.contains("Navigation should not be readable content"),
            "navigation leaked into markdown: {markdown}"
        );
    }

    #[test]
    fn raw_route_preserves_response_bytes_as_octet_stream() {
        let harness = web_harness();
        let body = b"<html><body><p>raw fixture</p></body></html>\n".to_vec();
        let mut op = harness
            .read("/raw/https/example.test/articles/raw")
            .unwrap();
        let fetch = op.expect_single_fetch();
        assert_eq!(fetch.url, "https://example.test/articles/raw");

        op.answer_callouts(vec![html_response(&body)]).unwrap();

        let result = op.into_read_file().unwrap();
        assert_eq!(
            result.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(into_inline(result), body);
    }
}

/// Adversarial tests: hostile or out-of-contract inputs that must be rejected.
mod adversarial {
    use super::*;

    #[tokio::test]
    async fn denies_domains_outside_mount_config() {
        let harness = RuntimeHarness::builder(
            r#"
            {
                "provider": "omnifs_provider_web.wasm",
                "mount": "web",
                "capabilities": {
                    "domains": { "dynamic": true }
                },
                "config": {
                    "domains": ["allowed.test"]
                }
            }
        "#,
        )
        .callouts(CalloutSetup::Real)
        .build()
        .unwrap();

        let error = harness
            .runtime
            .namespace()
            .read_file(
                &p("/https/denied.test/articles/readable"),
                p("/https/denied.test/articles/readable")
                    .content_type_mime(None)
                    .to_string(),
                None,
            )
            .await
            .unwrap_err();

        match error {
            EngineError::ProviderError(error) => {
                assert_eq!(error.kind, ErrorKind::Denied);
                assert!(
                    error.message.contains("domain not in allowlist"),
                    "unexpected denied error: {error:?}"
                );
            },
            other => panic!("expected denied provider error, got {other:?}"),
        }
    }
}
