#![cfg(not(target_os = "wasi"))]

mod support;

use omnifs_cache::RecordKind;
use omnifs_core::path::Path as OmnifsPath;
use omnifs_core::view::DirentsPayload;
use omnifs_host::LookupOutcome;
use omnifs_wit::provider::types::{
    CalloutResult, EntryKind, Header, HttpResponse, ListChildrenResult, LookupChildResult,
    OpResult, ReadFileOutcome, Stability,
};
use support::{
    TestOpExt, github_harness, project_file_is_deferred_full, project_file_stability,
    project_paths, seed_github_repo_cache,
};

#[test]
fn github_provider_routes_namespace_and_numeric_paths() {
    let harness = github_harness();
    // listing a repo dir triggers repo_gate, which fetches /repos/{owner}/{repo}
    // to gate existence. Stub a 200 to confirm the repo exists, then the router
    // merges the literal children (repo/issues/pulls/actions).
    let mut repo_listing = harness.list("/octocat/Hello-World").unwrap();
    let gate_fetch = repo_listing.expect_single_fetch();
    assert!(
        gate_fetch.url.ends_with("/repos/octocat/Hello-World"),
        "unexpected repo gate URL: {}",
        gate_fetch.url
    );
    repo_listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"name":"Hello-World"}"#.to_vec(),
        })])
        .unwrap();
    match repo_listing.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let mut names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec!["actions", "issues", "pulls", "repo", "repo.json"]
            );
        },
        other => panic!("expected repo namespace listing, got {other:?}"),
    }
    // lookup of `runs` under `actions` is structural (no fetch). The path
    // `actions/runs` is an implicit prefix dir because dir routes extend under it.
    let runs_lookup = harness
        .lookup("/octocat/Hello-World/actions", "runs")
        .unwrap();
    match runs_lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::Entry(result)) => {
            assert_eq!(result.target.name, "runs");
            assert!(
                matches!(result.target.kind, EntryKind::Directory),
                "runs should resolve as directory"
            );
        },
        other => panic!("expected structural runs dir lookup, got {other:?}"),
    }

    // Listing `runs` DOES fetch from GitHub.
    let mut runs_listed = harness.list("/octocat/Hello-World/actions/runs").unwrap();
    let runs_fetch = runs_listed.expect_single_fetch();
    assert!(
        runs_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30"),
        "unexpected action runs listing URL: {}",
        runs_fetch.url
    );

    runs_listed
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{
                "workflow_runs": [
                    {"id":123,"status":"completed","conclusion":"success"}
                ]
            }"#
            .to_vec(),
        })])
        .unwrap();
    match runs_listed.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["123"]);
        },
        other => panic!("expected runs listing, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_issue_list_projects_files() {
    use omnifs_wit::provider::types::{Callout, CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut response = harness.list("/octocat/Hello-World/issues/open").unwrap();
    assert!(
        response.is_suspended(),
        "expected suspended response, got {response:?}"
    );
    let [Callout::Fetch(fetch)] = response.callouts() else {
        panic!("expected fetch callout, got {:?}", response.callouts());
    };
    assert!(
        fetch.url.ends_with(
            "/search/issues?q=repo:octocat/Hello-World+is:issue+state:open&sort=created&order=desc&per_page=100"
        ),
        "unexpected issues list URL: {}",
        fetch.url
    );

    response
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "total_count": 1,
                "items": [
                    {
                        "number":7,
                        "title":"Issue title",
                        "body":"Issue body",
                        "state":"open",
                        "user":null
                    }
                ]
            }"#
            .to_vec(),
        })])
        .unwrap();

    // Listing preloads the issue directory shape and cheap fields already
    // present in the list row. Body-derived leaves stay deferred because
    // GitHub list responses can carry many large issue bodies.
    assert!(
        response.callouts().is_empty(),
        "list terminal should carry no callouts, got {:?}",
        response.callouts()
    );
    match response.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let mut preloaded = project_paths(response.effects().unwrap());
            preloaded.sort_unstable();
            assert_eq!(
                preloaded,
                vec![
                    "/octocat/Hello-World/issues/open/7",
                    "/octocat/Hello-World/issues/open/7/body",
                    "/octocat/Hello-World/issues/open/7/comments",
                    "/octocat/Hello-World/issues/open/7/item.json",
                    "/octocat/Hello-World/issues/open/7/item.md",
                    "/octocat/Hello-World/issues/open/7/state",
                    "/octocat/Hello-World/issues/open/7/title",
                    "/octocat/Hello-World/issues/open/7/user",
                ]
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["7"]);
            let effects = response.effects().unwrap();
            let issue_body_path = "/octocat/Hello-World/issues/open/7/body";
            let issue_md_path = "/octocat/Hello-World/issues/open/7/item.md";
            assert!(
                project_file_is_deferred_full(effects, issue_body_path),
                "{:?}",
                effects
                    .fs
                    .iter()
                    .find(|write| write.path == issue_body_path)
            );
            assert!(
                project_file_is_deferred_full(effects, issue_md_path),
                "{:?}",
                effects.fs.iter().find(|write| write.path == issue_md_path)
            );
        },
        other => panic!("expected issue listing terminal, got {other:?}"),
    }

    let harness = github_harness();
    harness.runtime.apply_effects_for_test(
        response.effects().unwrap(),
        harness.runtime.current_generation(),
    );
    let dirents = harness
        .runtime
        .view_get(
            "/octocat/Hello-World/issues/open/7",
            RecordKind::Dirents,
            None,
        )
        .and_then(|record| DirentsPayload::deserialize(&record.payload))
        .expect("issue list preload should materialize hot issue dirents");
    assert!(
        dirents.exhaustive,
        "issue dirents should be exhaustive after list preload"
    );
    let mut child_names: Vec<_> = dirents
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    child_names.sort_unstable();
    assert_eq!(
        child_names,
        vec![
            "body",
            "comments",
            "item.json",
            "item.md",
            "state",
            "title",
            "user"
        ]
    );
}

