//! Kubernetes provider integration-test helpers.
//!
//! These drive the provider through the host the way every consumer does:
//! `list`/`lookup`/`read` ops whose HTTP callouts are answered with canned
//! Kubernetes API responses. No live cluster is needed.

use omnifs_host::TestOp;
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::provider::types::{
    ByteSource, CalloutResult, Header, HttpResponse, ListChildrenResult, OpResult, ReadFileOutcome,
};

pub use omnifs_itest::TestOpExt;

pub fn kube_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_kubernetes.wasm",
            "mount": "k8s",
            "config": {
                "endpoint": "unix:///run/omnifs/k8s.sock"
            }
        }
    "#,
    )
}

// Canned API discovery, shaped to exercise scope, subresource/verb filtering,
// plural collisions, and version preference in one catalog. Mirrors the
// fixtures the provider's former in-crate discovery unit tests used.

const CORE_V1: &str = r#"{
  "kind": "APIResourceList",
  "groupVersion": "v1",
  "resources": [
    {"name":"bindings","singularName":"binding","namespaced":true,"kind":"Binding","verbs":["create"]},
    {"name":"pods","singularName":"pod","namespaced":true,"kind":"Pod","verbs":["get","list","watch"]},
    {"name":"pods/log","singularName":"","namespaced":true,"kind":"Pod","verbs":["get"]},
    {"name":"pods/status","singularName":"","namespaced":true,"kind":"Pod","verbs":["get","patch"]},
    {"name":"configmaps","singularName":"configmap","namespaced":true,"kind":"ConfigMap","verbs":["get","list"]},
    {"name":"nodes","singularName":"node","namespaced":false,"kind":"Node","verbs":["get","list"]},
    {"name":"namespaces","singularName":"namespace","namespaced":false,"kind":"Namespace","verbs":["get","list"]},
    {"name":"events","singularName":"event","namespaced":true,"kind":"Event","verbs":["get","list"]}
  ]
}"#;

const API_GROUPS: &str = r#"{
  "kind": "APIGroupList",
  "groups": [
    {"name":"apps",
     "versions":[{"groupVersion":"apps/v1","version":"v1"}],
     "preferredVersion":{"groupVersion":"apps/v1","version":"v1"}},
    {"name":"events.k8s.io",
     "versions":[{"groupVersion":"events.k8s.io/v1","version":"v1"}],
     "preferredVersion":{"groupVersion":"events.k8s.io/v1","version":"v1"}},
    {"name":"example.io",
     "versions":[{"groupVersion":"example.io/v2","version":"v2"},{"groupVersion":"example.io/v1","version":"v1"}],
     "preferredVersion":{"groupVersion":"example.io/v2","version":"v2"}}
  ]
}"#;

const APPS_V1: &str = r#"{"groupVersion":"apps/v1","resources":[
  {"name":"deployments","singularName":"deployment","namespaced":true,"kind":"Deployment","verbs":["get","list"]},
  {"name":"deployments/scale","singularName":"","namespaced":true,"kind":"Scale","verbs":["get"]}
]}"#;

const EVENTS_GROUP_V1: &str = r#"{"groupVersion":"events.k8s.io/v1","resources":[
  {"name":"events","singularName":"event","namespaced":true,"kind":"Event","verbs":["get","list"]}
]}"#;

const EXAMPLE_V2: &str = r#"{"groupVersion":"example.io/v2","resources":[
  {"name":"bars","singularName":"bar","namespaced":true,"kind":"Bar","verbs":["get","list"]}
]}"#;

const EXAMPLE_V1: &str = r#"{"groupVersion":"example.io/v1","resources":[
  {"name":"bars","singularName":"bar","namespaced":true,"kind":"Bar","verbs":["get","list"]},
  {"name":"legacies","singularName":"legacy","namespaced":true,"kind":"Legacy","verbs":["get","list"]}
]}"#;

pub fn http_ok(body: &[u8]) -> CalloutResult {
    CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::<Header>::new(),
        body: body.to_vec(),
    })
}

fn discovery_response(url: &str) -> CalloutResult {
    let body = if url.ends_with("/api/v1") {
        CORE_V1
    } else if url.ends_with("/apis") {
        API_GROUPS
    } else if url.ends_with("/apis/apps/v1") {
        APPS_V1
    } else if url.ends_with("/apis/events.k8s.io/v1") {
        EVENTS_GROUP_V1
    } else if url.ends_with("/apis/example.io/v2") {
        EXAMPLE_V2
    } else if url.ends_with("/apis/example.io/v1") {
        EXAMPLE_V1
    } else {
        panic!("unexpected discovery URL: {url}");
    };
    http_ok(body.as_bytes())
}

/// Drive a `/cluster` listing, answering the sequential discovery callouts
/// (`/api/v1`, `/apis`, then each group's preferred-first versions). Leaves the
/// discovery catalog cached in provider state for the rest of the test, and
/// returns the projected cluster-scoped type names (sorted).
pub fn warm_discovery(harness: &RuntimeHarness) -> Vec<String> {
    let mut op = harness.list("/cluster").unwrap();
    while op.is_suspended() {
        let responses: Vec<CalloutResult> = op
            .expect_fetches()
            .iter()
            .map(|fetch| discovery_response(&fetch.url))
            .collect();
        op.resume(responses).unwrap();
    }
    sorted_entry_names(op.into_list_children().unwrap())
}

/// List a directory that should resolve from cached discovery alone (no
/// callout), returning its entry names sorted.
pub fn list_type_names(harness: &RuntimeHarness, path: &str) -> Vec<String> {
    let op = harness.list(path).unwrap();
    assert!(
        !op.is_suspended(),
        "type listing for {path} should reuse cached discovery with no callout"
    );
    sorted_entry_names(op.into_list_children().unwrap())
}

pub fn sorted_entry_names(result: ListChildrenResult) -> Vec<String> {
    match result {
        ListChildrenResult::Entries(listing) => {
            let mut names: Vec<String> = listing
                .entries
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            names.sort();
            names
        },
        other => panic!("expected an entries listing, got {other:?}"),
    }
}

/// The served bytes of a completed read, whether the terminal serves the
/// canonical store (identity representations) or inline bytes (projections).
pub fn read_bytes(op: &TestOp<'_>) -> Vec<u8> {
    match op.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => match &file.bytes {
            ByteSource::Canonical => op.effects().unwrap().canonical[0].bytes.clone(),
            ByteSource::Inline(bytes) => bytes.clone(),
            other => panic!("expected canonical or inline read bytes, got {other:?}"),
        },
        other => panic!("expected a found read, got {other:?}"),
    }
}
