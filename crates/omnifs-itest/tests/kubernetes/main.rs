#![cfg(not(target_os = "wasi"))]

mod support;

use omnifs_wit::provider::types::{ErrorKind, ListChildrenResult};
use serde_json::{Value, json};
use support::{
    TestOpExt, answer_partial_discovery, http_ok, kube_harness, list_type_names, read_bytes,
    sorted_entry_names, warm_discovery,
};

const CONFIGMAP_JSON: &str = r#"{
  "apiVersion": "v1",
  "kind": "ConfigMap",
  "metadata": {
    "name": "greeting",
    "namespace": "demo",
    "resourceVersion": "12",
    "uid": "abc",
    "managedFields": [{"manager": "kubectl"}],
    "annotations": {
      "kubectl.kubernetes.io/last-applied-configuration": "{...}",
      "keep": "me"
    }
  },
  "data": {"message": "hello"}
}"#;

/// Discovery classifies resources by scope and drops what is not browsable:
/// subresources (`pods/log`, `pods/status`, `deployments/scale`) and resources
/// lacking `get`+`list` (`bindings`). Core resources keep bare names while
/// every grouped resource uses `<plural>.<group>`, and only the preferred
/// group version surfaces (`bars` from `example.io/v2`, while `legacies`,
/// present only in `v1`, is still kept).
#[test]
fn cluster_and_namespace_listings_classify_scope_and_filter_unreadable() {
    let harness = kube_harness();

    let cluster_types = warm_discovery(&harness);
    assert_eq!(cluster_types, vec!["README.md", "namespaces", "nodes"]);

    let namespaced_types = list_type_names(&harness, "/namespaces/demo");
    assert_eq!(
        namespaced_types,
        vec![
            "bars.example.io",
            "configmaps",
            "deployments.apps",
            "events",
            "events.events.k8s.io",
            "legacies.example.io",
            "pods",
        ]
    );
}

#[test]
fn partial_discovery_stays_open_qualifies_groups_and_retries_unknown_types() {
    let harness = kube_harness();

    let mut types = harness.list("/namespaces/demo").unwrap();
    answer_partial_discovery(&mut types, Some(503), 503);
    match types.into_list_children().unwrap() {
        ListChildrenResult::Entries(listing) => {
            assert!(
                !listing.exhaustive,
                "a listing from incomplete discovery must remain open"
            );
            let mut names: Vec<String> = listing
                .entries
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            names.sort();
            assert_eq!(names, vec!["bars.example.io", "legacies.example.io"]);
        },
        other => panic!("expected partial type entries, got {other:?}"),
    }

    let mut omitted_core_type = harness.list("/namespaces/demo/pods").unwrap();
    assert!(
        omitted_core_type.is_waiting_for_callouts(),
        "a partial discovery snapshot must be retried by the next operation"
    );
    answer_partial_discovery(&mut omitted_core_type, Some(503), 503);
    match omitted_core_type.result().unwrap() {
        Err(error) => assert_eq!(error.kind, ErrorKind::Network),
        other => panic!("expected retained discovery Network error, got {other:?}"),
    }

    warm_discovery(&harness);
    let mut recovered = harness.list("/namespaces/demo/bars.example.io").unwrap();
    assert!(
        recovered
            .expect_single_fetch()
            .url
            .ends_with("/apis/example.io/v2/namespaces/demo/bars"),
        "the grouped path must remain stable and routable after complete recovery"
    );
    recovered
        .answer_callouts(vec![http_ok(br#"{"items":[]}"#)])
        .unwrap();

    let not_found_harness = kube_harness();
    let mut unknown = not_found_harness.list("/namespaces/demo/unknowns").unwrap();
    answer_partial_discovery(&mut unknown, None, 404);
    match unknown.result().unwrap() {
        Err(error) => {
            assert_eq!(error.kind, ErrorKind::Network);
            assert!(
                error.retryable,
                "a missing discovery source is not authoritative type absence"
            );
        },
        other => panic!("expected normalized discovery-source error, got {other:?}"),
    }
}

/// A resource collection is fetched at its discovered group-version root: core
/// types under `/api/v1`, grouped types under `/apis/<group>/<version>`. Every
/// grouped type keeps its qualified filesystem name while core types keep bare
/// names.
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

    let mut deployments = harness.list("/namespaces/demo/deployments.apps").unwrap();
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
        sorted_entry_names(deployments.into_ok().unwrap()),
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

/// `manifest.json` is the verbatim object minus server-managed noise:
/// `metadata.managedFields` is stripped, while everything else (the
/// last-applied-configuration annotation, user annotations, resourceVersion,
/// uid) survives.
#[test]
fn object_manifest_strips_only_managed_fields() {
    let harness = kube_harness();
    warm_discovery(&harness);

    let mut op = harness
        .read("/namespaces/demo/configmaps/greeting/manifest.json")
        .unwrap();
    assert!(
        op.expect_single_fetch()
            .url
            .ends_with("/api/v1/namespaces/demo/configmaps/greeting")
    );
    op.answer_callouts(vec![http_ok(CONFIGMAP_JSON.as_bytes())])
        .unwrap();

    let canonical: Value = serde_json::from_slice(&read_bytes(&op)).unwrap();
    let meta = canonical.get("metadata").expect("metadata present");
    assert!(
        meta.get("managedFields").is_none(),
        "managedFields must be stripped"
    );
    assert_eq!(
        meta.pointer("/annotations/kubectl.kubernetes.io~1last-applied-configuration"),
        Some(&Value::String("{...}".to_string()))
    );
    assert_eq!(meta.pointer("/annotations/keep"), Some(&json!("me")));
    assert_eq!(meta.pointer("/resourceVersion"), Some(&json!("12")));
    assert_eq!(meta.pointer("/uid"), Some(&json!("abc")));
}

/// `status.yaml` renders the object's `.status` as YAML, or `null` when the
/// object carries no status.
#[test]
fn status_yaml_renders_status_or_null() {
    let harness = kube_harness();
    warm_discovery(&harness);

    let mut pod = harness
        .read("/namespaces/demo/pods/web/status.yaml")
        .unwrap();
    assert!(
        pod.expect_single_fetch()
            .url
            .ends_with("/api/v1/namespaces/demo/pods/web")
    );
    pod.answer_callouts(vec![http_ok(
        br#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"web"},"status":{"phase":"Running"}}"#,
    )])
    .unwrap();
    assert_eq!(read_bytes(&pod), b"phase: Running\n");

    let mut configmap = harness
        .read("/namespaces/demo/configmaps/greeting/status.yaml")
        .unwrap();
    assert!(
        configmap
            .expect_single_fetch()
            .url
            .ends_with("/api/v1/namespaces/demo/configmaps/greeting")
    );
    configmap
        .answer_callouts(vec![http_ok(
            br#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"greeting"},"data":{"k":"v"}}"#,
        )])
        .unwrap();
    assert_eq!(read_bytes(&configmap), b"null\n");
}

/// `events.txt` first loads the object (for its kind and uid), then queries the
/// core events collection with a kubectl-shaped `involvedObject` field
/// selector.
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
