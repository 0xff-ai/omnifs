//! Data-driven kubernetes scenarios over the callout tape system.
//!
//! Recording prerequisite: the `just dev` k3s fixture
//! (`providers/kubernetes/dev/compose.yaml`) must be up, plus a host-side
//! `kubectl proxy` bridging it onto a unix socket, because a container-created
//! socket does not bridge out to a macOS host. The full recipe lives in
//! `providers/kubernetes/dev/README.md`; the short form:
//!
//! ```bash
//! export OMNIFS_K8S_SOCK_DIR=$(mktemp -d)
//! docker compose -p omnifs-itest-k8s -f providers/kubernetes/dev/compose.yaml up -d --wait
//! docker compose -p omnifs-itest-k8s -f providers/kubernetes/dev/compose.yaml \
//!     cp k3s:/output/kubeconfig.yaml /tmp/omnifs-itest-k8s-kubeconfig.yaml
//! kubectl --kubeconfig /tmp/omnifs-itest-k8s-kubeconfig.yaml config set-cluster default \
//!     --server=https://127.0.0.1:16443
//! kubectl proxy --kubeconfig /tmp/omnifs-itest-k8s-kubeconfig.yaml \
//!     --unix-socket=/tmp/omnifs-itest-k8s.sock &
//! just host itest-record kubernetes
//! ```
//!
//! The socket path `/tmp/omnifs-itest-k8s.sock` is PINNED: the provider embeds
//! it (hex-encoded) in every `unix://` request URL, so it is part of the
//! checked-in tapes' match keys. Re-recording must serve the proxy at exactly
//! that path.
//!
//! Snapshot churn on re-record is expected data noise, not drift:
//! resourceVersions, uids, and event timestamps come from a freshly booted
//! cluster. Replay is byte-deterministic against the checked-in tape.

use omnifs_itest::scenario::{Scenario, Step, run};
use omnifs_itest::tape::scrub::TapeRules;

/// The kubernetes mount config the scenarios record against. No auth: kubectl
/// proxy terminates TLS and injects the cluster credential, so the provider
/// speaks plain HTTP over the socket. The dynamic unix-socket grant resolves
/// from the `endpoint` config field, mirroring the dev fixture's `mount.json`.
const KUBERNETES_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_kubernetes.wasm",
    "mount": "k8s",
    "config": {
        "endpoint": "unix:///tmp/omnifs-itest-k8s.sock"
    },
    "capabilities": {
        "unix_sockets": { "dynamic": true }
    }
}
"#;

/// Browse the projection top-down against real API discovery: the static root,
/// the namespace listing, the cluster-scoped and namespaced type catalogs, and
/// resource collections fetched at their discovered group-version roots (core
/// `configmaps` under `/api/v1`, grouped `deployments` under `/apis/apps/v1`,
/// and the plural-collision `events.events.k8s.io` routed to its own group).
/// The type catalogs also prove discovery filtering on real data: subresources
/// (`pods/log`) and get+list-less resources (`bindings`) never surface.
#[test]
fn cluster_browse() {
    run(&Scenario {
        name: "cluster-browse",
        dir: "kubernetes",
        config: KUBERNETES_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::List("/"),
            Step::List("/namespaces"),
            Step::List("/cluster"),
            Step::List("/namespaces/demo"),
            Step::List("/namespaces/demo/configmaps"),
            Step::List("/namespaces/demo/deployments"),
            Step::List("/namespaces/demo/events.events.k8s.io"),
        ],
    });
}

/// Read the per-object files off the fixture-seeded workloads: the canonical
/// `manifest.json` (server `managedFields` stripped, everything else, including
/// annotations, resourceVersion, and uid, survives verbatim), its YAML
/// representation, `status.yaml` for an object with status (the deployment) and
/// one without (`null` for the configmap), and `events.txt` in both the
/// populated and the `No events.` arm.
#[test]
fn object_files() {
    run(&Scenario {
        name: "object-files",
        dir: "kubernetes",
        config: KUBERNETES_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::Read("/namespaces/demo/configmaps/greeting/manifest.json"),
            Step::Read("/namespaces/demo/configmaps/greeting/manifest.yaml"),
            Step::Read("/namespaces/demo/deployments/ticker/status.yaml"),
            Step::Read("/namespaces/demo/configmaps/greeting/status.yaml"),
            Step::Read("/namespaces/demo/deployments/ticker/events.txt"),
            Step::Read("/namespaces/demo/configmaps/greeting/events.txt"),
        ],
    });
}
