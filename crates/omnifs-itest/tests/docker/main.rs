#![cfg(not(target_os = "wasi"))]

mod support;

use omnifs_engine::test_support::TestOp;
use omnifs_wit::provider::types::Effects;
use omnifs_wit::provider::types::{
    ByteSource, CalloutResult, FsKind, HttpResponse, ListChildrenResult, ReadFileOutcome, Stability,
};
use support::{TestOpExt, docker_harness, project_paths};

const CONTAINER_ID: &str = "0123456789ab";

fn inspect_body(status: &str, running: bool) -> Vec<u8> {
    format!(
        r#"{{
            "Id": "{CONTAINER_ID}",
            "Name": "/web",
            "Config": {{"Image": "nginx:latest"}},
            "State": {{"Status": "{status}", "Running": {running}}}
        }}"#
    )
    .into_bytes()
}

fn resume_http<T>(op: &mut TestOp<'_, T>, body: Vec<u8>) {
    op.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body,
    })])
    .unwrap();
}

fn assert_no_persistent_effects(effects: &Effects) {
    assert!(
        effects.canonical.is_empty(),
        "docker must not store canonical objects: {:?}",
        effects.canonical
    );
    assert!(
        effects.invalidations.is_empty(),
        "route-shaped docker reads should not emit invalidations: {:?}",
        effects.invalidations
    );
}

fn assert_project_paths(effects: &Effects, expected: &[&str]) {
    assert_eq!(
        project_paths(effects),
        expected,
        "unexpected docker preload files: {:?}",
        effects.fs
    );
}

fn assert_projected_inline_file(effects: &Effects, path: &str, expected: &[u8]) {
    let write = effects
        .fs
        .iter()
        .find(|write| write.path == path)
        .unwrap_or_else(|| panic!("expected docker preload file {path}"));
    let FsKind::File(file) = &write.kind else {
        panic!("expected docker preload file {path}, got {:?}", write.kind);
    };
    assert_eq!(file.attrs.stability, Stability::Dynamic);
    match &file.bytes {
        ByteSource::Inline(bytes) => assert_eq!(bytes.as_slice(), expected),
        other => panic!("expected inline docker preload for {path}, got {other:?}"),
    }
}

fn assert_no_project_effects(effects: &Effects) {
    assert!(
        project_paths(effects).is_empty(),
        "docker should not preload files for this operation: {:?}",
        effects.fs
    );
}

fn assert_inline_file(op: &TestOp<'_, ReadFileOutcome>, expected: &[u8]) {
    match op.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            match &file.bytes {
                ByteSource::Inline(bytes) => assert_eq!(bytes.as_slice(), expected),
                other => panic!("expected inline docker bytes, got {other:?}"),
            }
        },
        other => panic!("expected docker read result, got {other:?}"),
    }
}

#[test]
fn docker_inspect_fetches_fresh_without_canonical_store() {
    let harness = docker_harness();
    let stub = inspect_body("running", true);
    let mut op = harness
        .read("/containers/by-name/web/inspect.json")
        .unwrap();
    let fetch = op.expect_single_fetch();
    assert!(
        fetch.url.ends_with("/v1.43/containers/web/json"),
        "unexpected inspect URL: {}",
        fetch.url
    );

    resume_http(&mut op, stub.clone());
    let effects = op.effects().unwrap();
    assert_no_persistent_effects(effects);
    assert_project_paths(effects, &["/state", "/summary.txt"]);
    assert_projected_inline_file(effects, "/state", b"running\n");
    assert_inline_file(&op, &stub);
}

#[test]
fn docker_state_fetches_fresh_on_each_read() {
    let harness = docker_harness();

    let mut first_op = harness
        .read("/containers/by-id/0123456789ab/state")
        .unwrap();
    let first_fetch = first_op.expect_single_fetch();
    assert!(
        first_fetch
            .url
            .ends_with("/v1.43/containers/0123456789ab/json"),
        "unexpected state URL: {}",
        first_fetch.url
    );
    resume_http(&mut first_op, inspect_body("running", true));
    let effects = first_op.effects().unwrap();
    assert_no_persistent_effects(effects);
    assert_project_paths(effects, &["/summary.txt"]);
    assert_inline_file(&first_op, b"running\n");

    let mut second_op = harness
        .read("/containers/by-id/0123456789ab/state")
        .unwrap();
    let second_fetch = second_op.expect_single_fetch();
    assert!(
        second_fetch
            .url
            .ends_with("/v1.43/containers/0123456789ab/json"),
        "unexpected second state URL: {}",
        second_fetch.url
    );
    resume_http(&mut second_op, inspect_body("exited", false));
    let effects = second_op.effects().unwrap();
    assert_no_persistent_effects(effects);
    assert_project_paths(effects, &["/summary.txt"]);
    assert_inline_file(&second_op, b"exited\n");
}