#[test]
fn github_issue_list_fetches_rest_followup_pages() {
    use omnifs_wit::provider::types::{Callout, CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut response = harness.list("/octocat/Hello-World/issues/open").unwrap();
    let [Callout::Fetch(fetch)] = response.callouts() else {
        panic!("expected first issues fetch, got {:?}", response.callouts());
    };
    assert!(fetch.url.ends_with(
        "/search/issues?q=repo:octocat/Hello-World+is:issue+state:open&sort=created&order=desc&per_page=100"
    ), "unexpected search URL: {}", fetch.url);

    response
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "total_count": 150,
                "items": [
                    {
                        "number":6,
                        "title":"Recent issue",
                        "body":"Issue body",
                        "state":"open",
                        "user":{"login":"hubot"}
                    }
                ]
            }"#
            .to_vec(),
        })])
        .unwrap();
    let [Callout::Fetch(fetch)] = response.callouts() else {
        panic!(
            "expected second issues page fetch, got {:?}",
            response.callouts()
        );
    };
    assert!(
        fetch.url.ends_with(
            "/repos/octocat/Hello-World/issues?state=open&sort=created&direction=desc&per_page=100&page=2"
        ),
        "unexpected REST page URL: {}",
        fetch.url
    );

    response
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[
                {
                    "number":7,
                    "title":"Older issue",
                    "body":"Issue body",
                    "state":"open",
                    "user":{"login":"octocat"}
                }
            ]"#
            .to_vec(),
        })])
        .unwrap();
    match response.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["6", "7"]);
            let preloaded = project_paths(response.effects().unwrap());
            assert!(
                preloaded.contains(&"/octocat/Hello-World/issues/open/7/title"),
                "missing issue title preload in {preloaded:?}"
            );
            assert!(listing.exhaustive);
        },
        other => panic!("expected issue listing terminal, got {other:?}"),
    }
}

#[test]
fn github_issue_list_dedupes_overlap_at_search_rest_seam() {
    use omnifs_wit::provider::types::{Callout, CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut response = harness.list("/octocat/Hello-World/issues/open").unwrap();
    let [Callout::Fetch(_)] = response.callouts() else {
        panic!("expected first issues fetch, got {:?}", response.callouts());
    };

    response
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "total_count": 150,
                "items": [
                    {"number":11,"title":"Search-only","body":"a","state":"open","user":{"login":"o"}},
                    {"number":10,"title":"Boundary","body":"b","state":"open","user":{"login":"o"}}
                ]
            }"#
            .to_vec(),
        })])
        .unwrap();
    let [Callout::Fetch(_)] = response.callouts() else {
        panic!("expected REST page-2 fetch, got {:?}", response.callouts());
    };

    response
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[
                {"number":10,"title":"Boundary","body":"b","state":"open","user":{"login":"o"}},
                {"number":9,"title":"REST-only","body":"c","state":"open","user":{"login":"o"}}
            ]"#
            .to_vec(),
        })])
        .unwrap();
    match response.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let mut names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(names, vec!["10", "11", "9"]);
        },
        other => panic!("expected deduped issue listing, got {other:?}"),
    }
}

#[test]
fn github_pr_list_projects_files() {
    use omnifs_wit::provider::types::{Callout, CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut response = harness.list("/octocat/Hello-World/pulls/open").unwrap();
    assert!(
        response.is_suspended(),
        "expected suspended response, got {response:?}"
    );
    let [Callout::Fetch(fetch)] = response.callouts() else {
        panic!("expected fetch callout, got {:?}", response.callouts());
    };
    assert!(
        fetch.url.ends_with(
            "/search/issues?q=repo:octocat/Hello-World+is:pr+state:open&sort=created&order=desc&per_page=100"
        ),
        "unexpected PR list URL: {}",
        fetch.url
    );

    response
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "total_count": 1,
                "items": [
                    {
                        "number":7,
                        "title":"PR title",
                        "body":"PR body",
                        "state":"open",
                        "user":{"login":"octocat"}
                    }
                ]
            }"#
            .to_vec(),
        })])
        .unwrap();

    assert!(
        response.callouts().is_empty(),
        "list terminal should carry no callouts, got {:?}",
        response.callouts()
    );
    match response.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let mut preloaded = project_paths(response.effects().unwrap());
            preloaded.sort_unstable();
            assert_eq!(
                preloaded,
                vec![
                    "/octocat/Hello-World/pulls/open/7",
                    "/octocat/Hello-World/pulls/open/7/body",
                    "/octocat/Hello-World/pulls/open/7/comments",
                    "/octocat/Hello-World/pulls/open/7/diff",
                    "/octocat/Hello-World/pulls/open/7/item.json",
                    "/octocat/Hello-World/pulls/open/7/item.md",
                    "/octocat/Hello-World/pulls/open/7/state",
                    "/octocat/Hello-World/pulls/open/7/title",
                    "/octocat/Hello-World/pulls/open/7/user",
                ]
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["7"]);
            let effects = response.effects().unwrap();
            let pull_body_path = "/octocat/Hello-World/pulls/open/7/body";
            let pull_md_path = "/octocat/Hello-World/pulls/open/7/item.md";
            assert!(
                project_file_is_deferred_full(effects, pull_body_path),
                "{:?}",
                effects.fs.iter().find(|write| write.path == pull_body_path)
            );
            assert!(
                project_file_is_deferred_full(effects, pull_md_path),
                "{:?}",
                effects.fs.iter().find(|write| write.path == pull_md_path)
            );
        },
        other => panic!("expected PR listing terminal, got {other:?}"),
    }
}

#[test]
fn github_action_run_list_projects_files() {
    use omnifs_wit::provider::types::{Callout, CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut response = harness.list("/octocat/Hello-World/actions/runs").unwrap();
    assert!(
        response.is_suspended(),
        "expected suspended response, got {response:?}"
    );
    let [Callout::Fetch(fetch)] = response.callouts() else {
        panic!("expected fetch callout, got {:?}", response.callouts());
    };
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30"),
        "unexpected action runs URL: {}",
        fetch.url
    );

    response
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "workflow_runs":[
                    {
                        "id":123,
                        "status":"completed",
                        "conclusion":"success"
                    }
                ]
            }"#
            .to_vec(),
        })])
        .unwrap();

    assert!(
        response.callouts().is_empty(),
        "list terminal should carry no callouts, got {:?}",
        response.callouts()
    );
    match response.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let project_paths = project_paths(response.effects().unwrap());
            assert_eq!(
                project_paths,
                vec![
                    "/octocat/Hello-World/actions/runs/123/status",
                    "/octocat/Hello-World/actions/runs/123/conclusion",
                ]
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["123"]);
        },
        other => panic!("expected action run listing terminal, got {other:?}"),
    }
}

