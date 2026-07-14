#![cfg(not(target_os = "wasi"))]

mod support;

use omnifs_engine::test_support::{LookupOutcome, NamespaceListOutcome, ReadBytes};
use omnifs_itest::parse_path;
use omnifs_wit::provider::types::{
    CalloutResult, EntryKind, Header, HttpResponse, ListChildrenResult, LookupChildResult,
    OpResult, ReadFileOutcome, Stability,
};
use support::{
    TestOpExt, github_harness, project_file_inline_bytes, project_file_stability, project_paths,
    seed_github_repo_cache,
};

#[test]
fn github_root_readme_snapshot() {
    let harness = github_harness();
    let readme = harness
        .read("/README.md")
        .unwrap()
        .into_read_file()
        .unwrap();
    let actual = String::from_utf8(omnifs_itest::expect_inline(&readme).to_vec()).unwrap();
    let expected = include_str!("snapshots/root-readme.md").trim_end();
    if actual.trim_end() != expected {
        eprintln!("{actual}");
    }
    assert_eq!(actual.trim_end(), expected);
}

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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        response.is_waiting_for_callouts(),
        "expected callout wait, got {response:?}"
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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

    // The issue listing projects each row as a `CollectionEntry::derived`: the
    // eager derived leaves (title/state/user) that the lossy list row can fill
    // are projected as inline files under the issue anchor. The item canonical
    // (item.json/item.md/body) is NOT seeded from the list row, because the row
    // cannot reproduce the single-item GET byte-for-byte; those load on first
    // read from the standalone object.
    assert!(
        response.callouts().is_empty(),
        "list terminal should carry no callouts, got {:?}",
        response.callouts()
    );
    match response.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let effects = response.effects().unwrap();
            let mut preloaded = project_paths(effects);
            preloaded.sort_unstable();
            assert_eq!(
                preloaded,
                vec![
                    "/octocat/Hello-World/issues/open/7/state",
                    "/octocat/Hello-World/issues/open/7/title",
                    "/octocat/Hello-World/issues/open/7/user",
                ],
                "list should project only the derived leaves, not seed the item"
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["7"]);
            // Derived leaves carry the cheap list-row fields inline.
            assert_eq!(
                project_file_inline_bytes(effects, "/octocat/Hello-World/issues/open/7/title"),
                Some(b"Issue title".as_slice()),
            );
            assert_eq!(
                project_file_inline_bytes(effects, "/octocat/Hello-World/issues/open/7/state"),
                Some(b"open".as_slice()),
            );
            assert_eq!(
                project_file_inline_bytes(effects, "/octocat/Hello-World/issues/open/7/user"),
                Some(b"".as_slice()),
            );
            // The single-item canonical (item.json) is NOT seeded by the list.
            assert!(
                effects.canonical.is_empty(),
                "list must not seed item canonical, got {:?}",
                effects.canonical
            );
        },
        other => panic!("expected issue listing terminal, got {other:?}"),
    }
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        response.is_waiting_for_callouts(),
        "expected callout wait, got {response:?}"
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
    // Like issues, a PR row projects only its eager derived leaves
    // (title/state/user). The item canonical (item.json/item.md/body) and the
    // diff blob are object faces that load on first read; the list does not
    // seed them.
    match response.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let effects = response.effects().unwrap();
            let mut preloaded = project_paths(effects);
            preloaded.sort_unstable();
            assert_eq!(
                preloaded,
                vec![
                    "/octocat/Hello-World/pulls/open/7/state",
                    "/octocat/Hello-World/pulls/open/7/title",
                    "/octocat/Hello-World/pulls/open/7/user",
                ],
                "PR list should project only the derived leaves, not seed the item or diff"
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["7"]);
            assert_eq!(
                project_file_inline_bytes(effects, "/octocat/Hello-World/pulls/open/7/title"),
                Some(b"PR title".as_slice()),
            );
            assert_eq!(
                project_file_inline_bytes(effects, "/octocat/Hello-World/pulls/open/7/state"),
                Some(b"open".as_slice()),
            );
            assert_eq!(
                project_file_inline_bytes(effects, "/octocat/Hello-World/pulls/open/7/user"),
                Some(b"octocat".as_slice()),
            );
            assert!(
                effects.canonical.is_empty(),
                "PR list must not seed item canonical, got {:?}",
                effects.canonical
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
        response.is_waiting_for_callouts(),
        "expected callout wait, got {response:?}"
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        issued.is_waiting_for_callouts(),
        "expected action run listing to dispatch validation, got {issued:?}"
    );

    issued
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
    // The owner is an OBJECT at `/{owner}`: listing it MERGES the owner's own
    // faces (owner.json canonical, profile.md representation) with the repo
    // names yielded by the `Owner::repos` anchor collection. The repo names ARE
    // the child `Repo` anchors (`/{owner}/{repo}`). This test pins that merge:
    // a browsed repo surfaces under the owner alongside the owner's own faces.
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"name":"Hello-World"}"#.to_vec(),
        })])
        .unwrap();
    assert!(
        matches!(repo_listing.result().unwrap(), OpResult::ListChildren(_)),
        "expected repo listing after gate, got {repo_listing:?}"
    );

    // Listing the owner anchor fetches (1) the owner profile to load its own
    // canonical (owner.json/profile.md faces), (2) the same profile again to
    // classify user-vs-org for the repos endpoint, then (3) the repos page.
    let mut owner_listing = harness.list("/octocat").unwrap();
    let owner_load = owner_listing.expect_single_fetch();
    assert!(
        owner_load.url.ends_with("/users/octocat"),
        "expected owner profile load first, got {}",
        owner_load.url
    );
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"login":"octocat","type":"User"}"#.to_vec(),
        })])
        .unwrap();
    let classify_fetch = owner_listing.expect_single_fetch();
    assert!(
        classify_fetch.url.ends_with("/users/octocat"),
        "expected owner classification fetch, got {}",
        classify_fetch.url
    );
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
            // The merge: owner faces AND the browsed repo name share one listing.
            assert!(
                names.contains(&"Hello-World"),
                "expected browsed repo in owner listing, got {names:?}"
            );
            assert!(
                names.contains(&"owner.json"),
                "expected owner.json face in owner listing, got {names:?}"
            );
            assert!(
                names.contains(&"profile.md"),
                "expected profile.md face in owner listing, got {names:?}"
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
            step.is_waiting_for_callouts(),
            "expected repo gate fetch for {path}, got {step:?}"
        );
        step.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
            assert_eq!(names, vec!["README.md", "notifications"]);
        },
        other => panic!("expected root listing, got {other:?}"),
    }

    let mut owner_listing = harness.list("/open").unwrap();
    let owner_load = owner_listing.expect_single_fetch();
    assert!(
        owner_load.url.ends_with("/users/open"),
        "expected owner profile load first, got {}",
        owner_load.url
    );
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"login":"open","type":"User"}"#.to_vec(),
        })])
        .unwrap();
    let classify_fetch = owner_listing.expect_single_fetch();
    assert!(
        classify_fetch.url.ends_with("/users/open"),
        "expected owner classification fetch, got {}",
        classify_fetch.url
    );
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
            // The owner listing is its own faces merged with the API-reported
            // repos (here none). The earlier browsed `/open/source` path must
            // NOT surface as a phantom repo child: unclassified scaffolding does
            // not bind. Only the object's own faces remain.
            let mut repo_children: Vec<&str> = names
                .iter()
                .copied()
                .filter(|name| *name != "owner.json" && *name != "profile.md")
                .collect();
            repo_children.sort_unstable();
            assert!(
                repo_children.is_empty(),
                "unexpected repo children after uncached repo traversal: {repo_children:?}"
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
        .list_children(&parse_path("/octocat/Hello-World/repo"), None, None, None)
        .await
        .unwrap();
    match repo_listing {
        NamespaceListOutcome::Subtree(tree_ref) => {
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
        .lookup_child(&parse_path("/octocat/Hello-World"), "repo", None)
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
    use omnifs_wit::provider::types::{CalloutResult, Header, HttpResponse};

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
        .lookup("/octocat/Hello-World/issues/open/999999999", "diff.patch")
        .unwrap();
    match diff_lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::NotFound(_)) => {},
        other => panic!("expected issue diff.patch lookup to be NotFound, got {other:?}"),
    }

    // Reading a face under a missing comment object triggers a fetch; a 404
    // from GitHub propagates as NotFound. A comment is a `{comment_id}` object
    // dir, so the fetchable leaf is `comments/1/comment.json`, which loads the
    // standalone comment GET keyed by comment id.
    let mut issued = harness
        .read("/octocat/Hello-World/issues/open/999999999/comments/1/comment.json")
        .unwrap();
    let fetch = issued.expect_single_fetch();
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/1"),
        "unexpected comment fetch URL: {}",
        fetch.url
    );

    issued
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: Vec::<Header>::new(),
            body: b"{\"message\":\"Not Found\"}".to_vec(),
        })])
        .unwrap();

    // A 404 on the comment object load resolves the object as not-found, keyed
    // to the comment anchor.
    match issued.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::NotFound(_)) => {},
        other => panic!("expected ReadFile NotFound on 404 read, got {other:?}"),
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
                    "checks",
                    "comments",
                    "diff.patch",
                    "files",
                    "item.json",
                    "item.md",
                    "reviews",
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

    body.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .read("/octocat/Hello-World/pulls/open/7/diff.patch")
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

    diff.answer_callouts(vec![CalloutResult::BlobFetched(BlobFetched {
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
        .read("/octocat/Hello-World/pulls/open/7/diff.patch")
        .unwrap();
    assert!(
        retry.is_waiting_for_callouts(),
        "expected PR diff reread to refetch, got {retry:?}"
    );
    retry
        .answer_callouts(vec![CalloutResult::CalloutError(CalloutError {
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
fn github_pr_files_list_and_read_changed_file_objects() {
    use omnifs_wit::provider::types::{CalloutResult, HttpResponse};

    const CHANGED_FILE_ROW: &[u8] = br#"{"changes":7,"unknown_marker":"preserved","filename":"src/lib.rs","deletions":2,"status":"modified","patch":"@@ -1 +1 @@\n-old\n+new","additions":5}"#;
    let files_response = [b"[".as_slice(), CHANGED_FILE_ROW, b"]".as_slice()].concat();
    let harness = github_harness();
    let mut listed = harness
        .list("/octocat/Hello-World/pulls/open/7/files")
        .unwrap();
    let fetch = listed.expect_single_fetch();
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7/files?per_page=100&page=1"),
        "unexpected PR files URL: {}",
        fetch.url
    );

    listed
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: files_response.clone(),
        })])
        .unwrap();
    match listed.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["src%2Flib.rs"]);
            let effects = listed.effects().unwrap();
            let mut preloaded = project_paths(effects);
            preloaded.sort_unstable();
            assert_eq!(
                preloaded,
                vec![
                    "/octocat/Hello-World/pulls/open/7/files/src%2Flib.rs/filename",
                    "/octocat/Hello-World/pulls/open/7/files/src%2Flib.rs/status",
                ]
            );
            assert!(
                effects.canonical.is_empty(),
                "PR files list must not seed changed-file canonicals, got {:?}",
                effects.canonical
            );
        },
        other => panic!("expected PR files listing, got {other:?}"),
    }

    let mut read = harness
        .read("/octocat/Hello-World/pulls/open/7/files/src%2Flib.rs/file.md")
        .unwrap();
    let read_fetch = read.expect_single_fetch();
    assert!(
        read_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7/files?per_page=100&page=1"),
        "unexpected changed-file read URL: {}",
        read_fetch.url
    );
    read.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: files_response,
    })])
    .unwrap();
    match read.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(body.contains("# src/lib.rs"), "unexpected file.md: {body}");
            assert_eq!(
                read.effects().unwrap().canonical[0].bytes.as_slice(),
                CHANGED_FILE_ROW,
                "changed-file canonical must preserve the matched list row verbatim"
            );
        },
        other => panic!("expected changed-file markdown read, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_pr_reviews_and_review_comments_list_and_read_objects() {
    use omnifs_wit::provider::types::{CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut reviews = harness
        .list("/octocat/Hello-World/pulls/open/7/reviews")
        .unwrap();
    let fetch = reviews.expect_single_fetch();
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7/reviews?per_page=100&page=1"),
        "unexpected reviews URL: {}",
        fetch.url
    );
    reviews
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[
                {
                    "id":80,
                    "state":"APPROVED",
                    "body":"Looks good",
                    "user":{"login":"reviewer"}
                }
            ]"#
            .to_vec(),
        })])
        .unwrap();
    match reviews.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["80"]);
            let effects = reviews.effects().unwrap();
            assert!(
                project_paths(effects)
                    .contains(&"/octocat/Hello-World/pulls/open/7/reviews/80/state"),
                "missing review state preload"
            );
            assert!(
                effects.canonical.is_empty(),
                "review list must not seed review canonicals, got {:?}",
                effects.canonical
            );
        },
        other => panic!("expected reviews listing, got {other:?}"),
    }

    let mut review_md = harness
        .read("/octocat/Hello-World/pulls/open/7/reviews/80/review.md")
        .unwrap();
    let review_fetch = review_md.expect_single_fetch();
    assert!(
        review_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7/reviews/80"),
        "unexpected review read URL: {}",
        review_fetch.url
    );
    review_md
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id":80,
                "state":"APPROVED",
                "body":"Looks good",
                "user":{"login":"reviewer"}
            }"#
            .to_vec(),
        })])
        .unwrap();
    match review_md.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(body.contains("APPROVED"), "unexpected review.md: {body}");
        },
        other => panic!("expected review markdown read, got {other:?}"),
    }

    let mut comments = harness
        .list("/octocat/Hello-World/pulls/open/7/reviews/80/comments")
        .unwrap();
    let comments_fetch = comments.expect_single_fetch();
    assert!(
        comments_fetch.url.ends_with(
            "/repos/octocat/Hello-World/pulls/7/reviews/80/comments?per_page=100&page=1"
        ),
        "unexpected review comments URL: {}",
        comments_fetch.url
    );
    comments
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[
                {
                    "id":99,
                    "body":"nit",
                    "path":"src/lib.rs",
                    "diff_hunk":"@@ -1 +1 @@",
                    "user":{"login":"reviewer"}
                }
            ]"#
            .to_vec(),
        })])
        .unwrap();
    match comments.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["99"]);
            assert!(
                comments.effects().unwrap().canonical.is_empty(),
                "review comment list must not seed canonicals"
            );
        },
        other => panic!("expected review comments listing, got {other:?}"),
    }

    let mut comment_md = harness
        .read("/octocat/Hello-World/pulls/open/7/reviews/80/comments/99/comment.md")
        .unwrap();
    let comment_md_fetch = comment_md.expect_single_fetch();
    assert!(
        comment_md_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/comments/99"),
        "unexpected review comment read URL: {}",
        comment_md_fetch.url
    );
    comment_md
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id":99,
                "body":"nit",
                "path":"src/lib.rs",
                "diff_hunk":"@@ -1 +1 @@",
                "user":{"login":"reviewer"}
            }"#
            .to_vec(),
        })])
        .unwrap();
    match comment_md.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(
                body.contains("reviewer on `src/lib.rs`"),
                "unexpected comment.md: {body}"
            );
        },
        other => panic!("expected review comment markdown read, got {other:?}"),
    }
}

