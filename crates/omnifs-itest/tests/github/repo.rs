//! `repo/`: the tree of the last commit of the default branch.

use omnifs_wit::host::types::{
    Callout, CalloutResult, GitRepoInfo, HttpRequest, HttpResponse, ListChildrenResult,
};

use crate::support::{TestOpExt, github_harness, not_modified, request_header};

const TREE: &str = "/octocat/Hello-World/repo";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const NEXT: &str = "89abcdef0123456789abcdef0123456789abcdef";

/// The request for the commit of `HEAD`, with the commit of the tree in
/// quotes as its `ETag`.
fn assert_head_request(request: &HttpRequest) {
    assert!(
        request
            .url
            .ends_with("/repos/octocat/Hello-World/commits/HEAD"),
        "unexpected commit request: {}",
        request.url
    );
    let accepts = request
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("accept"))
        .map(|header| header.value.as_str())
        .collect::<Vec<_>>();
    assert_eq!(accepts, ["application/vnd.github.sha"]);
    assert_eq!(
        request_header(request, "if-none-match"),
        Some(format!("\"{COMMIT}\"").as_str())
    );
}

/// `304` to the commit of the tree keeps the tree.
#[test]
fn github_repo_tree_stands_on_304() {
    let harness = github_harness();
    let mut list = harness.list_page(TREE, None, Some(COMMIT)).unwrap();
    assert_head_request(list.expect_single_fetch());
    list.answer_callouts(vec![CalloutResult::HttpResponse(not_modified())])
        .unwrap();
    assert!(matches!(
        list.into_ok().unwrap(),
        ListChildrenResult::Unchanged(None)
    ));
}

/// A new commit of `HEAD` opens the clone at it and hands its tree off.
#[test]
fn github_repo_tree_opens_a_new_commit() {
    let harness = github_harness();
    let mut list = harness.list_page(TREE, None, Some(COMMIT)).unwrap();
    assert_head_request(list.expect_single_fetch());
    list.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: NEXT.as_bytes().to_vec(),
    })])
    .unwrap();
    let [Callout::GitOpenRepo(open)] = list.callouts() else {
        panic!(
            "expected the open of the new commit, got {:?}",
            list.callouts()
        );
    };
    assert_eq!(open.clone_url, "git@github.com:octocat/Hello-World.git");
    assert_eq!(open.reference, None);
    assert_eq!(open.commit, NEXT);
    list.answer_callouts(vec![CalloutResult::GitRepoOpened(GitRepoInfo {
        repo: 7,
        tree: 7,
    })])
    .unwrap();
    match list.into_ok().unwrap() {
        ListChildrenResult::Subtree(subtree) => {
            assert_eq!(subtree.tree, 7);
            assert_eq!(subtree.commit, NEXT);
        },
        other => panic!("expected the tree at the new commit, got {other:?}"),
    }
}