#[test]
fn github_provider_action_run_lookup_validates_and_listing_validates() {
    use omnifs_wit::provider::types::{CalloutResult, Header, HttpResponse};

    let harness = github_harness();
    // lookup_child on a run id is structural (no fetch). The run id path is
    // an implicit literal-prefix dir (because `/{run_id}/log` is a registered
    // file route under it), so it resolves immediately as a directory.
    let lookup = harness
        .lookup("/octocat/Hello-World/actions/runs", "123")
        .unwrap();
    match lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::Entry(result)) => {
            let entry = &result.target;
            assert_eq!(entry.name, "123");
            assert!(matches!(entry.kind, EntryKind::Directory));
        },
        other => panic!("expected structural run dir lookup, got {other:?}"),
    }

    // Listing the run dir DOES fetch the run to resolve status/conclusion.
    let mut issued = harness
        .list("/octocat/Hello-World/actions/runs/123")
        .unwrap();
    assert!(
        issued.is_suspended(),
        "expected action run listing to dispatch validation, got {issued:?}"
    );

    issued
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"id":123,"status":"completed","conclusion":"success"}"#.to_vec(),
        })])
        .unwrap();

    match issued.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(result)) => {
            let names: Vec<&str> = result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"status"), "missing status in {names:?}");
            assert!(
                names.contains(&"conclusion"),
                "missing conclusion in {names:?}"
            );
            assert!(names.contains(&"log"), "missing log in {names:?}");
            assert!(
                !names.contains(&"comments"),
                "unexpected item comments route leaked into action run listing: {names:?}"
            );
            // status and conclusion are preloaded via preload_file on the dir listing.
            let preloaded = project_paths(issued.effects().unwrap());
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/123/status"),
                "missing status preload in {preloaded:?}"
            );
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/123/conclusion"),
                "missing conclusion preload in {preloaded:?}"
            );
        },
        other => panic!("expected list entries(123) after 200, got {other:?}"),
    }
}

#[test]
fn github_owner_listing_tracks_browsed_repos() {
    // State is empty; there is no active-path tracking. The test now just
    // verifies that (a) listing a repo dir triggers the repo_gate fetch, and
    // (b) listing an owner dir fetches the user/org profile then the repos.
    let harness = github_harness();
    // listing /{owner}/{repo} triggers repo_gate which fetches the repo API.
    let mut repo_listing = harness.list("/octocat/Hello-World").unwrap();
    let gate_fetch = repo_listing.expect_single_fetch();
    assert!(
        gate_fetch.url.ends_with("/repos/octocat/Hello-World"),
        "expected repo gate URL, got {}",
        gate_fetch.url
    );
    repo_listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"name":"Hello-World"}"#.to_vec(),
        })])
        .unwrap();
    assert!(
        matches!(repo_listing.result().unwrap(), OpResult::ListChildren(_)),
        "expected repo listing after gate, got {repo_listing:?}"
    );

    let mut owner_listing = harness.list("/octocat").unwrap();
    let user_fetch = owner_listing.expect_single_fetch();
    assert!(
        user_fetch.url.ends_with("/users/octocat"),
        "expected owner user lookup first, got {}",
        user_fetch.url
    );
    owner_listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"login":"octocat","type":"User"}"#.to_vec(),
        })])
        .unwrap();
    let repos_fetch = owner_listing.expect_single_fetch();
    assert!(
        repos_fetch
            .url
            .ends_with("/users/octocat/repos?per_page=100&sort=updated&page=1"),
        "expected owner repo listing fetch, got {}",
        repos_fetch.url
    );

    owner_listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"[{"name":"Hello-World"}]"#.to_vec(),
        })])
        .unwrap();
    match owner_listing.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(
                names.contains(&"Hello-World"),
                "expected Hello-World in owner listing, got {names:?}"
            );
        },
        other => panic!("expected owner listing, got {other:?}"),
    }
}

#[test]
fn github_root_and_owner_listings_ignore_unclassified_repo_paths() {
    // listing /{owner}/{repo} triggers repo_gate which fetches /repos/{owner}/{repo}.
    // Complete the fetch for each repo path before verifying owner/root listings.
    let harness = github_harness();

    for path in ["/zeta/zulu", "/open/source", "/alpha/app", "/openai/api"] {
        let mut step = harness.list(path).unwrap();
        assert!(
            step.is_suspended(),
            "expected repo gate fetch for {path}, got {step:?}"
        );
        step.resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: b"{}".to_vec(),
        })])
        .unwrap();
        assert!(
            matches!(step.result().unwrap(), OpResult::ListChildren(_)),
            "expected repo listing for {path}, got {step:?}"
        );
    }

    let root_listing = harness.list("/").unwrap();
    match root_listing.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.is_empty(), "unexpected root names: {names:?}");
        },
        other => panic!("expected root listing, got {other:?}"),
    }

    let mut owner_listing = harness.list("/open").unwrap();
    let user_fetch = owner_listing.expect_single_fetch();
    assert!(
        user_fetch.url.ends_with("/users/open"),
        "expected owner user lookup first, got {}",
        user_fetch.url
    );
    owner_listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"login":"open","type":"User"}"#.to_vec(),
        })])
        .unwrap();
    let repos_fetch = owner_listing.expect_single_fetch();
    assert!(
        repos_fetch
            .url
            .ends_with("/users/open/repos?per_page=100&sort=updated&page=1"),
        "expected owner repo listing fetch, got {}",
        repos_fetch.url
    );

    owner_listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: b"[]".to_vec(),
        })])
        .unwrap();
    match owner_listing.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(
                names.is_empty(),
                "unexpected owner names after uncached repo traversal: {names:?}"
            );
        },
        other => panic!("expected owner listing, got {other:?}"),
    }
}

