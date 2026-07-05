#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
use omnifs_engine::EngineError;
use omnifs_itest::{RuntimeHarness, TestOpExt, into_inline, make_initialized_runtime};
use omnifs_wit::provider::types::{CalloutResult, ErrorKind, Header, HttpResponse};

fn p(path: &str) -> Path {
    Path::parse(path).unwrap()
}

fn web_harness() -> RuntimeHarness {
    make_initialized_runtime(
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
}

#[test]
fn web_provider_extracts_fixture_html_to_markdown() {
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
fn web_provider_raw_route_preserves_fixture_response_bytes() {
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

#[test]
fn web_provider_empty_rest_fetches_site_root() {
    let harness = web_harness();
    let body = b"<html><body><p>site root</p></body></html>\n".to_vec();
    let mut op = harness.read("/raw/https/example.test").unwrap();
    let fetch = op.expect_single_fetch();
    assert_eq!(fetch.url, "https://example.test/");

    op.answer_callouts(vec![html_response(&body)]).unwrap();

    let result = op.into_read_file().unwrap();
    assert_eq!(into_inline(result), body);
}

#[tokio::test]
async fn web_provider_denies_domains_outside_mount_config() {
    let harness = RuntimeHarness::new_real_callouts(
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
