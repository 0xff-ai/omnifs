//! Data-driven docker scenarios over the callout tape system.
//!
//! Docker has no auth (the daemon socket is the trust boundary) and no
//! canonical store (every read is a route-shaped direct fetch; every step's
//! trace renders `canonical: (none)`). Recording targets the local Docker
//! daemon over its UDS.
//!
//! Every step is scoped to a dedicated fixture container
//! (`omnifs-itest-docker-fixture`, compose project `omnifs-itest-fixture`,
//! service `web`) created before recording and removed after: the daemon's
//! `/containers/json` listing endpoint has no server-side name filter, so
//! any step that lists containers embeds every container running on the
//! recording machine (dev machines routinely run unrelated containers) into
//! a checked-in tape. Listing routes (`by-name`, `by-id`, `running`,
//! `stopped`, and the compose project/service listings) stay covered by the
//! hand-written stub tests in `main.rs`'s `adversarial` module instead,
//! where a synthetic response gives full control over the listing contents.
//!
//! The recorded endpoint is the portable `unix:///var/run/docker.sock` (the
//! provider's own default), not the machine-specific path a particular
//! daemon distribution actually creates its socket at (this machine's is
//! `OrbStack`'s `~/.orbstack/run/docker.sock`, symlinked from
//! `/var/run/docker.sock`): the socket path is hex-encoded straight into the
//! callout's request URL (`crates/omnifs-sdk/src/http.rs`'s `unix://` URL
//! builder), so recording against the portable path keeps that URL
//! reproducible across recording machines instead of embedding one
//! contributor's home directory.
use omnifs_itest::scenario::{Scenario, Step, run};
use omnifs_itest::tape::scrub::TapeRules;

const DOCKER_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_docker.wasm",
    "mount": "docker",
    "capabilities": {
        "unix_sockets": { "dynamic": true }
    },
    "config": {
        "endpoint": "unix:///var/run/docker.sock"
    }
}
"#;

/// Read a fixture container's three faces (`inspect.json`, `state`,
/// `summary.txt`) through every route that reaches them: `by-name`,
/// `by-id`, and the compose leaf that project/service routing shares with
/// `by-name`/`by-id`. Each read is a fresh scoped fetch
/// (`/containers/{reference}/json`) against the fixture container only, so
/// the tape never embeds another container's identity. The leading list
/// step is fully structural (the three face names are static; no daemon
/// call) and proves the route-shaped directory listing.
#[test]
fn container_faces() {
    run(&Scenario {
        name: "container-faces",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: DOCKER_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::List("/containers/by-name/omnifs-itest-docker-fixture"),
            Step::Read("/containers/by-name/omnifs-itest-docker-fixture/inspect.json"),
            Step::Read("/containers/by-id/bca3b9668811/state"),
            Step::Read("/containers/by-name/omnifs-itest-docker-fixture/summary.txt"),
            Step::Read(
                "/compose/omnifs-itest-fixture/services/web/containers/omnifs-itest-docker-fixture/state",
            ),
        ],
    });
}