#[tokio::test]
async fn github_repo_tree_lists_looks_up_and_reads_from_git_cache() {
    let harness = github_harness();
    seed_github_repo_cache(&harness, "octocat", "Hello-World");

    let repo_listing = harness
        .runtime
        .namespace()
        .list_children("/octocat/Hello-World/repo", None, None, None)
        .await
        .unwrap();
    match repo_listing {
        ListChildrenResult::Subtree(tree_ref) => {
            let real_root = harness
                .runtime
                .resolve_tree_ref(tree_ref)
                .expect("missing disowned repo tree");
            assert!(real_root.join("README.md").is_file());
            assert!(real_root.join("src").is_dir());
        },
        other => {
            panic!("expected repo tree listing, got {other:?}")
        },
    }

    let repo_child = harness
        .runtime
        .namespace()
        .lookup_child("/octocat/Hello-World", "repo", None)
        .await
        .unwrap();
    match repo_child {
        LookupOutcome::Subtree(tree_ref) => {
            let real_root = harness
                .runtime
                .resolve_tree_ref(tree_ref)
                .expect("missing disowned repo tree");
            assert!(real_root.join("README.md").is_file());
            assert!(real_root.join("src").is_dir());
            assert_eq!(
                std::fs::read(real_root.join("README.md")).unwrap(),
                b"Hello from cache\n"
            );
            assert!(real_root.join("src/main.rs").is_file());
        },
        other => panic!("expected repo child lookup, got {other:?}"),
    }
}

#[test]
fn github_provider_missing_item_resources_validate_on_lookup() {
    use omnifs_wit::provider::types::{CalloutResult, ErrorKind, Header, HttpResponse};

    // Issue item dirs resolve structurally; existence is validated on read.
    let harness = github_harness();

    let lookup = harness
        .lookup("/octocat/Hello-World/issues/open", "999999999")
        .unwrap();
    match lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::Entry(result)) => {
            assert_eq!(result.target.name, "999999999");
            assert!(
                matches!(result.target.kind, EntryKind::Directory),
                "issue anchor should resolve as directory, got {:?}",
                result.target.kind
            );
        },
        other => panic!("expected immediate Dir entry for issue anchor, got {other:?}"),
    }

    let diff_lookup = harness
        .lookup("/octocat/Hello-World/issues/open/999999999", "diff")
        .unwrap();
    match diff_lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::NotFound(_)) => {},
        other => panic!("expected issue diff lookup to be NotFound, got {other:?}"),
    }

    // Reading a structural file under the anchor triggers a fetch; a 404 from
    // GitHub propagates as NotFound. Use comments/1 (structural handler).
    let mut issued = harness
        .read("/octocat/Hello-World/issues/open/999999999/comments/1")
        .unwrap();
    let fetch = issued.expect_single_fetch();
    assert!(
        fetch
            .url
            .contains("/repos/octocat/Hello-World/issues/999999999/comments"),
        "unexpected issue comments fetch URL: {}",
        fetch.url
    );

    issued
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: Vec::<Header>::new(),
            body: b"{\"message\":\"Not Found\"}".to_vec(),
        })])
        .unwrap();

    match issued.result().unwrap() {
        OpResult::Error(error) => {
            assert_eq!(error.kind, ErrorKind::NotFound);
        },
        other => panic!("expected ProviderErr(NotFound) on 404 read, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_pr_lookup_validates_and_exposes_diff() {
    use omnifs_wit::provider::types::{
        BlobFetched, ByteSource, Callout, CalloutError, CalloutResult, ErrorKind, HttpResponse,
    };

    let harness = github_harness();

    let lookup = harness
        .lookup("/octocat/Hello-World/pulls/open", "7")
        .unwrap();
    match lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::Entry(result)) => {
            assert_eq!(result.target.name, "7");
            assert!(matches!(result.target.kind, EntryKind::Directory));
        },
        other => panic!("expected structural PR dir lookup, got {other:?}"),
    }

    let mut listing = harness.list("/octocat/Hello-World/pulls/open/7").unwrap();
    let listing_fetch = listing.expect_single_fetch();
    assert!(
        listing_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7"),
        "unexpected PR listing fetch URL: {}",
        listing_fetch.url
    );

    listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "number": 7,
                "title": "Fix the thing",
                "body": "PR body",
                "state": "open",
                "user": {"login": "octocat"}
            }"#
            .to_vec(),
        })])
        .unwrap();
    match listing.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(result)) => {
            let mut names: Vec<&str> = result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec![
                    "body",
                    "comments",
                    "diff",
                    "item.json",
                    "item.md",
                    "state",
                    "title",
                    "user",
                ]
            );
        },
        other => panic!("expected PR dir listing, got {other:?}"),
    }

    let mut body = harness
        .read("/octocat/Hello-World/pulls/open/7/body")
        .unwrap();
    let body_fetch = body.expect_single_fetch();
    assert!(
        body_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7"),
        "unexpected PR body fetch URL: {}",
        body_fetch.url
    );

    body.resume(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: br#"{
                "number": 7,
                "title": "Fix the thing",
                "body": "PR body",
                "state": "open",
                "user": {"login": "octocat"}
            }"#
        .to_vec(),
    })])
    .unwrap();
    match body.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            match &file.bytes {
                ByteSource::Inline(bytes) => assert_eq!(bytes.as_slice(), b"PR body"),
                other => panic!("expected inline body field, got {other:?}"),
            }
            assert!(
                !body.effects().unwrap().canonical.is_empty(),
                "field read should store canonical bytes"
            );
        },
        other => panic!("expected PR body field after read, got {other:?}"),
    }

    let mut diff = harness
        .read("/octocat/Hello-World/pulls/open/7/diff")
        .unwrap();
    let diff_fetch = match diff.callouts() {
        [Callout::FetchBlob(request)] => request,
        other => panic!("expected fetch-blob callout, got {other:?}"),
    };
    assert!(
        diff_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7"),
        "unexpected PR diff fetch URL: {}",
        diff_fetch.url
    );
    assert!(
        diff_fetch
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("accept")
                && h.value == "application/vnd.github.diff"),
        "expected diff Accept header, got {:?}",
        diff_fetch.headers
    );

    diff.resume(vec![CalloutResult::BlobFetched(BlobFetched {
        blob: 1,
        size: 25,
        content_type: Some("application/octet-stream".to_string()),
        etag: None,
        status: 200,
        response_headers: Vec::new(),
    })])
    .unwrap();

    match diff.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => match &file.bytes {
            ByteSource::Blob(blob) => assert_eq!(*blob, 1),
            other => panic!("expected blob-backed diff, got {other:?}"),
        },
        other => panic!("expected PR diff file after read, got {other:?}"),
    }

    let mut retry = harness
        .read("/octocat/Hello-World/pulls/open/7/diff")
        .unwrap();
    assert!(
        retry.is_suspended(),
        "expected PR diff reread to refetch, got {retry:?}"
    );
    retry
        .resume(vec![CalloutResult::CalloutError(CalloutError {
            kind: ErrorKind::Network,
            message: "network down".to_string(),
            retryable: true,
        })])
        .unwrap();
    match retry.result().unwrap() {
        OpResult::Error(error) => {
            assert_eq!(error.kind, ErrorKind::Network);
        },
        other => panic!("expected Network error on refetch, got {other:?}"),
    }
}