#[test]
fn github_pr_checks_list_from_head_sha_and_read_check_run_objects() {
    use omnifs_wit::provider::types::{CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut checks = harness
        .list("/octocat/Hello-World/pulls/open/7/checks")
        .unwrap();
    let pull_fetch = checks.expect_single_fetch();
    assert!(
        pull_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7"),
        "unexpected PR head URL: {}",
        pull_fetch.url
    );
    checks
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"head":{"sha":"abc123"}}"#.to_vec(),
        })])
        .unwrap();
    let check_fetch = checks.expect_single_fetch();
    assert!(
        check_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/commits/abc123/check-runs?per_page=100&page=1"),
        "unexpected check runs URL: {}",
        check_fetch.url
    );
    checks
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "check_runs":[
                    {
                        "id":700,
                        "name":"ci",
                        "status":"completed",
                        "conclusion":"success",
                        "output":{"title":"CI","summary":"All green"}
                    }
                ]
            }"#
            .to_vec(),
        })])
        .unwrap();
    match checks.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["700"]);
            assert!(
                checks.effects().unwrap().canonical.is_empty(),
                "check run list must not seed canonicals"
            );
        },
        other => panic!("expected check runs listing, got {other:?}"),
    }

    let mut check_md = harness
        .read("/octocat/Hello-World/pulls/open/7/checks/700/check.md")
        .unwrap();
    let read_fetch = check_md.expect_single_fetch();
    assert!(
        read_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/check-runs/700"),
        "unexpected check run read URL: {}",
        read_fetch.url
    );
    check_md
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id":700,
                "name":"ci",
                "status":"completed",
                "conclusion":"success",
                "html_url":"https://github.com/octocat/Hello-World/runs/700",
                "output":{"title":"CI","summary":"All green"}
            }"#
            .to_vec(),
        })])
        .unwrap();
    match check_md.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(body.contains("# ci"), "unexpected check.md: {body}");
            assert!(body.contains("All green"), "unexpected check.md: {body}");
        },
        other => panic!("expected check run markdown read, got {other:?}"),
    }
}

