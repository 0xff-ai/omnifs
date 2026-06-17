#![cfg(not(target_os = "wasi"))]

mod support;

use omnifs_core::path::Path as OmnifsPath;
use omnifs_host::{Error, LookupOutcome};
use omnifs_itest::{make_initialized_runtime, try_make_runtime_from_config};
use omnifs_wit::provider::types::{
    CalloutResult, ErrorKind, HttpResponse, ListChildrenResult, OpResult, ReadFileOutcome,
    Stability,
};
use support::{canned_a_response, dns_harness, expect_fetch as dns_expect_fetch, expect_fetches};

fn assert_materialized_lookup(
    lookup: LookupOutcome,
    expected_path: &str,
    expected_directory: bool,
) {
    match lookup {
        LookupOutcome::Entry(entry) => {
            assert_eq!(entry.path().as_str(), expected_path);
            assert_eq!(entry.meta().is_directory(), expected_directory);
        },
        other => panic!("expected materialized lookup entry, got {other:?}"),
    }
}

fn assert_lookup_not_found(lookup: &LookupOutcome) {
    assert!(
        matches!(lookup, LookupOutcome::NotFound),
        "expected lookup miss, got {lookup:?}"
    );
}

#[test]
fn dns_provider_rejects_invalid_default_resolver_config_during_initialize() {
    let error = match try_make_runtime_from_config(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            },
            "config": {
                "default_resolver": "missing",
                "resolvers": {
                    "cloudflare": {
                        "url": "https://cloudflare-dns.com/dns-query",
                        "aliases": ["1.1.1.1"]
                    }
                }
            }
        }
    "#,
    ) {
        Ok(_) => panic!("expected runtime construction to fail for invalid dns config"),
        Err(error) => error,
    }
    .to_string();

    assert!(
        error.contains("default resolver"),
        "unexpected construction error: {error}"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dns_provider_routes_static_and_dynamic_paths() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            }
        }
    "#,
    );

    let lookup = harness
        .runtime
        .namespace()
        .lookup_child("/", "resolvers", None)
        .await
        .unwrap();
    assert_materialized_lookup(lookup, "/resolvers", false);

    let resolvers_file = harness
        .runtime
        .namespace()
        .read_file(
            "/resolvers",
            OmnifsPath::parse("/resolvers")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap();
    let body =
        String::from_utf8(omnifs_itest::into_inline(resolvers_file)).expect("utf8 resolvers file");
    assert!(
        body.contains("cloudflare"),
        "unexpected resolvers file: {body}"
    );

    let reverse_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/", "reverse", None)
        .await
        .unwrap();
    assert_materialized_lookup(reverse_lookup, "/reverse", true);

    let resolver_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/", "@cloudflare", None)
        .await
        .unwrap();
    assert_materialized_lookup(resolver_lookup, "/@cloudflare", true);

    let resolver_domain_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/@cloudflare", "example.com", None)
        .await
        .unwrap();
    assert_materialized_lookup(resolver_domain_lookup, "/@cloudflare/example.com", true);

    let resolver_reverse_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/@cloudflare", "reverse", None)
        .await
        .unwrap();
    assert_materialized_lookup(resolver_reverse_lookup, "/@cloudflare/reverse", true);

    let reverse_ip_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/reverse", "8.8.8.8", None)
        .await
        .unwrap();
    assert_materialized_lookup(reverse_ip_lookup, "/reverse/8.8.8.8", false);

    let resolver_reverse_ip_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/@cloudflare/reverse", "8.8.8.8", None)
        .await
        .unwrap();
    assert_materialized_lookup(
        resolver_reverse_ip_lookup,
        "/@cloudflare/reverse/8.8.8.8",
        false,
    );

    let invalid_reverse_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/reverse", "not-an-ip", None)
        .await
        .unwrap();
    assert_lookup_not_found(&invalid_reverse_lookup);

    let invalid_resolver_reverse_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/@cloudflare/reverse", "not-an-ip", None)
        .await
        .unwrap();
    assert_lookup_not_found(&invalid_resolver_reverse_lookup);

    let direct_ip_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/", "8.8.8.8", None)
        .await
        .unwrap();
    assert_lookup_not_found(&direct_ip_lookup);

    let resolver_direct_ip_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/@cloudflare", "8.8.8.8", None)
        .await
        .unwrap();
    assert_lookup_not_found(&resolver_direct_ip_lookup);

    let domain_lookup = harness
        .runtime
        .namespace()
        .lookup_child("/", "example.com", None)
        .await
        .unwrap();
    assert_materialized_lookup(domain_lookup, "/example.com", true);
    // lookup_child does NOT warm adjacent cache entries; the
    // cache_get assertions from v1 are removed.

    let listing = harness
        .runtime
        .namespace()
        .list_children("/example.com", None, None, None)
        .await
        .unwrap();
    match listing {
        ListChildrenResult::Entries(listing) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"A"));
            assert!(names.contains(&"all"));
            assert!(names.contains(&"raw"));
        },
        other => {
            panic!("expected domain listing, got {other:?}")
        },
    }

    let reverse_listing = harness
        .runtime
        .namespace()
        .list_children("/reverse", None, None, None)
        .await
        .unwrap();
    match reverse_listing {
        ListChildrenResult::Entries(listing) => {
            assert!(
                listing.entries.is_empty(),
                "reverse dir should not eagerly list dynamic children: {listing:?}"
            );
        },
        other => {
            panic!("expected reverse dir listing, got {other:?}")
        },
    }

    let resolver_reverse_listing = harness
        .runtime
        .namespace()
        .list_children("/@cloudflare/reverse", None, None, None)
        .await
        .unwrap();
    match resolver_reverse_listing {
        ListChildrenResult::Entries(listing) => {
            assert!(
                listing.entries.is_empty(),
                "resolver reverse dir should not eagerly list dynamic children: {listing:?}"
            );
        },
        other => {
            panic!("expected resolver reverse dir listing, got {other:?}")
        },
    }
}