#[test]
fn github_projected_resource_reads_return_all_fetched_siblings() {
    // per-field files (title, body, state, user, status, conclusion) are
    // descoped; sibling preloads happen at listing time via run_dir and runs_list.
    //
    // This test verifies that listing a specific run directory fetches the run
    // and preloads its status and conclusion alongside the listing (so the host
    // can cache them without a second round-trip).
    use omnifs_wit::provider::types::{CalloutResult, HttpResponse};

    let harness = github_harness();

    // list_children on a run dir fetches the run and preloads status/conclusion.
    let mut run_listed = harness
        .list("/octocat/Hello-World/actions/runs/123")
        .unwrap();
    let run_fetch = run_listed.expect_single_fetch();
    assert!(
        run_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs/123"),
        "unexpected action run dir URL: {}",
        run_fetch.url
    );

    run_listed
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"id":123,"status":"completed","conclusion":"success"}"#.to_vec(),
        })])
        .unwrap();
    match run_listed.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(_)) => {
            let effects = run_listed.effects().unwrap();
            let preloaded = project_paths(effects);
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/123/status"),
                "missing status preload in {preloaded:?}"
            );
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/123/conclusion"),
                "missing conclusion preload in {preloaded:?}"
            );
            assert_eq!(
                project_file_stability(effects, "/octocat/Hello-World/actions/runs/123/conclusion"),
                Some(Stability::Dynamic),
                "conclusion should be Dynamic"
            );
        },
        other => panic!("expected run dir listing with preloads, got {other:?}"),
    }

    // list_children on the runs directory also preloads at the full mount-relative path.
    let mut index_listed = harness.list("/octocat/Hello-World/actions/runs").unwrap();
    let index_fetch = index_listed.expect_single_fetch();
    assert!(
        index_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30"),
        "unexpected runs listing URL: {}",
        index_fetch.url
    );

    index_listed
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"workflow_runs":[{"id":42,"status":"in_progress","conclusion":null}]}"#
                .to_vec(),
        })])
        .unwrap();
    match index_listed.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(_)) => {
            let preloaded = project_paths(index_listed.effects().unwrap());
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/42/status"),
                "missing run 42 status preload in {preloaded:?}"
            );
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/42/conclusion"),
                "missing run 42 conclusion preload in {preloaded:?}"
            );
        },
        other => panic!("expected runs listing with preloads, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_resource_reads_do_not_fall_back_to_provider_cache() {
    use omnifs_wit::provider::types::{
        CalloutError, CalloutResult, ErrorKind, Header, HttpResponse,
    };

    struct Case {
        name: &'static str,
        path: &'static str,
        ok_headers: Vec<Header>,
        ok_body: &'static [u8],
        expected_content: &'static [u8],
    }

    // PR diff is covered separately by `github_pr_lookup_validates_and_exposes_diff`
    // because it dispatches a fetch-blob callout (and returns a blob-backed
    // ReadFileBytes) rather than an inline HttpResponse.
    //
    // Object field files are covered by item-specific tests. These structural
    // file routes must still work from a cold read path without relying on a
    // previous directory listing to preload content into the host cache.
    let cases = [
        Case {
            name: "issue comment",
            path: "/octocat/Hello-World/issues/open/1/comments/1",
            ok_headers: vec![Header {
                name: "etag".to_string(),
                value: "\"comment-1\"".to_string(),
            }],
            ok_body: br#"[{"user":{"login":"octocat"},"body":"A comment"}]"#,
            expected_content: b"octocat:\nA comment\n",
        },
        Case {
            name: "action status",
            path: "/octocat/Hello-World/actions/runs/99/status",
            ok_headers: Vec::new(),
            ok_body: br#"{"id":99,"status":"completed","conclusion":"success"}"#,
            expected_content: b"completed",
        },
        Case {
            name: "action conclusion",
            path: "/octocat/Hello-World/actions/runs/99/conclusion",
            ok_headers: Vec::new(),
            ok_body: br#"{"id":99,"status":"completed","conclusion":"success"}"#,
            expected_content: b"success",
        },
        Case {
            name: "action log",
            path: "/octocat/Hello-World/actions/runs/99/log",
            ok_headers: Vec::new(),
            // run_log fetches the zip archive; empty body → pass-through raw bytes.
            ok_body: b"",
            expected_content: b"",
        },
    ];

    let harness = github_harness();
    for case in &cases {
        let mut first = harness.read(case.path).unwrap();
        assert!(
            first.is_suspended(),
            "{name}: expected fetch callout on first read, got {first:?}",
            name = case.name
        );
        first
            .resume(vec![CalloutResult::HttpResponse(HttpResponse {
                status: 200,
                headers: case.ok_headers.clone(),
                body: case.ok_body.to_vec(),
            })])
            .unwrap();
        match first.result().unwrap() {
            OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
                if !case.expected_content.is_empty() {
                    assert_eq!(
                        omnifs_itest::expect_inline(file),
                        case.expected_content,
                        "{name}: unexpected cached content",
                        name = case.name
                    );
                }
            },
            other => panic!("{}: expected ReadFile result, got {other:?}", case.name),
        }

        let mut second = harness.read(case.path).unwrap();
        assert!(
            second.is_suspended(),
            "{name}: expected fetch callout on second read (no provider cache), got {second:?}",
            name = case.name
        );
        second
            .resume(vec![CalloutResult::CalloutError(CalloutError {
                kind: ErrorKind::Network,
                message: "network down".to_string(),
                retryable: true,
            })])
            .unwrap();
        match second.result().unwrap() {
            OpResult::Error(err) => {
                assert_eq!(
                    err.kind,
                    ErrorKind::Network,
                    "{}: wrong error kind",
                    case.name
                );
            },
            other => panic!(
                "{}: expected Network error on second read, got {other:?}",
                case.name
            ),
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_comment_routes_refetch_and_reject_zero_index() {
    use omnifs_wit::provider::types::{Callout, CalloutError, CalloutResult, ErrorKind};

    fn ok_body(body: &[u8]) -> Vec<CalloutResult> {
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
        })]
    }

    fn network_error() -> Vec<CalloutResult> {
        vec![CalloutResult::CalloutError(CalloutError {
            kind: ErrorKind::Network,
            message: "network down".to_string(),
            retryable: true,
        })]
    }

    fn expect_network_error_on_refetch(op: &mut omnifs_host::TestOp<'_>) {
        assert!(
            op.is_suspended(),
            "expected fetch callout on refetch, got {op:?}"
        );
        op.resume(network_error()).unwrap();
        match op.result().unwrap() {
            OpResult::Error(error) => {
                assert_eq!(error.kind, ErrorKind::Network);
            },
            other => panic!("expected Network error on refetch, got {other:?}"),
        }
    }

    fn expect_not_found(response: &omnifs_host::TestOp<'_>) {
        match response.result().unwrap() {
            OpResult::Error(error) => {
                assert_eq!(error.kind, ErrorKind::NotFound);
            },
            other => panic!("expected NotFound error, got {other:?}"),
        }
    }

    fn expect_fetch_url(response: &omnifs_host::TestOp<'_>) -> String {
        let [Callout::Fetch(request)] = response.callouts() else {
            panic!(
                "expected single fetch callout, got {:?}",
                response.callouts()
            );
        };
        request.url.clone()
    }

    let harness = github_harness();
    // filter segment dropped; path is /{owner}/{repo}/issues/{number}/comments.
    // Issue comments surface through list_children.
    let issue_list_path = "/octocat/Hello-World/issues/open/1/comments";
    let mut issue_first = harness.list(issue_list_path).unwrap();
    assert!(issue_first.is_suspended());
    issue_first
        .resume(ok_body(
            br#"[{"user":{"login":"octocat"},"body":"first issue comment"}]"#,
        ))
        .unwrap();
    match issue_first.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["1"]);
            let EntryKind::File(file) = &listing.entries[0].kind else {
                panic!("expected comment entry to be a file");
            };
            // comment listing entries use the default Stable stability;
            // the Dynamic stability is carried on the read result from comment_read.
            assert_eq!(file.attrs.stability, Stability::Stable);
        },
        other => panic!("expected issue comment listing, got {other:?}"),
    }
    let mut issue_refetch = harness.list(issue_list_path).unwrap();
    expect_network_error_on_refetch(&mut issue_refetch);
    let issue_zero = harness
        .read("/octocat/Hello-World/issues/open/1/comments/0")
        .unwrap();
    expect_not_found(&issue_zero);

    let mut issue_page_two = harness
        .read("/octocat/Hello-World/issues/open/1/comments/101")
        .unwrap();
    let issue_page_two_url = expect_fetch_url(&issue_page_two);
    assert!(
        issue_page_two_url.contains("/issues/1/comments?per_page=100&page=2"),
        "expected second-page issue comment fetch, got {issue_page_two_url}"
    );
    issue_page_two
        .resume(ok_body(
            br#"[{"user":{"login":"octocat"},"body":"page two issue comment"}]"#,
        ))
        .unwrap();
    match issue_page_two.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            assert_eq!(
                omnifs_itest::expect_inline(file),
                b"octocat:\npage two issue comment\n"
            );
        },
        other => panic!("expected issue comment page-two content, got {other:?}"),
    }

    // PR comments surface through read_file at a specific index.
    // filter segment dropped; path is /{owner}/{repo}/pulls/{number}/comments/{idx}.
    let pr_read_path = "/octocat/Hello-World/pulls/open/7/comments/1";
    let mut pr_first = harness.read(pr_read_path).unwrap();
    assert!(pr_first.is_suspended());
    pr_first
        .resume(ok_body(
            br#"[{"user":{"login":"hubot"},"body":"first pr comment"}]"#,
        ))
        .unwrap();
    match pr_first.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            assert_eq!(
                omnifs_itest::expect_inline(file),
                b"hubot:\nfirst pr comment\n"
            );
        },
        other => panic!("expected PR comment content, got {other:?}"),
    }
    let mut pr_refetch = harness.read(pr_read_path).unwrap();
    expect_network_error_on_refetch(&mut pr_refetch);
    let pr_zero = harness
        .read("/octocat/Hello-World/pulls/open/7/comments/0")
        .unwrap();
    expect_not_found(&pr_zero);

    let mut pr_page_two = harness
        .read("/octocat/Hello-World/pulls/open/7/comments/101")
        .unwrap();
    let pr_page_two_url = expect_fetch_url(&pr_page_two);
    assert!(
        pr_page_two_url.contains("/issues/7/comments?per_page=100&page=2"),
        "expected second-page PR comment fetch, got {pr_page_two_url}"
    );
    pr_page_two
        .resume(ok_body(
            br#"[{"user":{"login":"hubot"},"body":"page two pr comment"}]"#,
        ))
        .unwrap();
    match pr_page_two.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            assert_eq!(
                omnifs_itest::expect_inline(file),
                b"hubot:\npage two pr comment\n"
            );
        },
        other => panic!("expected PR comment page-two content, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_paginates_issue_and_pr_results_in_parallel() {
    use omnifs_wit::provider::types::{Callout, CalloutResult, Header, HttpResponse};

    fn page_items(first_number: u64) -> String {
        (first_number..first_number + 100)
            .map(|number| {
                format!(
                    r#"{{
                        "number": {number},
                        "title": "page item",
                        "body": "text",
                        "state": "open",
                        "user": {{"login": "octocat"}}
                    }}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    fn search_page(total_count: u64, first_number: u64) -> CalloutResult {
        let body = format!(
            r#"{{"total_count":{total_count},"items":[{}]}}"#,
            page_items(first_number)
        );
        CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.into_bytes(),
        })
    }

    fn rest_page(first_number: u64) -> CalloutResult {
        let body = format!("[{}]", page_items(first_number));
        CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"page\"".to_string(),
            }],
            body: body.into_bytes(),
        })
    }

    fn assert_page_fetches(callouts: &[Callout], expected: std::ops::RangeInclusive<u64>) {
        let expected_pages: Vec<u64> = expected.collect();
        assert_eq!(callouts.len(), expected_pages.len());
        for (callout, page) in callouts.iter().zip(expected_pages) {
            let page_suffix = format!("&page={page}");
            let page_middle = format!("{page_suffix}&");
            assert!(
                matches!(callout, Callout::Fetch(req)
                    if req.url.ends_with(&page_suffix) || req.url.contains(&page_middle)),
                "expected page {page} fetch, got {callout:?}"
            );
        }
    }

    let harness = github_harness();
    // filter segment dropped; path is /{owner}/{repo}/issues (always open).
    let mut issues = harness.list("/octocat/Hello-World/issues/open").unwrap();
    let first_issue_page = issues.expect_single_fetch();
    assert!(
        first_issue_page.url.ends_with(
            "/search/issues?q=repo:octocat/Hello-World+is:issue+state:open&sort=created&order=desc&per_page=100"
        ),
        "unexpected issue list URL: {}",
        first_issue_page.url
    );
    issues.resume(vec![search_page(1500, 1)]).unwrap();
    assert!(
        issues.is_suspended(),
        "expected parallel issue page fetches, got {issues:?}"
    );
    assert_page_fetches(issues.callouts(), 2..=10);
    let issue_pages = (2..=10).map(|page| rest_page(page * 100)).collect();
    issues.resume(issue_pages).unwrap();
    match issues.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"1"));
            assert!(names.contains(&"200"));
            assert!(names.contains(&"1099"));
            assert!(
                !listing.exhaustive,
                "issue listing should remain partial after hitting the search-API page cap"
            );
        },
        other => panic!("expected paginated issue listing, got {other:?}"),
    }
    // filter segment dropped; path is /{owner}/{repo}/pulls (always open).
    let mut pulls = harness.list("/octocat/Hello-World/pulls/open").unwrap();
    let first_pr_page = pulls.expect_single_fetch();
    assert!(first_pr_page.url.ends_with(
        "/search/issues?q=repo:octocat/Hello-World+is:pr+state:open&sort=created&order=desc&per_page=100"
    ), "unexpected PR list URL: {}", first_pr_page.url);
    pulls.resume(vec![search_page(1500, 7)]).unwrap();
    assert!(
        pulls.is_suspended(),
        "expected parallel PR page fetches, got {pulls:?}"
    );
    assert_page_fetches(pulls.callouts(), 2..=10);
    let pr_pages = (2..=10).map(|page| rest_page(page * 100 + 7)).collect();
    pulls.resume(pr_pages).unwrap();
    match pulls.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"7"));
            assert!(names.contains(&"207"));
            assert!(names.contains(&"1106"));
            assert!(
                !listing.exhaustive,
                "PR listing should remain partial after hitting the search-API page cap"
            );
        },
        other => panic!("expected paginated PR listing, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_lookup_owner_validates_and_owner_listing_classifies_with_org_fallback() {
    use omnifs_wit::provider::types::{CalloutResult, Header, HttpResponse};

    let harness = github_harness();
    // lookup_child on an owner segment resolves structurally without a fetch.
    // The owner is an implicit prefix dir (/{owner}/{repo} routes exist under it).
    let lookup = harness.lookup("/", "openai").unwrap();
    match lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::Entry(result)) => {
            assert_eq!(result.target.name, "openai");
            assert!(
                matches!(result.target.kind, EntryKind::Directory),
                "owner anchor should be a directory"
            );
        },
        other => panic!("expected immediate Dir entry for owner, got {other:?}"),
    }

    // list_children does classify the owner (user vs org) to pick the right
    // repos endpoint. Test the org-fallback path.
    let mut listing = harness.list("/openai").unwrap();
    let first = listing.expect_single_fetch();
    assert!(
        first.url.ends_with("/users/openai"),
        "expected user profile lookup first, got {}",
        first.url
    );

    listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"miss\"".to_string(),
            }],
            body: Vec::new(),
        })])
        .unwrap();
    let second = listing.expect_single_fetch();
    assert!(
        second.url.ends_with("/orgs/openai"),
        "expected org profile fallback, got {}",
        second.url
    );

    listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "login": "openai",
                "public_repos": 42
            }"#
            .to_vec(),
        })])
        .unwrap();
    let repos_fetch = listing.expect_single_fetch();
    assert!(
        repos_fetch
            .url
            .ends_with("/orgs/openai/repos?per_page=100&sort=updated&page=1"),
        "expected repo listing fetch after owner classification, got {}",
        repos_fetch.url
    );

    listing
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[{"name":"api"}]"#.to_vec(),
        })])
        .unwrap();
    match listing.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(result)) => {
            let names: Vec<&str> = result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(
                names.contains(&"api"),
                "expected api in owner listing, got {names:?}"
            );
        },
        other => panic!("expected owner listing result, got {other:?}"),
    }

    // Root is not enumerable; should always return empty, regardless
    // of which owners have been resolved in prior calls.
    let root_listing = harness.list("/").unwrap();
    match root_listing.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            assert!(
                listing.entries.is_empty(),
                "root should be empty, got {:?}",
                listing.entries
            );
        },
        other => panic!("expected empty root listing, got {other:?}"),
    }
}

