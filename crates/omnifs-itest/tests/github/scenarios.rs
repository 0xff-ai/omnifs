//! Data-driven github scenarios over the callout tape system.
//!
//! Each scenario records real GitHub HTTP callouts once (via
//! `just host itest-record github <scenario>`) and replays them hermetically in
//! the default host-test lane. The scenario reads namespace-projected files
//! only; the repo git tree (`{owner}/{repo}/repo`) is a subtree boundary served
//! by the OS from a resolved clone, not readable through the namespace read op,
//! so scenarios browse and read the HTTP-backed projection instead.

use omnifs_itest::scenario::{RecordAuth, Scenario, Step, run};
use omnifs_itest::tape::scrub::TapeRules;

/// The github mount config the scenarios record against: the `api.github.com`
/// domain for the projection callouts and a static PAT the recorder
/// authenticates with.
const GITHUB_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_github.wasm",
    "mount": "github",
    "auth": {
        "type": "static-token",
        "scheme": "pat"
    },
    "capabilities": {
        "domains": ["api.github.com"]
    }
}
"#;

/// Browse a public repo top-down: the provider root, the owner anchor (owner
/// faces merged with the repo collection), the repo anchor (gated existence plus
/// its static faces), then a read of the repo's canonical JSON out of the object
/// cache the browse warmed. Every callout is a real recorded GitHub fetch.
#[test]
fn repo_browse() {
    run(&Scenario {
        name: "repo-browse",
        dir: "github",
        config: GITHUB_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_GITHUB_TOKEN",
        }),
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::List("/"),
            Step::List("/octocat"),
            Step::List("/octocat/Hello-World"),
            Step::Read("/octocat/Hello-World/repo.json"),
        ],
    });
}
