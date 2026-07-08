//! Data-driven DNS scenarios over the callout tape system.
//!
//! Each scenario records real DNS-over-HTTPS callouts once (via
//! `just host itest-record dns <scenario>`) and replays them hermetically in the
//! default host-test lane. The provider is unauthenticated, so no credential is
//! seeded; recordings target the bundled public resolver (`cloudflare-dns.com`)
//! for durable fixture domains (`example.com`) whose records exist stably.
//!
//! `DoH` responses are binary DNS wire messages, so record-read tapes carry
//! `TapeBody::Base64` bodies (non-UTF-8 under the sidecar threshold). The
//! rendered projection is TTL-free (`format_record_lines` emits `{type}\t{value}`
//! only), so a snapshot is deterministic per tape even though the wire bytes
//! embed counting-down TTLs that churn on re-record.
//!
//! The projection routing surface (root listing, typed record directories, the
//! reverse tree, and per-resolver `@{resolver}` paths) resolves entirely from
//! provider state and static route tables, so the `routing` scenario issues no
//! HTTP callout and records an empty tape; only the `records` scenario reaches
//! the wire.

use omnifs_itest::scenario::{Scenario, Step, run};
use omnifs_itest::tape::scrub::TapeRules;

/// The dns mount config the scenarios record against: the two bundled public
/// `DoH` resolver domains as the callout allowlist. No auth block: the provider is
/// unauthenticated.
const DNS_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_dns.wasm",
    "mount": "dns",
    "capabilities": {
        "domains": ["cloudflare-dns.com", "dns.google"]
    }
}
"#;

/// Walk the projection routing surface without touching the wire: the root
/// listing and the inline `resolvers` file, the exhaustive record-type directory
/// for a domain, the reverse tree (default and per-resolver), and the
/// `@{resolver}` prefix routes. The lookups cover both the positive materialized
/// entries and the negatives the router rejects (a bare IP at a domain position,
/// an unparseable IP in a reverse directory). Every step resolves from provider
/// state or a static route table, so the recorded tape is empty and replay is
/// fully hermetic.
#[test]
fn routing() {
    run(&Scenario {
        name: "routing",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: DNS_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::List("/"),
            Step::Read("/resolvers"),
            Step::Lookup {
                parent: "/",
                name: "resolvers",
            },
            Step::Lookup {
                parent: "/",
                name: "reverse",
            },
            Step::Lookup {
                parent: "/",
                name: "@cloudflare",
            },
            Step::Lookup {
                parent: "/",
                name: "example.com",
            },
            // A bare IP is not a domain position at the root, so the router
            // rejects it rather than treating it as a reverse lookup.
            Step::Lookup {
                parent: "/",
                name: "8.8.8.8",
            },
            Step::List("/example.com"),
            Step::List("/reverse"),
            Step::Lookup {
                parent: "/reverse",
                name: "8.8.8.8",
            },
            // An unparseable IP fails the `{ip}` path-segment validator, so the
            // reverse route does not match and the child is absent.
            Step::Lookup {
                parent: "/reverse",
                name: "not-an-ip",
            },
            Step::Lookup {
                parent: "/@cloudflare",
                name: "example.com",
            },
            Step::Lookup {
                parent: "/@cloudflare",
                name: "reverse",
            },
            Step::Lookup {
                parent: "/@cloudflare",
                name: "8.8.8.8",
            },
            // A per-resolver reverse directory does not eagerly list its dynamic
            // children; the listing is empty even though `{ip}` lookups resolve.
            Step::List("/@cloudflare/reverse"),
            Step::Lookup {
                parent: "/@cloudflare/reverse",
                name: "8.8.8.8",
            },
            Step::Lookup {
                parent: "/@cloudflare/reverse",
                name: "not-an-ip",
            },
        ],
    });
}

/// Read real DNS records through the `DoH` wire path against the default resolver.
/// The typed reads (`A`, `AAAA`) exercise the single-fetch record path, `raw`
/// exercises the dig-style dump formatter, `all` exercises the parallel
/// fan-out that queries every common record type at once (its callout burst is
/// the multi-entry tape), and the explicit `@cloudflare` read proves the
/// per-resolver record route resolves end to end. Each callout is a real
/// recorded `DoH` fetch whose binary wire body tapes as `TapeBody::Base64`; the
/// snapshot captures the rendered records and the dynamic, no-canonical effects.
#[test]
fn records() {
    run(&Scenario {
        name: "records",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: DNS_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::Read("/example.com/A"),
            Step::Read("/example.com/AAAA"),
            Step::Read("/example.com/raw"),
            Step::Read("/example.com/all"),
            Step::Read("/@cloudflare/example.com/A"),
        ],
    });
}
