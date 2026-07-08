#![cfg(not(target_os = "wasi"))]

mod scenarios;
mod support;

/// Adversarial DNS coverage that the recorded scenarios cannot express: config
/// rejection, error-kind mapping for unknown resolvers and record types,
/// request-side resolver routing and `DoH` wire-format assertions (the projection
/// snapshot renders the response, never the outbound request), the
/// no-canonical-store structural invariant, and the `all` fan-out's rate-limit
/// and network error handling driven by canned wire responses. The projection
/// happy path (routing, listing, record reads) lives in `scenarios.rs` over
/// recorded tapes.
mod adversarial {
    use omnifs_core::path::Path;
    use omnifs_engine::EngineError;
    use omnifs_itest::RuntimeHarness;
    use omnifs_wit::provider::types::{
        CalloutResult, ErrorKind, HttpResponse, OpResult, ReadFileOutcome,
    };

    use crate::support::{
        canned_a_response, dns_harness, expect_fetch as dns_expect_fetch, expect_fetches,
    };

    fn parse_path(s: &str) -> Path {
        Path::parse(s).unwrap()
    }

    fn empty_http_response(status: u16) -> CalloutResult {
        CalloutResult::HttpResponse(HttpResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }

    #[test]
    fn dns_provider_rejects_invalid_default_resolver_config_during_initialize() {
        let error = match RuntimeHarness::new(
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
    async fn dns_provider_unknown_resolver_read_is_invalid_input() {
        let harness = RuntimeHarness::new(
            r#"
            {
                "provider": "omnifs_provider_dns.wasm",
                "mount": "dns",
                "capabilities": {
                    "domains": ["cloudflare-dns.com", "dns.google"]
                }
            }
        "#,
        )
        .unwrap();

        let error = harness
            .runtime
            .namespace()
            .read_file(
                &parse_path("/@missing/example.com/A"),
                Path::parse("/@missing/example.com/A")
                    .unwrap()
                    .content_type_mime(None)
                    .to_string(),
                None,
            )
            .await
            .unwrap_err();
        match error {
            EngineError::ProviderError(error) => {
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
        let harness = RuntimeHarness::new(
            r#"
            {
                "provider": "omnifs_provider_dns.wasm",
                "mount": "dns",
                "capabilities": {
                    "domains": ["cloudflare-dns.com", "dns.google"]
                }
            }
        "#,
        )
        .unwrap();

        let error = harness
            .runtime
            .namespace()
            .read_file(
                &parse_path("/example.com/BOGUS"),
                Path::parse("/example.com/BOGUS")
                    .unwrap()
                    .content_type_mime(None)
                    .to_string(),
                None,
            )
            .await
            .unwrap_err();
        match error {
            EngineError::ProviderError(error) => {
                assert_eq!(error.kind, ErrorKind::NotFound);
            },
            other => panic!("expected unknown-record NotFound, got {other:?}"),
        }

        let error = harness
            .runtime
            .namespace()
            .read_file(
                &parse_path("/@cloudflare/example.com/BOGUS"),
                Path::parse("/@cloudflare/example.com/BOGUS")
                    .unwrap()
                    .content_type_mime(None)
                    .to_string(),
                None,
            )
            .await
            .unwrap_err();
        match error {
            EngineError::ProviderError(error) => {
                assert_eq!(error.kind, ErrorKind::NotFound);
            },
            other => panic!("expected resolver unknown-record NotFound, got {other:?}"),
        }
    }

    // A record read stays a mutable projection: it never emits a canonical-store
    // effect and its fs-writes never carry object ids. The scenario snapshot
    // renders `canonical: (none)` but not the fs object-id, so this invariant is
    // asserted here against a canned response rather than a recorded tape.
    #[test]
    fn dns_record_read_emits_no_canonical_store() {
        use omnifs_wit::provider::types::Callout;

        let harness = dns_harness();
        let mut op = harness.read("/example.com/A").unwrap();
        let [Callout::Fetch(_)] = op.callouts() else {
            panic!("expected single fetch callout, got {:?}", op.callouts());
        };

        op.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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

    // Resolver routing lives on the outbound request URL, which the projection
    // snapshot never renders; assert the default and explicit resolver both
    // target the configured cloudflare endpoint with a DoH `dns=` parameter.
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

    // Resolver aliases (`@google`, `@8.8.8.8`) route the request to the aliased
    // endpoint. Like the default-resolver test, this asserts the request URL,
    // which the projection snapshot does not render.
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

    // The outbound request encodes a DoH query in wire format under `dns=`; the
    // projection snapshot renders the decoded response, never this request shape,
    // so decode the request wire and assert the query name and type here.
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

    // The `all` fan-out keeps records from the queries that succeeded even when
    // the rest are rate-limited: one 200 plus six 429s still yields the answered
    // record. Canned responses drive the exact partial-failure mix a live
    // recording could not reproduce.
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

        op.answer_callouts(outcomes).unwrap();
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

    // Every `all` query rate-limited maps to a terminal RateLimited error, not a
    // partial success.
    #[test]
    fn dns_all_returns_rate_limited_when_every_query_is_rate_limited() {
        let harness = dns_harness();
        let mut op = harness.read("/example.com/all").unwrap();
        let fetches = expect_fetches(&op);

        op.answer_callouts(
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

    // Every `all` query failing without rate-limiting maps to a terminal Network
    // error, distinguishing transport failure from throttling.
    #[test]
    fn dns_all_returns_network_when_every_query_fails_without_rate_limit() {
        let harness = dns_harness();
        let mut op = harness.read("/example.com/all").unwrap();
        let fetches = expect_fetches(&op);

        op.answer_callouts(
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
}
