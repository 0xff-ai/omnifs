#![cfg(not(target_os = "wasi"))]

mod scenarios;
mod support;

use support::{
    TestOpExt, http_ok, kube_harness, list_type_names, read_bytes, sorted_entry_names,
    warm_discovery,
};

// The happy-path projection surface (listings, discovered group-version
// collections, manifest.json managed-fields stripping, status.yaml both arms,
// events.txt both arms) is covered by the recorded scenarios in scenarios.rs
// (`cluster-browse`, `object-files`). The tests kept here assert surfaces the
// step trace cannot render (request URLs) or catalog shapes the live fixture
// cannot produce (synthetic multi-version groups).

/// Discovery classification against a synthetic catalog the live fixture
/// cannot reproduce: version preference across a multi-version group (`bars`
/// surfaces from `example.io/v2`; `legacies`, present only in `v1`, is still
/// kept). The real-catalog aspects (subresource and verb filtering, plural
/// collisions, scope split) are also covered by the `cluster-browse` scenario.
#[test]
fn cluster_and_namespace_listings_classify_scope_and_filter_unreadable() {
    let harness = kube_harness();

    let cluster_types = warm_discovery(&harness);
    assert_eq!(cluster_types, vec!["README.md", "namespaces", "nodes"]);

    let namespaced_types = list_type_names(&harness, "/namespaces/demo");
    assert_eq!(
        namespaced_types,
        vec![
            "bars",
            "configmaps",
            "deployments",
            "events",
            "events.events.k8s.io",
            "legacies",
            "pods",
        ]
    );
}

/// A resource collection is fetched at its discovered group-version root: core
/// types under `/api/v1`, grouped types under `/apis/<group>/<version>`. A
/// plural collision's qualified name routes to its own group while the bare
/// name stays on core. Kept for the request-URL assertions the step trace
/// cannot render; the listing outcomes themselves are covered by the
/// `cluster-browse` scenario.
#[test]
fn resource_collections_use_discovered_group_version_paths() {
    let harness = kube_harness();
    warm_discovery(&harness);

    let mut core_events = harness.list("/namespaces/demo/events").unwrap();
    assert!(
        core_events
            .expect_single_fetch()
            .url
            .ends_with("/api/v1/namespaces/demo/events")
    );
    core_events
        .answer_callouts(vec![http_ok(br#"{"items":[]}"#)])
        .unwrap();

    let mut deployments = harness.list("/namespaces/demo/deployments").unwrap();
    assert!(
        deployments
            .expect_single_fetch()
            .url
            .ends_with("/apis/apps/v1/namespaces/demo/deployments")
    );
    deployments
        .answer_callouts(vec![http_ok(
            br#"{"items":[{"metadata":{"name":"ticker"}}]}"#,
        )])
        .unwrap();
    assert_eq!(
        sorted_entry_names(deployments.into_list_children().unwrap()),
        vec!["ticker"]
    );

    let qualified = harness
        .list("/namespaces/demo/events.events.k8s.io")
        .unwrap();
    assert!(
        qualified
            .expect_single_fetch()
            .url
            .ends_with("/apis/events.k8s.io/v1/namespaces/demo/events")
    );
}

/// `events.txt` first loads the object (for its kind and uid), then queries the
/// core events collection with a kubectl-shaped `involvedObject` field
/// selector. Kept for the field-selector URL assertion the step trace cannot
/// render; the events.txt read flow is covered by the `object-files` scenario.
#[test]
fn events_txt_uses_kubectl_field_selector() {
    let harness = kube_harness();
    warm_discovery(&harness);

    let mut op = harness
        .read("/namespaces/demo/pods/web/events.txt")
        .unwrap();
    assert!(
        op.expect_single_fetch()
            .url
            .ends_with("/api/v1/namespaces/demo/pods/web"),
        "events.txt first loads the object for its kind/uid"
    );
    op.answer_callouts(vec![http_ok(
        br#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"web","namespace":"demo","uid":"abc-123"}}"#,
    )])
    .unwrap();

    let events_url = op.expect_single_fetch().url.clone();
    assert!(events_url.contains("/api/v1/namespaces/demo/events"));
    assert_eq!(
        field_selector_of(&events_url),
        "involvedObject.name=web,involvedObject.namespace=demo,involvedObject.kind=Pod,involvedObject.uid=abc-123"
    );
    op.answer_callouts(vec![http_ok(br#"{"items":[]}"#)])
        .unwrap();
    assert_eq!(read_bytes(&op), b"No events.\n");
}

/// Extract and percent-decode the `fieldSelector` query value from a fetch URL.
fn field_selector_of(url: &str) -> String {
    let query = url.split_once('?').map_or("", |(_, query)| query);
    let raw = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("fieldSelector="))
        .unwrap_or_else(|| panic!("no fieldSelector in {url}"));
    raw.replace("%3D", "=")
        .replace("%3d", "=")
        .replace("%2C", ",")
        .replace("%2c", ",")
}
