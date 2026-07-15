#![cfg(not(target_os = "wasi"))]

use omnifs_itest::{
    ReadFileOpExt, RuntimeHarness, TestOpExt, into_inline, make_initialized_runtime,
};
use omnifs_wit::provider::types::{
    CalloutResult, EntryKind, ErrorKind, Header, HttpResponse, ListChildrenResult,
    LookupChildResult, ReadFileOutcome,
};

fn web_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_web.wasm",
            "mount": "web",
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
        .read("/https/example.test/articles%2Freadable")
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
        .read("/raw/https/example.test/articles%2Fraw")
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
fn web_provider_host_directories_are_enumerable_and_open() {
    let harness = web_harness();

    let listing = harness.list("/https").unwrap();
    match listing.into_result().unwrap().unwrap() {
        ListChildrenResult::Entries(entries) => {
            assert!(entries.exhaustive);
            let host = entries
                .entries
                .iter()
                .find(|entry| entry.name == "example.test")
                .expect("configured host entry");
            assert!(matches!(host.kind, EntryKind::Directory));
        },
        other => panic!("expected host directory listing, got {other:?}"),
    }

    let lookup = harness.lookup("/https", "example.test").unwrap();
    match lookup.into_result().unwrap().unwrap() {
        LookupChildResult::Entry(entry) => {
            assert_eq!(entry.target.name, "example.test");
            assert!(matches!(entry.target.kind, EntryKind::Directory));
        },
        other => panic!("expected host directory lookup, got {other:?}"),
    }

    let host_listing = harness.list("/https/example.test").unwrap();
    match host_listing.into_result().unwrap().unwrap() {
        ListChildrenResult::Entries(entries) => assert!(!entries.exhaustive),
        other => panic!("expected open host listing, got {other:?}"),
    }
}

#[test]
fn web_provider_query_path_builds_exact_url() {
    let harness = web_harness();
    let mut op = harness
        .read("/raw/https/example.test/item?id=48884815")
        .unwrap();
    let fetch = op.expect_single_fetch();
    assert_eq!(fetch.url, "https://example.test/item?id=48884815");

    let body = b"query fixture".to_vec();
    op.answer_callouts(vec![html_response(&body)]).unwrap();
    assert_eq!(into_inline(op.into_read_file().unwrap()), body);
}

#[test]
fn web_provider_encoded_path_builds_multi_segment_url() {
    let harness = web_harness();
    let mut op = harness
        .read("/raw/https/example.test/docs%2Fguide")
        .unwrap();
    let fetch = op.expect_single_fetch();
    assert_eq!(fetch.url, "https://example.test/docs/guide");

    let body = b"encoded path fixture".to_vec();
    op.answer_callouts(vec![html_response(&body)]).unwrap();
    assert_eq!(into_inline(op.into_read_file().unwrap()), body);
}

#[test]
fn web_provider_root_leaf_fetches_site_root() {
    let harness = web_harness();
    let body = b"<html><body><p>site root</p></body></html>\n".to_vec();
    let mut op = harness.read("/raw/https/example.test/@root").unwrap();
    let fetch = op.expect_single_fetch();
    assert_eq!(fetch.url, "https://example.test/");

    op.answer_callouts(vec![html_response(&body)]).unwrap();

    let result = op.into_read_file().unwrap();
    assert_eq!(into_inline(result), body);
}

#[test]
fn web_provider_rejects_fragments_and_traversal() {
    let harness = web_harness();

    for path in [
        "/raw/https/example.test/item#fragment",
        "/raw/https/example.test/%2e%2e",
        "/raw/https/example.test/foo%2F%2e%2E",
    ] {
        let op = harness.read(path).unwrap();
        match op.result().unwrap() {
            Ok(ReadFileOutcome::NotFound(_)) => {},
            Err(error) if error.kind == ErrorKind::NotFound => {},
            other => panic!("expected rejected web path for {path}, got {other:?}"),
        }
    }
}

#[test]
fn web_provider_only_exposes_configured_hosts() {
    let harness = RuntimeHarness::new(
        r#"
        {
            "provider": "omnifs_provider_web.wasm",
            "mount": "web",
            "config": {
                "domains": ["EXAMPLE.test", "example.test", "second.test"]
            }
        }
    "#,
    )
    .unwrap();

    let listing = harness.list("/https").unwrap();
    match listing.into_result().unwrap().unwrap() {
        ListChildrenResult::Entries(entries) => {
            let names = entries
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .filter(|name| *name != "README.md")
                .collect::<Vec<_>>();
            assert_eq!(names, ["example.test", "second.test"]);
            assert!(entries.exhaustive);
        },
        other => panic!("expected configured host listing, got {other:?}"),
    }

    let configured = harness
        .read("/https/second.test/articles%2Freadable")
        .unwrap();
    assert_eq!(
        configured.expect_single_fetch().url,
        "https://second.test/articles/readable"
    );

    for prefix in ["/https", "/raw/https"] {
        let lookup = harness.lookup(prefix, "denied.test").unwrap();
        match lookup.result().unwrap() {
            Ok(LookupChildResult::NotFound(_)) => {},
            Err(error) => assert_eq!(error.kind, ErrorKind::NotFound),
            other => panic!("expected not-found lookup, got {other:?}"),
        }

        let read = harness
            .read(&format!("{prefix}/denied.test/@root"))
            .unwrap();
        assert!(
            read.callouts().is_empty(),
            "denied host must not fetch upstream"
        );
        match read.result().unwrap() {
            Ok(ReadFileOutcome::NotFound(_)) => {},
            Err(error) => assert_eq!(error.kind, ErrorKind::NotFound),
            other => panic!("expected not-found read, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn web_provider_denies_domains_outside_mount_config() {
    let harness = RuntimeHarness::new_real_callouts(
        r#"
        {
            "provider": "omnifs_provider_web.wasm",
            "mount": "web",
            "config": {
                "domains": ["allowed.test"]
            }
        }
    "#,
    )
    .unwrap();

    let error = harness
        .read("/https/denied.test/articles/readable")
        .unwrap()
        .into_result()
        .unwrap()
        .unwrap_err();

    assert_eq!(error.kind, ErrorKind::Denied);
    assert!(
        error.message.contains("domain not in allowlist"),
        "unexpected denied error: {error:?}"
    );
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
