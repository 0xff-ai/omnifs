#![cfg(not(target_os = "wasi"))]

mod scenarios;
mod support;

/// Adversarial docker coverage the recorded scenario cannot express:
///
/// - Fetch-URL shaping (the pinned `/v1.43` prefix and reference
///   interpolation across `by-name`/`by-id`/compose-leaf routes) and a
///   preloaded sibling file's exact byte content: the projection snapshot
///   renders a preloaded `fs` write's size and kind, never its bytes, and
///   never the outbound request URL.
/// - The no-caching invariant, proven by two sequential reads of the same
///   path returning different daemon state: a scenario step sequence has no
///   way to mutate live daemon state between steps.
/// - The `by-name`/compose listing routes: the daemon's `/containers/json`
///   listing has no server-side name filter, so recording it for real would
///   embed every container running on the recording machine into a checked-in
///   tape (`scenarios.rs` documents this in full). Their happy path, and the
///   compose project/service label-filtering logic, stay covered by a
///   synthetic canned listing here instead.
///
/// The container-faces happy path (structural face listing, inspect/state/
/// summary reads by name, by id, and via a compose leaf, preload chains, and
/// the no-canonical-store invariant) lives in `scenarios.rs` over a tape
/// recorded against a dedicated fixture container.
mod adversarial {
    use omnifs_engine::test_support::TestOp;
    use omnifs_wit::provider::types::Effects;
    use omnifs_wit::provider::types::{
        ByteSource, CalloutResult, FsKind, HttpResponse, ListChildrenResult, OpResult,
        ReadFileOutcome, Stability,
    };

    use crate::support::{TestOpExt, docker_harness, project_paths};

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

    fn resume_http(op: &mut TestOp<'_>, body: Vec<u8>) {
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

    fn assert_inline_file(op: &TestOp<'_>, expected: &[u8]) {
        match op.result().unwrap() {
            OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
                assert_eq!(file.attrs.stability, Stability::Dynamic);
                match &file.bytes {
                    ByteSource::Inline(bytes) => assert_eq!(bytes.as_slice(), expected),
                    other => panic!("expected inline docker bytes, got {other:?}"),
                }
            },
            other => panic!("expected docker read result, got {other:?}"),
        }
    }

    // TODO(tape): scenarios::container_faces step 1 covers this read's
    // preload/no-canonical-store happy path over a real recorded fetch; kept
    // for the fetch-URL assertion and the preloaded `/state` sibling's exact
    // byte content, neither of which the projection snapshot renders.
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

    // Two sequential reads of the same path with different daemon state
    // prove there is no caching layer between them. A scenario step sequence
    // cannot mutate the live daemon between steps, so this stays synthetic.
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

    // TODO(tape): scenarios::container_faces step 3 covers the summary.txt
    // happy path (format, real content) over a recorded fetch; kept for the
    // fetch-URL assertion and the preloaded `/state` sibling's exact byte
    // content, neither of which the projection snapshot renders.
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
            OpResult::ReadFile(ReadFileOutcome::Found(file)) => match &file.bytes {
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

    // docker_container_dir_lists_route_shaped_leaves deleted: its entire
    // op + assertion surface (list a container's face names, assert the
    // sorted entry set) is demonstrably covered by
    // scenarios::container_faces step 0, a structural list requiring no
    // callout at all.

    // The by-name listing fetches every container the daemon knows about
    // with no server-side name filter; recording it for real would embed
    // every container running on the recording machine (dev machines
    // routinely run unrelated containers) into a checked-in tape. See
    // `scenarios.rs` for the full determinism rationale. This stays a
    // synthetic canned response.
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
            OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
                let mut names: Vec<&str> =
                    listing.entries.iter().map(|e| e.name.as_str()).collect();
                names.sort_unstable();
                assert_eq!(names, vec!["api", "web"]);
            },
            other => panic!("expected by-name listing, got {other:?}"),
        }
        let effects = op.effects().unwrap();
        assert_no_persistent_effects(effects);
        assert_no_project_effects(effects);
    }

    // Same whole-daemon-listing determinism constraint as the by-name test
    // above, plus this is the only coverage of the compose project/service
    // label-filtering logic, which needs precise control over which labels
    // are present to prove the filter is exact.
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
            OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
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
            OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
                let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
                assert_eq!(names, vec!["web"]);
            },
            other => panic!("expected compose containers listing, got {other:?}"),
        }
    }

    // TODO(tape): scenarios::container_faces step 4 covers this route's
    // happy path (preload, no-canonical-store, inline bytes) over a recorded
    // fetch reaching the compose leaf; kept for the fetch-URL assertion,
    // which is this test's whole point: proving the compose leaf reaches the
    // identical `/v1.43/containers/{reference}/json` URL that by-name/by-id
    // share.
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
}
