//! Data-driven web scenarios over the callout tape system.
//!
//! Each scenario records real HTTP callouts once (via
//! `just host itest-record web <scenario>`) and replays them hermetically in the
//! default host-test lane. The provider is unauthenticated, so no credential is
//! seeded; the recorded upstreams are durable example.com-class pages
//! (`example.com` and IANA's example-domains help page) whose HTML is stable
//! enough to tape.
//!
//! The web surface is two file routes: `/https/{host}/{*rest}` extracts the
//! fetched HTML to markdown via readability, and `/raw/https/{host}/{*rest}`
//! passes the fetched bytes through verbatim. Both routes fetch the site root
//! when `rest` is empty. Scenarios read through both routes; the readability
//! transform itself is proven against controlled fixture HTML by the unit-shaped
//! tests in `main.rs`.

use omnifs_itest::scenario::{Scenario, Step, run};
use omnifs_itest::tape::scrub::TapeRules;

/// The web mount config the scenarios record against: dynamic domain capability
/// with the two durable fixture hosts enumerated in the mount's `domains`
/// allowlist. No auth block: the provider is unauthenticated.
const WEB_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_web.wasm",
    "mount": "web",
    "capabilities": {
        "domains": { "dynamic": true }
    },
    "config": {
        "domains": ["example.com", "www.iana.org"]
    }
}
"#;

/// Read the markdown route against two durable pages: `example.com` with an
/// empty rest (exercising the empty-rest -> site-root fetch through the
/// readability route) and IANA's example-domains help page with a nested rest
/// (exercising rest routing on a content-rich page). Both callouts are real
/// recorded HTTP fetches; the snapshot captures the extracted markdown.
#[test]
fn read_markdown() {
    run(&Scenario {
        name: "read-markdown",
        dir: "web",
        config: WEB_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::Read("/https/example.com"),
            Step::Read("/https/www.iana.org/help/example-domains"),
        ],
    });
}

/// Read the raw route against `example.com` with an empty rest: the provider
/// fetches the site root and passes the response bytes through verbatim as an
/// octet stream, without the readability transform. The snapshot captures the
/// raw HTML, proving the raw route is byte-faithful against a real upstream.
#[test]
fn read_raw() {
    run(&Scenario {
        name: "read-raw",
        dir: "web",
        config: WEB_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[Step::Read("/raw/https/example.com")],
    });
}