#[test]
fn github_provider_polls_events_and_invalidates_caches() {
    // the event poller and active-path tracking are removed; State is empty.
    // This test verifies that the issue read path still works with issue.json,
    // and that timer ticks are no-ops (no callouts, no invalidations).
    let harness = github_harness();
    // per-field files dropped; use a structural file route (issue comment).
    let issue_path = "/octocat/Hello-World/issues/open/1/comments/1";

    let mut issue_cached = harness.read(issue_path).unwrap();
    let issue_fetch = issue_cached.expect_single_fetch();
    assert!(
        issue_fetch
            .url
            .contains("/repos/octocat/Hello-World/issues/1/comments"),
        "unexpected issue comment fetch URL: {}",
        issue_fetch.url
    );
    issue_cached
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"comment-1\"".to_string(),
            }],
            body: br#"[{"user":{"login":"octocat"},"body":"A comment"}]"#.to_vec(),
        })])
        .unwrap();
    match issue_cached.result().unwrap() {
        OpResult::ReadFile(_) => {},
        other => panic!("expected issue comment ReadFile result, got {other:?}"),
    }
    // the event poller (events_etags, active_paths, timer handler) is
    // removed. State is empty. TimerTick returns immediately with no callouts
    // and no cache invalidations.
    let first_tick = harness.timer_tick().unwrap();
    assert!(
        first_tick.callouts().is_empty(),
        "timer tick should not issue callouts, got {:?}",
        first_tick.callouts()
    );
    match first_tick.result().unwrap() {
        OpResult::OnEvent => {
            assert!(
                first_tick.effects().unwrap().invalidations.is_empty(),
                "timer tick should not invalidate anything, got {:?}",
                first_tick.effects().unwrap().invalidations
            );
        },
        other => panic!("expected OnEvent terminal from timer tick, got {other:?}"),
    }

    // A second tick is equally a no-op.
    let second_tick = harness.timer_tick().unwrap();
    assert!(
        matches!(second_tick.result().unwrap(), OpResult::OnEvent),
        "expected second timer tick OnEvent terminal, got {second_tick:?}"
    );
    assert!(
        second_tick.callouts().is_empty(),
        "second timer tick should not issue callouts, got {:?}",
        second_tick.callouts()
    );
}