#[test]
fn docker_summary_txt_renders_from_fresh_inspect() {
    let harness = docker_harness();
    let mut op = harness.read("/containers/by-name/web/summary.txt").unwrap();
    let fetch = op.expect_single_fetch();
    assert!(
        fetch.url.ends_with("/v1.43/containers/web/json"),
        "unexpected summary URL: {}",
        fetch.url
    );

    resume_http(&mut op, inspect_body("running", true));
    let effects = op.effects().unwrap();
    assert_no_persistent_effects(effects);
    assert_project_paths(effects, &["/state"]);
    assert_projected_inline_file(effects, "/state", b"running\n");
    match op.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => match &file.bytes {
            ByteSource::Inline(body) => {
                let text = std::str::from_utf8(body.as_slice()).unwrap();
                assert!(text.contains("id     0123456789ab"));
                assert!(text.contains("name   web"));
                assert!(text.contains("image  nginx:latest"));
                assert!(text.contains("state  running"));
            },
            other => panic!("expected inline summary, got {other:?}"),
        },
        other => panic!("expected summary read, got {other:?}"),
    }
}

#[test]
fn docker_container_dir_lists_route_shaped_leaves() {
    let harness = docker_harness();
    let op = harness.list("/containers/by-name/web").unwrap();

    match op.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let mut names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            names.sort_unstable();
            assert_eq!(names, vec!["inspect.json", "state", "summary.txt"]);
        },
        other => panic!("expected container dir listing, got {other:?}"),
    }
}

#[test]
fn docker_by_name_listing_enumerates_names() {
    let harness = docker_harness();
    let mut op = harness.list("/containers/by-name").unwrap();
    let fetch = op.expect_single_fetch();
    assert!(
        fetch.url.ends_with("/v1.43/containers/json?all=true"),
        "unexpected listing URL: {}",
        fetch.url
    );

    resume_http(
        &mut op,
        br#"[{"Id":"0123456789ab","Names":["/web","/api"]}]"#.to_vec(),
    );

    match op.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let mut names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            names.sort_unstable();
            assert_eq!(names, vec!["api", "web"]);
        },
        other => panic!("expected by-name listing, got {other:?}"),
    }
    let effects = op.effects().unwrap();
    assert_no_persistent_effects(effects);
    assert_no_project_effects(effects);
}

#[test]
fn docker_compose_routes_enumerate_services_and_containers() {
    let harness = docker_harness();
    let mut services_op = harness.list("/compose/demo/services").unwrap();
    let services_fetch = services_op.expect_single_fetch();
    assert!(
        services_fetch
            .url
            .ends_with("/v1.43/containers/json?all=true"),
        "unexpected services URL: {}",
        services_fetch.url
    );
    resume_http(
        &mut services_op,
        br#"[{"Id":"0123456789ab","Names":["/web"],"Labels":{"com.docker.compose.project":"demo","com.docker.compose.service":"api"}}]"#.to_vec(),
    );
    match services_op.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["api"]);
        },
        other => panic!("expected compose services listing, got {other:?}"),
    }

    let mut containers_op = harness
        .list("/compose/demo/services/api/containers")
        .unwrap();
    let containers_fetch = containers_op.expect_single_fetch();
    assert!(
        containers_fetch
            .url
            .ends_with("/v1.43/containers/json?all=true"),
        "unexpected containers URL: {}",
        containers_fetch.url
    );
    resume_http(
        &mut containers_op,
        br#"[{"Id":"0123456789ab","Names":["/web"],"Labels":{"com.docker.compose.project":"demo","com.docker.compose.service":"api"}}]"#.to_vec(),
    );
    match containers_op.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["web"]);
        },
        other => panic!("expected compose containers listing, got {other:?}"),
    }
}

#[test]
fn docker_compose_container_leaf_uses_shared_fresh_handler() {
    let harness = docker_harness();
    let mut op = harness
        .read("/compose/demo/services/api/containers/web/state")
        .unwrap();
    let fetch = op.expect_single_fetch();
    assert!(
        fetch.url.ends_with("/v1.43/containers/web/json"),
        "unexpected compose state URL: {}",
        fetch.url
    );

    resume_http(&mut op, inspect_body("running", true));
    let effects = op.effects().unwrap();
    assert_no_persistent_effects(effects);
    assert_project_paths(effects, &["/summary.txt"]);
    assert_inline_file(&op, b"running\n");
}