#[test]
fn github_notifications_list_and_read_thread_objects() {
    use omnifs_wit::provider::types::{CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut listed = harness.list("/notifications").unwrap();
    let fetch = listed.expect_single_fetch();
    assert!(
        fetch.url.ends_with("/notifications?per_page=50&page=1"),
        "unexpected notifications URL: {}",
        fetch.url
    );
    listed
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[
                {
                    "id":"123",
                    "unread":true,
                    "reason":"mention",
                    "updated_at":"2026-07-05T00:00:00Z",
                    "subject":{"title":"Review requested","type":"PullRequest"},
                    "repository":{"full_name":"octocat/Hello-World"}
                }
            ]"#
            .to_vec(),
        })])
        .unwrap();
    match listed.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            // The generated branch README (route-schema synthesis) lists
            // alongside the notification threads.
            assert_eq!(names, vec!["README.md", "thread-123"]);
            let effects = listed.effects().unwrap();
            assert!(
                effects.canonical.is_empty(),
                "notification list must not seed thread canonicals, got {:?}",
                effects.canonical
            );
        },
        other => panic!("expected notifications listing, got {other:?}"),
    }

    let mut item = harness.read("/notifications/thread-123/item.md").unwrap();
    let read_fetch = item.expect_single_fetch();
    assert!(
        read_fetch.url.ends_with("/notifications/threads/123"),
        "unexpected notification read URL: {}",
        read_fetch.url
    );
    item.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: br#"{
            "id":"123",
            "unread":true,
            "reason":"mention",
            "updated_at":"2026-07-05T00:00:00Z",
            "subject":{"title":"Review requested","type":"PullRequest"},
            "repository":{"full_name":"octocat/Hello-World"}
        }"#
        .to_vec(),
    })])
    .unwrap();
    match item.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(
                body.contains("# Review requested"),
                "unexpected notification item.md: {body}"
            );
        },
        other => panic!("expected notification item read, got {other:?}"),
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
            // A comment is a `{comment_id}` object dir; `comment.md` renders the
            // standalone comment GET. Reading it fetches the comment from
            // upstream rather than serving a provider-side cache.
            name: "issue comment",
            path: "/octocat/Hello-World/issues/open/1/comments/1/comment.md",
            ok_headers: vec![Header {
                name: "etag".to_string(),
                value: "\"comment-1\"".to_string(),
            }],
            ok_body: br#"{"id":1,"user":{"login":"octocat"},"body":"A comment"}"#,
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
            first.is_waiting_for_callouts(),
            "{name}: expected fetch callout on first read, got {first:?}",
            name = case.name
        );
        first
            .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
            second.is_waiting_for_callouts(),
            "{name}: expected fetch callout on second read (no provider cache), got {second:?}",
            name = case.name
        );
        second
            .answer_callouts(vec![CalloutResult::CalloutError(CalloutError {
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

/// Comments are `{comment_id}` object directories. The collection lists each
/// comment by its own id. Each `comment.json` / `comment.md` face loads from
/// the standalone
/// comment GET. This test pins (a) the id-keyed dir model, (b) the refetch
/// invariant: a comment face read fetches upstream and a reread refetches
/// (never serves a provider-side cache), and (c) reject: a face under a missing
/// comment validates to not-found on read.
#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_comment_routes_id_dirs_and_refetch() {
    use omnifs_wit::provider::types::{CalloutError, CalloutResult, Header};

    fn ok_body(body: &[u8]) -> Vec<CalloutResult> {
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"c\"".to_string(),
            }],
            body: body.to_vec(),
        })]
    }

    let harness = github_harness();

    // The issue comment collection lists each comment as an id-keyed DIR. The
    // listing fetches the comments page and emits one directory per comment id.
    let issue_list_path = "/octocat/Hello-World/issues/open/1/comments";
    let mut issue_list = harness.list(issue_list_path).unwrap();
    let list_fetch = issue_list.expect_single_fetch();
    assert!(
        list_fetch
            .url
            .contains("/repos/octocat/Hello-World/issues/1/comments?per_page=100&page=1"),
        "unexpected comment listing URL: {}",
        list_fetch.url
    );
    issue_list
        .answer_callouts(ok_body(
            br#"[{"id":42,"user":{"login":"octocat"},"body":"first issue comment"}]"#,
        ))
        .unwrap();
    match issue_list.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            // The child name is the comment id, not a positional index.
            assert_eq!(names, vec!["42"]);
            assert!(
                matches!(listing.entries[0].kind, EntryKind::Directory),
                "a comment is an object dir, got {:?}",
                listing.entries[0].kind
            );
        },
        other => panic!("expected issue comment listing, got {other:?}"),
    }

    // Listing the comment object dir itself enumerates its faces. Regression
    // guard: the comment's `body.md` derive face is lazy, so the eager-leaf
    // projection on the anchor-listing path does not reject its non-inline body.
    let comment_dir = "/octocat/Hello-World/issues/open/1/comments/42";
    let mut dir = harness.list(comment_dir).unwrap();
    let _ = dir.expect_single_fetch();
    dir.answer_callouts(ok_body(
        br#"{"id":42,"user":{"login":"octocat"},"body":"first issue comment"}"#,
    ))
    .unwrap();
    match dir.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let mut names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec!["author", "body.md", "comment.json", "comment.md"]
            );
        },
        other => panic!("expected comment dir listing, got {other:?}"),
    }

    // `comment.md` renders the standalone comment GET, keyed by comment id.
    let comment_md = "/octocat/Hello-World/issues/open/1/comments/42/comment.md";
    let mut first = harness.read(comment_md).unwrap();
    let first_fetch = first.expect_single_fetch();
    assert!(
        first_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/42"),
        "unexpected comment fetch URL: {}",
        first_fetch.url
    );
    first
        .answer_callouts(ok_body(
            br#"{"id":42,"user":{"login":"octocat"},"body":"A comment"}"#,
        ))
        .unwrap();
    match first.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            assert_eq!(omnifs_itest::expect_inline(file), b"octocat:\nA comment\n");
        },
        other => panic!("expected comment.md content, got {other:?}"),
    }

    // Refetch invariant: a reread does NOT serve a provider-side cache; it
    // fetches again. A network error on the refetch surfaces as Network.
    let mut reread = harness.read(comment_md).unwrap();
    assert!(
        reread.is_waiting_for_callouts(),
        "comment reread should refetch, got {reread:?}"
    );
    reread
        .answer_callouts(vec![CalloutResult::CalloutError(CalloutError {
            kind: omnifs_wit::provider::types::ErrorKind::Network,
            message: "network down".to_string(),
            retryable: true,
        })])
        .unwrap();
    match reread.result().unwrap() {
        OpResult::Error(error) => {
            assert_eq!(error.kind, omnifs_wit::provider::types::ErrorKind::Network);
        },
        other => panic!("expected Network error on comment refetch, got {other:?}"),
    }

    // `comment.json` serves the canonical comment bytes verbatim and refetches
    // identically on a cold reread.
    let comment_json = "/octocat/Hello-World/issues/open/1/comments/42/comment.json";
    let mut json_read = harness.read(comment_json).unwrap();
    assert!(json_read.is_waiting_for_callouts());
    json_read
        .answer_callouts(ok_body(
            br#"{"id":42,"user":{"login":"octocat"},"body":"A comment"}"#,
        ))
        .unwrap();
    match json_read.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(_)) => {},
        other => panic!("expected comment.json read, got {other:?}"),
    }

    // PR comments use the same id-keyed dir model and the same standalone
    // comment GET (comments live under the shared issues endpoint).
    let pr_comment_md = "/octocat/Hello-World/pulls/open/7/comments/9/comment.md";
    let mut pr_read = harness.read(pr_comment_md).unwrap();
    let pr_fetch = pr_read.expect_single_fetch();
    assert!(
        pr_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/9"),
        "unexpected PR comment fetch URL: {}",
        pr_fetch.url
    );
    pr_read
        .answer_callouts(ok_body(
            br#"{"id":9,"user":{"login":"hubot"},"body":"a pr comment"}"#,
        ))
        .unwrap();
    match pr_read.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            assert_eq!(omnifs_itest::expect_inline(file), b"hubot:\na pr comment\n");
        },
        other => panic!("expected PR comment content, got {other:?}"),
    }

    // Reject: a face under a missing comment id validates on read. A 404 on the
    // comment object load resolves the object as not-found.
    let mut missing = harness
        .read("/octocat/Hello-World/issues/open/1/comments/999/comment.json")
        .unwrap();
    let missing_fetch = missing.expect_single_fetch();
    assert!(
        missing_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/999"),
        "unexpected missing-comment fetch URL: {}",
        missing_fetch.url
    );
    missing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: Vec::new(),
            body: b"{\"message\":\"Not Found\"}".to_vec(),
        })])
        .unwrap();
    match missing.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::NotFound(_)) => {},
        other => panic!("expected NotFound on missing comment, got {other:?}"),
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
    issues.answer_callouts(vec![search_page(1500, 1)]).unwrap();
    assert!(
        issues.is_waiting_for_callouts(),
        "expected parallel issue page fetches, got {issues:?}"
    );
    assert_page_fetches(issues.callouts(), 2..=10);
    let issue_pages = (2..=10).map(|page| rest_page(page * 100)).collect();
    issues.answer_callouts(issue_pages).unwrap();
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
    pulls.answer_callouts(vec![search_page(1500, 7)]).unwrap();
    assert!(
        pulls.is_waiting_for_callouts(),
        "expected parallel PR page fetches, got {pulls:?}"
    );
    assert_page_fetches(pulls.callouts(), 2..=10);
    let pr_pages = (2..=10).map(|page| rest_page(page * 100 + 7)).collect();
    pulls.answer_callouts(pr_pages).unwrap();
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

    // Listing the owner object first LOADS its canonical (owner.json/profile.md
    // faces): `Owner::load` probes `/users` then falls back to `/orgs`. Then the
    // `Owner::repos` collection classifies the owner again (user-vs-org) to pick
    // the repos endpoint. Both rounds probe users(404) then orgs(200) here.
    let mut listing = harness.list("/openai").unwrap();
    let load_user = listing.expect_single_fetch();
    assert!(
        load_user.url.ends_with("/users/openai"),
        "expected owner load user probe first, got {}",
        load_user.url
    );

    listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"miss\"".to_string(),
            }],
            body: Vec::new(),
        })])
        .unwrap();
    let load_org = listing.expect_single_fetch();
    assert!(
        load_org.url.ends_with("/orgs/openai"),
        "expected owner load org fallback, got {}",
        load_org.url
    );

    listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "login": "openai",
                "public_repos": 42
            }"#
            .to_vec(),
        })])
        .unwrap();
    // Collection classification: probe users again, fall back to orgs again.
    let classify_user = listing.expect_single_fetch();
    assert!(
        classify_user.url.ends_with("/users/openai"),
        "expected collection classification user probe, got {}",
        classify_user.url
    );
    listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"miss\"".to_string(),
            }],
            body: Vec::new(),
        })])
        .unwrap();
    let classify_org = listing.expect_single_fetch();
    assert!(
        classify_org.url.ends_with("/orgs/openai"),
        "expected collection classification org fallback, got {}",
        classify_org.url
    );
    listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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

    // Root does not enumerate owners: it exposes only the generated README
    // and the notifications literal. Browsed owners do not leak into it.
    let root_listing = harness.list("/").unwrap();
    match root_listing.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["README.md", "notifications"]);
        },
        other => panic!("expected root listing, got {other:?}"),
    }
}