#[test]
fn github_provider_list_routes_preserve_typed_http_errors() {
    use omnifs_wit::provider::types::{CalloutResult, ErrorKind, Header, HttpResponse};

    fn denied_page() -> Vec<CalloutResult> {
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 403,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"denied\"".to_string(),
            }],
            body: br#"{"message":"forbidden"}"#.to_vec(),
        })]
    }

    fn expect_denied(response: &omnifs_host::TestOp<'_>) {
        match response.result().unwrap() {
            OpResult::Error(error) => assert_eq!(error.kind, ErrorKind::Denied),
            other => panic!("expected provider error result, got {other:?}"),
        }
    }

    let cases = [
        (
            "issues",
            "/octocat/Hello-World/issues/open",
            "/search/issues?q=repo:octocat/Hello-World+is:issue+state:open&sort=created&order=desc&per_page=100",
        ),
        (
            "pulls",
            "/octocat/Hello-World/pulls/open",
            "/search/issues?q=repo:octocat/Hello-World+is:pr+state:open&sort=created&order=desc&per_page=100",
        ),
        (
            "actions",
            "/octocat/Hello-World/actions/runs",
            "/repos/octocat/Hello-World/actions/runs?per_page=30",
        ),
    ];

    let harness = github_harness();
    for (kind, path, suffix) in cases {
        let mut op = harness.list(path).unwrap();
        let fetch = op.expect_single_fetch();
        assert!(
            fetch.url.ends_with(suffix),
            "{kind}: unexpected URL {}",
            fetch.url
        );
        op.resume(denied_page()).unwrap();
        expect_denied(&op);
    }
}

