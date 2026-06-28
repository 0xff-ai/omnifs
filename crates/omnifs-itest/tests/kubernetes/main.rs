#![cfg(not(target_os = "wasi"))]

mod support;

use serde_json::{Value, json};
use support::{
    TestOpExt, http_ok, kube_harness, list_type_names, read_bytes, sorted_entry_names,
    warm_discovery,
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
/// lacking `get`+`list` (`bindings`). Plural collisions are qualified by group
/// (core `events` keeps the bare name; `events.k8s.io` becomes
/// `events.events.k8s.io`), and only the preferred group version surfaces
/// (`bars` from `example.io/v2`, while `legacies`, present only in `v1`, is
/// still kept).
#[test]
fn cluster_and_namespace_listings_classify_scope_and_filter_unreadable() {
    let harness = kube_harness();

    let cluster_types = warm_discovery(&harness);
    assert_eq!(cluster_types, vec!["namespaces", "nodes"]);

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
/// name stays on core.
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