#[test]
fn github_provider_polls_events_and_invalidates_caches() {
    // State is empty and timer ticks are no-ops. This test verifies both the
    // comment read path and the absence of timer callouts or invalidations.
    let harness = github_harness();
    // Read a comment object face: a `{comment_id}` dir's `comment.json` loads the
    // standalone comment GET.
    let comment_path = "/octocat/Hello-World/issues/open/1/comments/1/comment.json";

    let mut issue_cached = harness.read(comment_path).unwrap();
    let issue_fetch = issue_cached.expect_single_fetch();
    assert!(
        issue_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/1"),
        "unexpected comment fetch URL: {}",
        issue_fetch.url
    );
    issue_cached
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"comment-1\"".to_string(),
            }],
            body: br#"{"id":1,"user":{"login":"octocat"},"body":"A comment"}"#.to_vec(),
        })])
        .unwrap();
    match issue_cached.result().unwrap() {
        OpResult::ReadFile(_) => {},
        other => panic!("expected comment ReadFile result, got {other:?}"),
    }
    // TimerTick returns immediately with no callouts or cache invalidations.
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

    fn expect_denied(response: &omnifs_engine::test_support::TestOp<'_>) {
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
        op.answer_callouts(denied_page()).unwrap();
        expect_denied(&op);
    }
}

/// `issues/open/42` and `issues/all/42` share one object load.
#[tokio::test]
async fn open_then_all_one_load() {
    use omnifs_wit::provider::types::{Callout, CalloutResult, HttpResponse, ReadFileOutcome};

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
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
        .apply_effects_for_test(effects, harness.current_generation());
    assert!(harness.cached_canonical_for(open_title).is_some());
    assert!(harness.cached_canonical_for(all_title).is_some());

    let all_title_path = parse_path(all_title);
    let warm = harness
        .runtime
        .namespace()
        .read_file(
            &all_title_path,
            all_title_path.content_type_mime(None).to_string(),
            None,
        )
        .await
        .unwrap();
    match warm.bytes {
        ReadBytes::Inline(bytes) => assert_eq!(bytes, b"Issue forty-two"),
        other => panic!("expected inline title on warm read, got {other:?}"),
    }
}

/// `item.json` bytes equal the single-item GET body verbatim.
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

    step.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
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