#[tokio::test]
async fn dns_provider_unknown_resolver_read_is_invalid_input() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            }
        }
    "#,
    );

    let error = harness
        .runtime
        .namespace()
        .read_file(
            "/@missing/example.com/A",
            OmnifsPath::parse("/@missing/example.com/A")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap_err();
    match error {
        Error::ProviderError(error) => {
            assert_eq!(error.kind, ErrorKind::InvalidInput);
            assert!(
                error.message.contains("unknown resolver specifier"),
                "unexpected resolver error: {error:?}"
            );
        },
        other => panic!("expected invalid-input resolver error, got {other:?}"),
    }
}

#[tokio::test]
async fn dns_provider_unknown_record_reads_are_not_found() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            }
        }
    "#,
    );

    let error = harness
        .runtime
        .namespace()
        .read_file(
            "/example.com/BOGUS",
            OmnifsPath::parse("/example.com/BOGUS")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap_err();
    match error {
        Error::ProviderError(error) => {
            assert_eq!(error.kind, ErrorKind::NotFound);
        },
        other => panic!("expected unknown-record NotFound, got {other:?}"),
    }

    let error = harness
        .runtime
        .namespace()
        .read_file(
            "/@cloudflare/example.com/BOGUS",
            OmnifsPath::parse("/@cloudflare/example.com/BOGUS")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap_err();
    match error {
        Error::ProviderError(error) => {
            assert_eq!(error.kind, ErrorKind::NotFound);
        },
        other => panic!("expected resolver unknown-record NotFound, got {other:?}"),
    }
}

#[test]
fn dns_record_read_emits_no_canonical_store() {
    use omnifs_wit::provider::types::Callout;

    let harness = dns_harness();
    let mut op = harness.read("/example.com/A").unwrap();
    let [Callout::Fetch(_)] = op.callouts() else {
        panic!("expected single fetch callout, got {:?}", op.callouts());
    };

    op.resume(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: canned_a_response("example.com", "93.184.216.34"),
    })])
    .unwrap();

    match op.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            let body = String::from_utf8(omnifs_itest::expect_inline(file).to_vec()).unwrap();
            assert!(
                body.contains("A\t93.184.216.34"),
                "unexpected A record body: {body}"
            );
            let effects = op.effects().unwrap();
            assert!(
                effects.canonical.is_empty(),
                "DNS record read must not emit canonical-store: {:?}",
                effects.canonical
            );
            assert!(
                effects.fs.iter().all(|write| write.id.is_none()),
                "DNS fs-writes must not carry object ids: {:?}",
                effects.fs
            );
        },
        other => panic!("expected ReadFile terminal, got {other:?}"),
    }
}

#[test]
fn dns_default_resolver_targets_configured_default() {
    let harness = dns_harness();

    let default = harness.read("/example.com/A").unwrap();
    let default_fetch = dns_expect_fetch(&default);
    assert!(
        default_fetch
            .url
            .starts_with("https://cloudflare-dns.com/dns-query"),
        "unexpected default resolver URL: {}",
        default_fetch.url
    );
    assert!(
        default_fetch.url.contains("dns="),
        "expected DoH dns= parameter in {}",
        default_fetch.url
    );

    let explicit = harness.read("/@cloudflare/example.com/A").unwrap();
    let explicit_fetch = dns_expect_fetch(&explicit);
    assert!(
        explicit_fetch
            .url
            .starts_with("https://cloudflare-dns.com/dns-query"),
        "unexpected explicit resolver URL: {}",
        explicit_fetch.url
    );
    assert!(
        explicit_fetch.url.contains("dns="),
        "expected DoH dns= parameter in {}",
        explicit_fetch.url
    );
}