/// Invariant #1: `issues/open/42` and `issues/all/42` share one object load.
#[tokio::test]
async fn open_then_all_one_load() {
    use omnifs_wit::provider::types::{
        ByteSource, Callout, CalloutResult, HttpResponse, ReadFileOutcome,
    };

    let issue_json = br#"{
        "number": 42,
        "title": "Issue forty-two",
        "body": "Body text",
        "state": "open",
        "user": {"login": "octocat"}
    }"#;

    let open_title = "/octocat/Hello-World/issues/open/42/title";
    let all_title = "/octocat/Hello-World/issues/all/42/title";

    let harness = github_harness();
    let mut first = harness.read(open_title).unwrap();
    let [Callout::Fetch(fetch)] = first.callouts() else {
        panic!("expected issue fetch on first read, got {first:?}");
    };
    assert!(
        fetch.url.ends_with("/repos/octocat/Hello-World/issues/42"),
        "unexpected issue URL: {}",
        fetch.url
    );

    first
        .resume(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: issue_json.to_vec(),
        })])
        .unwrap();
    assert!(
        matches!(
            first.result().unwrap(),
            OpResult::ReadFile(ReadFileOutcome::Found(_))
        ),
        "expected read terminal, got {first:?}"
    );
    let effects = first.effects().unwrap();
    assert!(
        !effects.canonical.is_empty(),
        "issue read must emit canonical-store"
    );
    let leaves: Vec<&str> = effects.canonical[0]
        .view_leaves
        .iter()
        .map(String::as_str)
        .collect();
    assert!(
        leaves.iter().any(|p| p.contains("/issues/open/42")),
        "missing open alias in {leaves:?}"
    );
    assert!(
        leaves.iter().any(|p| p.contains("/issues/all/42")),
        "missing all alias in {leaves:?}"
    );

    let harness = github_harness();
    harness
        .runtime
        .apply_effects_for_test(effects, harness.runtime.current_generation());
    assert!(harness.runtime.cached_canonical_for(open_title).is_some());
    assert!(harness.runtime.cached_canonical_for(all_title).is_some());

    let warm = harness
        .runtime
        .namespace()
        .read_file(
            all_title,
            OmnifsPath::parse(all_title)
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap();
    match warm.bytes {
        ByteSource::Inline(bytes) => assert_eq!(bytes, b"Issue forty-two"),
        other => panic!("expected inline title on warm read, got {other:?}"),
    }
}

/// Invariant #3: `item.json` bytes equal the single-item GET body verbatim.
#[test]
fn item_json_byte_equals_single_get() {
    use omnifs_wit::provider::types::{
        ByteSource, Callout, CalloutResult, HttpResponse, ReadFileOutcome,
    };

    let issue_json = br#"{
        "number": 42,
        "title": "Issue forty-two",
        "body": "Body text",
        "state": "open",
        "user": {"login": "octocat"}
    }"#;

    let item_path = "/octocat/Hello-World/issues/all/42/item.json";

    let harness = github_harness();
    let mut step = harness.read(item_path).unwrap();
    let [Callout::Fetch(fetch)] = step.callouts() else {
        panic!("expected issue fetch, got {step:?}");
    };
    assert!(
        fetch.url.ends_with("/repos/octocat/Hello-World/issues/42"),
        "unexpected URL: {}",
        fetch.url
    );

    step.resume(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: issue_json.to_vec(),
    })])
    .unwrap();
    let OpResult::ReadFile(ReadFileOutcome::Found(file)) = step.result().unwrap() else {
        panic!("expected read terminal, got {step:?}");
    };
    assert_eq!(step.effects().unwrap().canonical[0].bytes, issue_json);
    match &file.bytes {
        ByteSource::Canonical => {},
        ByteSource::Inline(bytes) => assert_eq!(bytes.as_slice(), issue_json),
        other => panic!("expected canonical or inline item.json bytes, got {other:?}"),
    }
}