#[test]
fn dns_resolver_aliases_target_configured_endpoint() {
    let harness = dns_harness();

    for path in ["/@google/example.com/A", "/@8.8.8.8/example.com/A"] {
        let op = harness.read(path).unwrap();
        let fetch = dns_expect_fetch(&op);
        assert!(
            fetch.url.starts_with("https://dns.google/dns-query"),
            "unexpected resolver URL for {path}: {}",
            fetch.url
        );
        assert!(
            fetch.url.contains("dns="),
            "expected DoH dns= parameter in {}",
            fetch.url
        );
    }
}

#[test]
fn dns_record_read_is_mutable_not_immutable() {
    let harness = dns_harness();
    let mut op = harness.read("/example.com/A").unwrap();
    assert!(op.is_suspended());

    op.resume(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: canned_a_response("example.com", "93.184.216.34"),
    })])
    .unwrap();

    match op.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            let effects = op.effects().unwrap();
            assert!(
                effects.fs.is_empty(),
                "read_file primary path must not duplicate into effects.fs: {:?}",
                effects.fs
            );
        },
        other => panic!("expected ReadFile terminal, got {other:?}"),
    }
}

#[test]
fn dns_query_uses_doh_wireformat() {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hickory_proto::op::Message;
    use hickory_proto::rr::RecordType;

    let harness = dns_harness();

    let a_op = harness.read("/example.com/A").unwrap();
    let a_fetch = dns_expect_fetch(&a_op);
    assert!(
        a_fetch.url.contains("dns="),
        "expected dns= query parameter in {}",
        a_fetch.url
    );
    let (_, dns_param) = a_fetch.url.split_once("dns=").expect("dns param");
    let wire = URL_SAFE_NO_PAD.decode(dns_param).unwrap();
    let message = Message::from_vec(&wire).unwrap();
    assert_eq!(message.queries.len(), 1);
    assert_eq!(message.queries[0].name.to_string(), "example.com.");
    assert_eq!(message.queries[0].query_type, RecordType::A);

    let ptr_op = harness.read("/reverse/26.3.0.103").unwrap();
    let ptr_fetch = dns_expect_fetch(&ptr_op);
    let (_, dns_param) = ptr_fetch.url.split_once("dns=").expect("dns param");
    let message = Message::from_vec(&URL_SAFE_NO_PAD.decode(dns_param).unwrap()).unwrap();
    assert_eq!(message.queries[0].query_type, RecordType::PTR);
    assert_eq!(
        message.queries[0].name.to_string(),
        "103.0.3.26.in-addr.arpa."
    );
}

fn empty_http_response(status: u16) -> CalloutResult {
    CalloutResult::HttpResponse(HttpResponse {
        status,
        headers: Vec::new(),
        body: Vec::new(),
    })
}

#[test]
fn dns_all_keeps_partial_success_under_rate_limit() {
    let harness = dns_harness();
    let mut op = harness.read("/example.com/all").unwrap();
    let fetches = expect_fetches(&op);
    assert_eq!(
        fetches.len(),
        7,
        "expected one fetch per common record type, got {}",
        fetches.len()
    );

    let mut outcomes = vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: canned_a_response("example.com", "93.184.216.34"),
    })];
    outcomes.extend((1..fetches.len()).map(|_| {
        CalloutResult::HttpResponse(HttpResponse {
            status: 429,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }));

    op.resume(outcomes).unwrap();
    match op.into_result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            let body = String::from_utf8(omnifs_itest::into_inline(file)).unwrap();
            assert!(
                body.contains("A\t93.184.216.34"),
                "expected partial-success A line in body: {body}"
            );
        },
        other => panic!("expected ReadFile terminal after partial success, got {other:?}"),
    }
}

#[test]
fn dns_all_returns_rate_limited_when_every_query_is_rate_limited() {
    let harness = dns_harness();
    let mut op = harness.read("/example.com/all").unwrap();
    let fetches = expect_fetches(&op);

    op.resume(
        (0..fetches.len())
            .map(|_| empty_http_response(429))
            .collect(),
    )
    .unwrap();
    match op.into_result().unwrap() {
        OpResult::Error(error) => {
            assert_eq!(error.kind, ErrorKind::RateLimited);
        },
        other => panic!("expected RateLimited error after all queries failed, got {other:?}"),
    }
}

#[test]
fn dns_all_returns_network_when_every_query_fails_without_rate_limit() {
    let harness = dns_harness();
    let mut op = harness.read("/example.com/all").unwrap();
    let fetches = expect_fetches(&op);

    op.resume(
        (0..fetches.len())
            .map(|_| empty_http_response(500))
            .collect(),
    )
    .unwrap();
    match op.into_result().unwrap() {
        OpResult::Error(error) => {
            assert_eq!(error.kind, ErrorKind::Network);
        },
        other => panic!("expected Network error after all queries failed, got {other:?}"),
    }
}
