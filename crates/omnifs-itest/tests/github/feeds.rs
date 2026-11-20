//! The feeds of the issues, pull requests, and comments of a repository: its
//! first full listing, a stop of it and its continuation, the checks after
//! it, and the feed of an item before it ends.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace, NsError};
use omnifs_itest::RuntimeHarness;
use omnifs_wit::host::types::{Header, HttpRequest, HttpResponse};
use serde_json::{Value, json};

use super::support::{answering, github_harness, not_modified, request_header};

/// Past the deadline of a feed, `DYNAMIC_TTL_MILLIS` after its last answer.
const PAST_DEADLINE: Duration = Duration::from_millis(3_200);

/// The link `next` of the first page of rows.
const NEXT: &str = r#"<https://api.github.com/repositories/1/issues?state=all&page=2>; rel="next""#;

/// The first page of the rows of the repository.
const ROWS: &str = "/repos/octocat/hello/issues?state=all&sort=updated&direction=asc&per_page=100";

fn api(path: &str) -> String {
    format!("https://api.github.com{path}")
}

/// Look up `path` under the mount a segment at a time, to its first missing
/// segment.
async fn lookup(harness: &RuntimeHarness, path: &str) -> Result<LookupAnswer, NsError> {
    let mut answer = harness.namespace.lookup(Path::root(), "github").await?;
    for segment in path.split('/') {
        if answer.is_missing() {
            break;
        }
        answer = harness.namespace.lookup(answer.path, segment).await?;
    }
    Ok(answer)
}

/// The names of the directory at `path`, sorted.
async fn names(harness: &RuntimeHarness, path: &str) -> Result<Vec<String>, NsError> {
    let dir = lookup(harness, path).await?;
    let page = harness
        .namespace
        .readdir(dir.path, DirCursor::start(), 0)
        .await?;
    let mut names = page
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

async fn read(harness: &RuntimeHarness, path: &str) -> Result<String, NsError> {
    let file = lookup(harness, path).await?;
    contents(harness, file.path).await
}

/// The file at `path` under the mount by its path alone, without a lookup,
/// as by a filehandle held across a change of the projection.
async fn read_at(harness: &RuntimeHarness, path: &str) -> Result<String, NsError> {
    let file = Path::parse(&format!("/github/{path}")).unwrap();
    contents(harness, file).await
}

async fn contents(harness: &RuntimeHarness, file: Path) -> Result<String, NsError> {
    let read = harness.namespace.read(file, 0, u32::MAX).await?;
    Ok(String::from_utf8(read.bytes).unwrap())
}

/// Run `operation`, which makes no request.
async fn served<T>(harness: &RuntimeHarness, operation: impl Future<Output = T>) -> T {
    answering(harness, operation, Vec::new()).await.0
}

fn answer(status: u16, headers: &[(&str, &str)], body: &Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| Header {
                name: (*name).to_string(),
                value: (*value).to_string(),
            })
            .collect(),
        body: body.to_string().into_bytes(),
    }
}

fn ok(body: &Value) -> HttpResponse {
    answer(200, &[], body)
}

fn empty(status: u16, headers: &[(&str, &str)]) -> HttpResponse {
    HttpResponse {
        status,
        headers: answer(status, headers, &Value::Null).headers,
        body: Vec::new(),
    }
}

/// An answer of GitHub GraphQL about the repository.
fn graphql(repository: &Value) -> HttpResponse {
    ok(&json!({ "data": { "repository": repository } }))
}

/// The query and the variables of a request of GitHub GraphQL.
fn graphql_of(request: &HttpRequest) -> (String, Value) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, api("/graphql"));
    let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["variables"]["owner"], "octocat");
    assert_eq!(body["variables"]["name"], "hello");
    (
        body["query"].as_str().unwrap().to_owned(),
        body["variables"].clone(),
    )
}

/// A row of the issues of the repository; a pull request has `pull_request`.
fn item(
    number: u64,
    title: &str,
    state: &str,
    updated_at: &str,
    comments: usize,
    pull: bool,
) -> Value {
    let mut row = json!({
        "number": number,
        "title": title,
        "body": format!("{title} body"),
        "state": state,
        "user": { "login": "octocat" },
        "updated_at": updated_at,
        "comments": comments,
    });
    if pull {
        row["pull_request"] =
            json!({ "url": format!("https://api.github.com/repos/octocat/hello/pulls/{number}") });
    }
    row
}

fn comment(id: u64, number: u64, pull: bool, body: &str) -> Value {
    let kind = if pull { "pull" } else { "issues" };
    json!({
        "id": id,
        "user": { "login": "hubot" },
        "body": body,
        "html_url": format!("https://github.com/octocat/hello/{kind}/{number}#issuecomment-{id}"),
    })
}

/// A page of the items of a connection with the counts of their comments.
fn counted(items: &[(u64, usize)]) -> Value {
    json!({
        "page": {
            "nodes": items
                .iter()
                .map(|(number, comments)| json!({ "number": number, "comments": { "totalCount": comments } }))
                .collect::<Vec<_>>(),
            "pageInfo": { "hasNextPage": false, "endCursor": null },
        },
    })
}

/// An item with a page of the ids of its comments.
fn ids(ids: &[(u64, &str)], next: Option<&str>) -> Value {
    json!({
        "comments": {
            "nodes": ids
                .iter()
                .map(|(id, updated_at)| json!({ "databaseId": id, "updatedAt": updated_at }))
                .collect::<Vec<_>>(),
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
        },
    })
}

/// The first full listing takes the rows by their link `next`, then the
/// comments by page, then the counts of the issues and of the pull requests.
/// An item with another count than its `comments/` takes its IDs: a comment
/// that a deletion moved past a page comes by its ID before the next page of
/// the IDs, the comments of the projection do not, whatever their
/// `updatedAt`, and the deleted comment leaves. The four folders are complete
/// after the last part, and `open/` shares the facts of `all/`. The facts live under the owner as the paths
/// name it, `OctoCat`, while the requests name the owner of the key,
/// `octocat`.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_first_full_listing_takes_the_rows_then_the_comments_then_the_counts() {
    let harness = github_harness();
    // The first page of comments has comments 1 to 100, comment 50 on the
    // pull request 2; its deletion moves comment 101 onto the first page
    // before the second is asked, which has comment 102 alone.
    let first = (1..=100)
        .map(|id| {
            if id == 50 {
                comment(50, 2, true, "on the pull")
            } else {
                comment(id, 1, false, &format!("comment {id}"))
            }
        })
        .collect::<Vec<_>>();
    // Each comment was edited after the last row, at 10:00.
    let ids_of_1 = (1..=102)
        .filter(|id| *id != 50)
        .map(|id| (id, "2026-11-18T10:30:00Z"))
        .collect::<Vec<_>>();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "OctoCat/hello/issues/all"),
        vec![
            answer(
                200,
                &[("Link", NEXT)],
                &json!([
                    item(3, "Three", "closed", "2026-11-18T08:00:00Z", 0, false),
                    item(1, "One", "open", "2026-11-18T09:00:00Z", 101, false),
                ]),
            ),
            ok(&json!([item(
                2,
                "Two",
                "open",
                "2026-11-18T10:00:00Z",
                1,
                true
            )])),
            ok(&Value::Array(first)),
            ok(&json!([comment(102, 1, false, "comment 102")])),
            graphql(&counted(&[(1, 101), (3, 0)])),
            graphql(&json!({ "item": ids(&ids_of_1[..100], Some("ids-2")) })),
            ok(&comment(101, 1, false, "comment 101")),
            graphql(&json!({ "item": ids(&ids_of_1[100..], None) })),
            graphql(&counted(&[(2, 0)])),
            graphql(&json!({ "item": ids(&[], None) })),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "3"]);
    assert_eq!(requests[0].url, api(ROWS));
    assert_eq!(request_header(&requests[0], "if-none-match"), None);
    assert_eq!(
        requests[1].url,
        api("/repositories/1/issues?state=all&page=2")
    );
    for (request, page) in [(&requests[2], 1), (&requests[3], 2)] {
        assert_eq!(
            request.url,
            api(&format!(
                "/repos/octocat/hello/issues/comments?sort=created&direction=asc&per_page=100&page={page}"
            ))
        );
    }
    let (query, variables) = graphql_of(&requests[4]);
    assert!(
        query.contains("page: issues(first: 100, after: $after)"),
        "{query}"
    );
    assert_eq!(variables["after"], Value::Null);
    for (request, after) in [(&requests[5], Value::Null), (&requests[7], json!("ids-2"))] {
        let (query, variables) = graphql_of(request);
        assert!(
            query.contains("item: issueOrPullRequest(number: $number)"),
            "{query}"
        );
        assert_eq!(variables["number"], 1);
        assert_eq!(variables["after"], after);
    }
    assert_eq!(
        requests[6].url,
        api("/repos/octocat/hello/issues/comments/101")
    );
    let (query, _) = graphql_of(&requests[8]);
    assert!(
        query.contains("page: pullRequests(first: 100, after: $after)"),
        "{query}"
    );
    assert_eq!(graphql_of(&requests[9]).1["number"], 2);

    // The projection serves the folders and the facts of the answer alone.
    for (folder, expected) in [
        ("issues/open", vec!["1"]),
        ("pulls/all", vec!["2"]),
        ("pulls/open", vec!["2"]),
        ("issues/all/3/comments", vec![]),
        ("pulls/all/2/comments", vec![]),
        ("pulls/open/2/comments", vec![]),
    ] {
        assert_eq!(
            served(
                &harness,
                names(&harness, &format!("OctoCat/hello/{folder}"))
            )
            .await
            .unwrap(),
            expected,
            "{folder}"
        );
    }
    for folder in ["all", "open"] {
        let comments = served(
            &harness,
            names(
                &harness,
                &format!("OctoCat/hello/issues/{folder}/1/comments"),
            ),
        )
        .await
        .unwrap();
        assert_eq!(comments.len(), 101, "{folder}");
        assert!(comments.contains(&"101".to_string()) && !comments.contains(&"50".to_string()));
        assert_eq!(
            served(
                &harness,
                read(
                    &harness,
                    &format!("OctoCat/hello/issues/{folder}/1/comments/101/body.md")
                ),
            )
            .await
            .unwrap(),
            "comment 101"
        );
    }
    for (file, expected) in [
        ("issues/all/1/title", "One"),
        ("issues/all/1/body", "One body"),
        ("issues/all/3/state", "closed"),
        ("pulls/open/2/user", "octocat"),
        ("issues/open/1/comments/7/author", "hubot"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &format!("OctoCat/hello/{file}")))
                .await
                .unwrap(),
            expected,
            "{file}"
        );
    }
    assert!(
        served(&harness, lookup(&harness, "OctoCat/hello/issues/open/3"))
            .await
            .unwrap()
            .is_missing()
    );
    let faces = served(&harness, names(&harness, "OctoCat/hello/issues/open/1"))
        .await
        .unwrap();
    assert!(faces.contains(&"title".to_string()), "{faces:?}");
    assert_eq!(
        served(
            &harness,
            read(&harness, "OctoCat/hello/issues/open/1/title")
        )
        .await
        .unwrap(),
        "One"
    );
}

/// A first full listing that stops at a rate limit gives each read of the
/// four folders the error of the stop while the files it took serve, and the
/// next read after the window continues it from its next page.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_full_listing_stopped_at_a_rate_limit_continues_from_its_next_page() {
    let harness = github_harness();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            answer(
                200,
                &[("Link", NEXT)],
                &json!([
                    item(1, "One", "open", "2026-11-18T09:00:00Z", 0, false),
                    item(2, "Two", "open", "2026-11-18T09:30:00Z", 1, true),
                ]),
            ),
            empty(403, &[("x-ratelimit-remaining", "0"), ("retry-after", "2")]),
        ],
    )
    .await;
    assert!(
        matches!(stopped, Err(NsError::RateLimited { .. })),
        "{stopped:?}"
    );
    assert_eq!(requests.len(), 2);

    for folder in ["issues/all", "issues/open", "pulls/all", "pulls/open"] {
        let listing = served(
            &harness,
            names(&harness, &format!("octocat/hello/{folder}")),
        )
        .await;
        assert!(
            matches!(listing, Err(NsError::RateLimited { .. })),
            "{folder}: {listing:?}"
        );
    }
    for (file, expected) in [
        ("issues/all/1/title", "One"),
        ("pulls/open/2/user", "octocat"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &format!("octocat/hello/{file}")))
                .await
                .unwrap(),
            expected,
            "{file}"
        );
    }

    tokio::time::sleep(Duration::from_millis(2_200)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/pulls/all"),
        vec![
            ok(&json!([item(
                3,
                "Three",
                "closed",
                "2026-11-18T10:00:00Z",
                0,
                false
            )])),
            ok(&json!([comment(20, 2, true, "on the pull")])),
            graphql(&counted(&[(1, 0), (3, 0)])),
            graphql(&counted(&[(2, 1)])),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["2"]);
    assert_eq!(
        requests[0].url,
        api("/repositories/1/issues?state=all&page=2")
    );
    assert_eq!(
        served(&harness, names(&harness, "octocat/hello/issues/all"))
            .await
            .unwrap(),
        ["1", "3"]
    );
    assert_eq!(
        served(
            &harness,
            names(&harness, "octocat/hello/pulls/open/2/comments")
        )
        .await
        .unwrap(),
        ["20"]
    );
}

/// An item from a page of rows serves from the projection within the
/// deadline of the state its row gave it, its `comments/` with the count `0`
/// without a request, and is asked for past it with `If-Modified-Since` and
/// the HTTP date of its `updated_at`.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_from_a_page_of_rows_is_asked_for_with_if_modified_since() {
    let harness = github_harness();
    let (stopped, _) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            answer(
                200,
                &[("Link", NEXT)],
                &json!([item(1, "One", "open", "2026-11-18T09:00:00Z", 0, false)]),
            ),
            empty(502, &[]),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    assert!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/all/1/comments")
        )
        .await
        .unwrap()
        .is_empty()
    );

    tokio::time::sleep(PAST_DEADLINE).await;
    let (title, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/1/title"),
        vec![not_modified()],
    )
    .await;
    assert_eq!(title.unwrap(), "One");
    assert_eq!(requests[0].url, api("/repos/octocat/hello/issues/1"));
    assert_eq!(
        request_header(&requests[0], "if-modified-since"),
        Some("Wed, 18 Nov 2026 09:00:00 GMT")
    );
    assert_eq!(request_header(&requests[0], "if-none-match"), None);
}

/// The first full listing of a check test: one page of `rows`, one of
/// `comments`, and the counts of the comments of the issues and of the pull
/// requests.
async fn first_full_listing(
    harness: &RuntimeHarness,
    rows: &Value,
    comments: &Value,
    issues: &[(u64, usize)],
    pulls: &[(u64, usize)],
) {
    let (listing, _) = answering(
        harness,
        names(harness, "octocat/hello/issues/all"),
        vec![
            ok(rows),
            ok(comments),
            graphql(&counted(issues)),
            graphql(&counted(pulls)),
        ],
    )
    .await;
    listing.unwrap();
}

/// Past the deadline of the first full listing, a check takes the rows since
/// its time, the comments since that time, and the counts with the IDs of the
/// changed items: a comment updated since that no list brought comes by its
/// ID, a deleted comment leaves, a changed item with the count `comments` `0`
/// has an empty `comments/` without its IDs, and a count of the issues other
/// than that of `issues/all/` with the new ones takes the numbers, which
/// remove the missing issue.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_check_takes_the_rows_and_the_comments_since_its_time_and_the_ids_of_the_changed_items() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &json!([
            item(4, "Four", "open", "2026-11-18T08:00:00Z", 0, false),
            item(1, "One", "open", "2026-11-18T09:00:00Z", 2, false),
        ]),
        &json!([
            comment(10, 1, false, "first"),
            comment(12, 1, false, "second")
        ]),
        &[(1, 2), (4, 0)],
        &[],
    )
    .await;

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            answer(
                200,
                &[("ETag", r#""rows-1""#)],
                &json!([
                    item(1, "One again", "open", "2026-11-18T11:00:00Z", 2, false),
                    item(5, "Five", "open", "2026-11-18T11:30:00Z", 0, false),
                ]),
            ),
            ok(&json!([comment(11, 1, false, "third")])),
            graphql(&json!({
                "issues": { "totalCount": 2 },
                "pullRequests": { "totalCount": 0 },
                "i1": ids(&[(11, "2026-11-18T11:00:00Z"), (12, "2026-11-18T10:30:00Z")], None),
            })),
            ok(&comment(12, 1, false, "second, edited")),
            graphql(&json!({
                "page": {
                    "nodes": [{ "number": 1 }, { "number": 5 }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                },
            })),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "5"]);
    assert_eq!(
        requests[0].url,
        api(&format!("{ROWS}&since=2026-11-18T09:00:00Z"))
    );
    assert_eq!(request_header(&requests[0], "if-none-match"), None);
    assert_eq!(
        requests[1].url,
        api(
            "/repos/octocat/hello/issues/comments?sort=created&direction=asc&per_page=100\
             &since=2026-11-18T09:00:00Z&page=1"
        )
    );
    let (query, _) = graphql_of(&requests[2]);
    assert!(
        query.contains("issues { totalCount } pullRequests { totalCount }"),
        "{query}"
    );
    assert!(
        query.contains("i1: issueOrPullRequest(number: 1)"),
        "{query}"
    );
    assert!(!query.contains("(number: 5)"), "{query}");
    assert_eq!(
        requests[3].url,
        api("/repos/octocat/hello/issues/comments/12")
    );
    let (query, _) = graphql_of(&requests[4]);
    assert!(
        query.contains("page: issues(first: 100, after: $after) { nodes { number }"),
        "{query}"
    );

    assert_eq!(
        served(&harness, names(&harness, "octocat/hello/issues/open"))
            .await
            .unwrap(),
        ["1", "5"]
    );
    assert_eq!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/all/1/comments")
        )
        .await
        .unwrap(),
        ["11", "12"]
    );
    for (file, expected) in [
        ("issues/all/1/title", "One again"),
        ("issues/open/1/comments/11/body.md", "third"),
        ("issues/all/1/comments/12/body.md", "second, edited"),
        ("issues/all/5/title", "Five"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &format!("octocat/hello/{file}")))
                .await
                .unwrap(),
            expected,
            "{file}"
        );
    }
    for path in ["issues/all/4", "issues/open/4", "issues/all/1/comments/10"] {
        assert!(
            served(&harness, lookup(&harness, &format!("octocat/hello/{path}")))
                .await
                .unwrap()
                .is_missing(),
            "{path}"
        );
    }
}

/// A check whose rows keep its time keeps the `ETag` of their answer, and the
/// next check asks with it: `304` asks for the counts alone, and the same
/// counts answer `unchanged`, which renews the deadline.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_with_304_asks_for_the_counts_alone() {
    let harness = github_harness();
    let one = || item(1, "One", "open", "2026-11-18T09:00:00Z", 0, false);
    first_full_listing(&harness, &json!([one()]), &json!([]), &[(1, 0)], &[]).await;

    tokio::time::sleep(PAST_DEADLINE).await;
    answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            answer(200, &[("ETag", r#""rows-1""#)], &json!([one()])),
            ok(&json!([])),
            graphql(&json!({
                "issues": { "totalCount": 1 },
                "pullRequests": { "totalCount": 0 },
            })),
        ],
    )
    .await
    .0
    .unwrap();

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            not_modified(),
            graphql(&json!({
                "issues": { "totalCount": 1 },
                "pullRequests": { "totalCount": 0 },
            })),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1"]);
    assert_eq!(
        requests[0].url,
        api(&format!("{ROWS}&since=2026-11-18T09:00:00Z"))
    );
    assert_eq!(
        request_header(&requests[0], "if-none-match"),
        Some(r#""rows-1""#)
    );
    let (query, _) = graphql_of(&requests[1]);
    assert!(!query.contains("issueOrPullRequest"), "{query}");
    assert_eq!(
        served(&harness, names(&harness, "octocat/hello/issues/all"))
            .await
            .unwrap(),
        ["1"]
    );
}

/// Before the first full listing ends, an item takes its own answers: `200`
/// with its facts, `304` with the `ETag` of its token, `301`, `404`, and
/// `410` with its removal, and an item of the other kind is not found. Its
/// `comments/` takes the pages of its comments with its IDs for another
/// count than that of its token; a changed answer takes the comments since
/// the `updated_at` of the previous one with the IDs, or empties `comments/`
/// for the count `0` without a request. `open/` shares the facts of `all/`.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn an_item_before_the_end_of_the_first_full_listing_takes_its_own_answers() {
    let harness = github_harness();
    for (number, comments) in [(3, 2), (6, 0), (7, 0), (8, 0), (9, 0)] {
        let (title, requests) = answering(
            &harness,
            read(
                &harness,
                &format!("octocat/hello/issues/all/{number}/title"),
            ),
            vec![answer(
                200,
                &[("ETag", &format!(r#""item-{number}""#))],
                &item(
                    number,
                    &format!("Item {number}"),
                    "open",
                    "2026-11-18T09:00:00Z",
                    comments,
                    false,
                ),
            )],
        )
        .await;
        assert_eq!(title.unwrap(), format!("Item {number}"));
        assert_eq!(
            requests[0].url,
            api(&format!("/repos/octocat/hello/issues/{number}"))
        );
        assert_eq!(request_header(&requests[0], "if-none-match"), None);
    }
    assert_eq!(
        served(
            &harness,
            read(&harness, "octocat/hello/issues/open/7/state")
        )
        .await
        .unwrap(),
        "open"
    );
    assert!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/all/7/comments")
        )
        .await
        .unwrap()
        .is_empty()
    );
    let (pull, _) = answering(
        &harness,
        lookup(&harness, "octocat/hello/pulls/all/5"),
        vec![ok(&item(
            5,
            "Five",
            "open",
            "2026-11-18T09:00:00Z",
            0,
            false,
        ))],
    )
    .await;
    assert!(pull.unwrap().is_missing(), "an issue is not a pull request");

    let (comments, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all/3/comments"),
        vec![
            ok(&json!([comment(30, 3, false, "first")])),
            graphql(&json!({
                "item": ids(&[(30, "2026-11-18T08:00:00Z"), (31, "2026-11-18T08:30:00Z")], None),
            })),
            ok(&comment(31, 3, false, "second")),
        ],
    )
    .await;
    assert_eq!(comments.unwrap(), ["30", "31"]);
    assert_eq!(
        requests[0].url,
        api("/repos/octocat/hello/issues/3/comments?per_page=100&page=1")
    );
    assert_eq!(graphql_of(&requests[1]).1["number"], 3);
    assert_eq!(
        requests[2].url,
        api("/repos/octocat/hello/issues/comments/31")
    );
    assert_eq!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/open/3/comments")
        )
        .await
        .unwrap(),
        ["30", "31"]
    );

    tokio::time::sleep(PAST_DEADLINE).await;
    let (title, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/7/title"),
        vec![not_modified()],
    )
    .await;
    assert_eq!(title.unwrap(), "Item 7");
    assert_eq!(
        request_header(&requests[0], "if-none-match"),
        Some(r#""item-7""#)
    );
    for (number, status) in [(6, 301), (8, 404), (9, 410)] {
        let (found, requests) = answering(
            &harness,
            lookup(&harness, &format!("octocat/hello/issues/all/{number}")),
            vec![empty(status, &[])],
        )
        .await;
        assert!(found.unwrap().is_missing(), "{status}");
        assert_eq!(
            request_header(&requests[0], "if-none-match"),
            Some(format!(r#""item-{number}""#).as_str())
        );
    }

    tokio::time::sleep(PAST_DEADLINE).await;
    let (title, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/7/title"),
        vec![
            answer(
                200,
                &[("ETag", r#""item-7b""#)],
                &item(
                    7,
                    "Item 7 closed",
                    "closed",
                    "2026-11-18T10:00:00Z",
                    1,
                    false,
                ),
            ),
            ok(&json!([comment(70, 7, false, "late")])),
            graphql(&json!({ "item": ids(&[(70, "2026-11-18T10:00:00Z")], None) })),
        ],
    )
    .await;
    assert_eq!(title.unwrap(), "Item 7 closed");
    assert_eq!(
        requests[1].url,
        api(
            "/repos/octocat/hello/issues/7/comments?per_page=100&since=2026-11-18T09:00:00Z&page=1"
        )
    );
    assert_eq!(graphql_of(&requests[2]).1["number"], 7);
    assert_eq!(
        served(
            &harness,
            read(&harness, "octocat/hello/issues/all/7/comments/70/body.md")
        )
        .await
        .unwrap(),
        "late"
    );
    let (title, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/3/title"),
        vec![answer(
            200,
            &[("ETag", r#""item-3b""#)],
            &item(3, "Item 3", "open", "2026-11-18T10:00:00Z", 0, false),
        )],
    )
    .await;
    assert_eq!(title.unwrap(), "Item 3");
    assert_eq!(
        request_header(&requests[0], "if-none-match"),
        Some(r#""item-3""#)
    );
    assert!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/open/3/comments")
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        served(&harness, lookup(&harness, "octocat/hello/issues/open/7"))
            .await
            .unwrap()
            .is_missing(),
        "a closed issue leaves issues/open"
    );
}

/// A check takes each page of its rows by the link `next` of the previous
/// one, and a count of the pull requests other than that of `pulls/all/`
/// with the new ones takes the numbers of the pull requests, which remove
/// the missing pull request.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_takes_each_page_of_its_rows_and_the_numbers_of_the_pull_requests() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &json!([
            item(2, "Two", "open", "2026-11-18T08:00:00Z", 0, true),
            item(1, "One", "open", "2026-11-18T09:00:00Z", 0, false),
        ]),
        &json!([]),
        &[(1, 0)],
        &[(2, 0)],
    )
    .await;

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/pulls/all"),
        vec![
            answer(
                200,
                &[("Link", NEXT)],
                &json!([item(
                    1,
                    "One again",
                    "open",
                    "2026-11-18T10:00:00Z",
                    0,
                    false
                )]),
            ),
            ok(&json!([item(
                3,
                "Three",
                "open",
                "2026-11-18T11:00:00Z",
                0,
                true
            )])),
            ok(&json!([])),
            graphql(&json!({
                "issues": { "totalCount": 1 },
                "pullRequests": { "totalCount": 1 },
            })),
            graphql(&json!({
                "page": {
                    "nodes": [{ "number": 3 }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                },
            })),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["3"]);
    assert_eq!(
        requests[0].url,
        api(&format!("{ROWS}&since=2026-11-18T09:00:00Z"))
    );
    assert_eq!(
        requests[1].url,
        api("/repositories/1/issues?state=all&page=2")
    );
    assert_eq!(
        requests[2].url,
        api(
            "/repos/octocat/hello/issues/comments?sort=created&direction=asc&per_page=100\
             &since=2026-11-18T09:00:00Z&page=1"
        )
    );
    let (query, _) = graphql_of(&requests[3]);
    assert!(!query.contains("issueOrPullRequest"), "{query}");
    let (query, _) = graphql_of(&requests[4]);
    assert!(
        query.contains("page: pullRequests(first: 100, after: $after) { nodes { number }"),
        "{query}"
    );
    assert_eq!(
        served(&harness, read(&harness, "octocat/hello/issues/all/1/title"))
            .await
            .unwrap(),
        "One again"
    );
    assert_eq!(
        served(&harness, names(&harness, "octocat/hello/pulls/open"))
            .await
            .unwrap(),
        ["3"]
    );
    assert!(
        served(&harness, lookup(&harness, "octocat/hello/pulls/all/2"))
            .await
            .unwrap()
            .is_missing()
    );
}

/// The pages of the rows omit some items of a repository. A check whose
/// counts are other than the counts of the projection takes each item that
/// the numbers bring and the projection lacks by its number, with its
/// comments, and the next check, with the same counts, asks for no numbers.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_takes_by_its_number_an_item_that_the_rows_omit() {
    let harness = github_harness();
    let one = || item(1, "One", "open", "2026-11-18T09:00:00Z", 0, false);
    first_full_listing(&harness, &json!([one()]), &json!([]), &[(1, 0)], &[]).await;

    let totals = || {
        graphql(&json!({
            "issues": { "totalCount": 2 },
            "pullRequests": { "totalCount": 0 },
        }))
    };
    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            answer(200, &[("ETag", r#""rows-1""#)], &json!([one()])),
            ok(&json!([])),
            totals(),
            graphql(&json!({
                "page": {
                    "nodes": [{ "number": 1 }, { "number": 5 }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                },
            })),
            ok(&item(5, "Five", "closed", "2026-11-17T08:00:00Z", 0, false)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "5"]);
    let (query, _) = graphql_of(&requests[3]);
    assert!(
        query.contains("page: issues(first: 100, after: $after) { nodes { number }"),
        "{query}"
    );
    assert_eq!(requests[4].url, api("/repos/octocat/hello/issues/5"));
    assert_eq!(
        served(&harness, read(&harness, "octocat/hello/issues/all/5/title"))
            .await
            .unwrap(),
        "Five"
    );

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![not_modified(), totals()],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "5"]);
    assert_eq!(requests.len(), 2);
}

/// A check makes a part of each request and publishes its parts with its
/// last one. A `RATE_LIMITED` answer of GitHub GraphQL after its rows and
/// comments stops it with the window of its `x-ratelimit-reset`, in which a
/// read asks GitHub nothing and the files from before the check serve; the
/// first read after the window continues the check with its next `POST`
/// alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_stopped_by_the_rate_limit_of_github_graphql_continues_with_its_next_post() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &json!([item(1, "One", "open", "2026-11-18T09:00:00Z", 0, false)]),
        &json!([]),
        &[(1, 0)],
        &[],
    )
    .await;

    tokio::time::sleep(PAST_DEADLINE).await;
    let reset = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + Duration::from_secs(2))
        .as_secs()
        .to_string();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            ok(&json!([item(
                1,
                "One again",
                "open",
                "2026-11-18T10:00:00Z",
                0,
                false
            )])),
            ok(&json!([])),
            answer(
                200,
                &[("x-ratelimit-reset", &reset)],
                &json!({
                    "data": null,
                    "errors": [{ "type": "RATE_LIMITED", "message": "API rate limit exceeded" }],
                }),
            ),
        ],
    )
    .await;
    let Err(NsError::RateLimited { retry_after }) = stopped else {
        panic!("{stopped:?}");
    };
    assert!(
        retry_after.is_some_and(|window| window <= Duration::from_secs(2)),
        "{retry_after:?}"
    );
    graphql_of(&requests[2]);
    assert_eq!(
        served(&harness, read(&harness, "octocat/hello/issues/all/1/title"))
            .await
            .unwrap(),
        "One"
    );

    tokio::time::sleep(Duration::from_millis(2_200)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![graphql(&json!({
            "issues": { "totalCount": 1 },
            "pullRequests": { "totalCount": 0 },
        }))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1"]);
    let (query, _) = graphql_of(&requests[0]);
    assert!(
        query.contains("issues { totalCount } pullRequests { totalCount }"),
        "{query}"
    );
    assert_eq!(
        served(&harness, read(&harness, "octocat/hello/issues/all/1/title"))
            .await
            .unwrap(),
        "One again"
    );
}

/// A check stopped by the network gives a read its error, and each next read
/// continues it with its next request alone, whatever its deadline: the read
/// of a network that fails again takes its error.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_stopped_by_the_network_gives_its_error_to_a_read() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &json!([item(1, "One", "open", "2026-11-18T09:00:00Z", 0, false)]),
        &json!([]),
        &[(1, 0)],
        &[],
    )
    .await;

    tokio::time::sleep(PAST_DEADLINE).await;
    let (stopped, _) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/1/title"),
        vec![
            ok(&json!([item(
                1,
                "One again",
                "open",
                "2026-11-18T10:00:00Z",
                0,
                false
            )])),
            empty(502, &[]),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");

    let comments = api(
        "/repos/octocat/hello/issues/comments?sort=created&direction=asc&per_page=100\
         &since=2026-11-18T09:00:00Z&page=1",
    );
    let (stopped, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/1/title"),
        vec![empty(502, &[])],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    assert_eq!(requests[0].url, comments);

    let (title, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/1/title"),
        vec![
            ok(&json!([])),
            graphql(&json!({
                "issues": { "totalCount": 1 },
                "pullRequests": { "totalCount": 0 },
            })),
        ],
    )
    .await;
    assert_eq!(title.unwrap(), "One again");
    assert_eq!(requests[0].url, comments);
}

/// A check asks GitHub GraphQL for the IDs of at most 100 changed items with
/// comments in a `POST`, the first with the counts.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_check_asks_github_graphql_for_a_hundred_changed_items_a_post() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &json!([item(
            200,
            "Two hundred",
            "open",
            "2026-11-18T09:00:00Z",
            0,
            false
        )]),
        &json!([]),
        &[(200, 0)],
        &[],
    )
    .await;

    let rows = (1..=101)
        .map(|number| item(number, "Item", "closed", "2026-11-18T10:00:00Z", 1, false))
        .collect::<Vec<_>>();
    let comments = (1..=101)
        .map(|number| comment(1_000 + number, number, false, "a comment"))
        .collect::<Vec<_>>();
    let page = |numbers: std::ops::RangeInclusive<u64>| {
        Value::Object(
            numbers
                .map(|number| {
                    (
                        format!("i{number}"),
                        ids(&[(1_000 + number, "2026-11-18T10:00:00Z")], None),
                    )
                })
                .collect(),
        )
    };
    let mut first = page(1..=100);
    first["issues"] = json!({ "totalCount": 102 });
    first["pullRequests"] = json!({ "totalCount": 0 });

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            answer(200, &[("Link", NEXT)], &Value::Array(rows[..100].to_vec())),
            ok(&Value::Array(rows[100..].to_vec())),
            ok(&Value::Array(comments[..100].to_vec())),
            ok(&Value::Array(comments[100..].to_vec())),
            graphql(&first),
            graphql(&page(101..=101)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap().len(), 102);
    let (query, _) = graphql_of(&requests[4]);
    assert!(query.contains("issues { totalCount }"), "{query}");
    assert!(
        query.contains("i100: issueOrPullRequest(number: 100)"),
        "{query}"
    );
    assert!(!query.contains("(number: 101)"), "{query}");
    let (query, _) = graphql_of(&requests[5]);
    assert!(!query.contains("totalCount"), "{query}");
    assert!(
        query.contains("i101: issueOrPullRequest(number: 101)"),
        "{query}"
    );
    assert_eq!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/all/101/comments")
        )
        .await
        .unwrap(),
        ["1101"]
    );
}

/// A first full listing stopped by the network at a page of its rows gives
/// each read of its folders the error of the network of its next page alone,
/// and the next read continues it from that page.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_full_listing_stopped_by_the_network_continues_from_its_next_page() {
    let harness = github_harness();
    let next = api("/repositories/1/issues?state=all&page=2");
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            answer(
                200,
                &[("Link", NEXT)],
                &json!([item(1, "One", "open", "2026-11-18T09:00:00Z", 0, false)]),
            ),
            empty(502, &[]),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    assert_eq!(requests[1].url, next);

    let (stopped, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/pulls/all"),
        vec![empty(502, &[])],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    assert_eq!(requests[0].url, next);

    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            ok(&json!([item(
                2,
                "Two",
                "open",
                "2026-11-18T10:00:00Z",
                0,
                false
            )])),
            ok(&json!([])),
            graphql(&counted(&[(1, 0), (2, 0)])),
            graphql(&counted(&[])),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "2"]);
    assert_eq!(requests[0].url, next);
}

/// The first answer of item 7, `Seven`, with the count `comments` `0`, and,
/// past its deadline, the request of its later answer for a change to
/// `Seven again` with one comment: its `GET` and the page of its comments
/// since its time, which `stop` answers.
async fn later_answer_stopped(harness: &RuntimeHarness, stop: HttpResponse) -> NsError {
    let (title, _) = answering(
        harness,
        read(harness, "octocat/hello/issues/all/7/title"),
        vec![answer(
            200,
            &[("ETag", r#""item-7""#)],
            &item(7, "Seven", "open", "2026-11-18T09:00:00Z", 0, false),
        )],
    )
    .await;
    assert_eq!(title.unwrap(), "Seven");

    tokio::time::sleep(PAST_DEADLINE).await;
    let (stopped, requests) = answering(
        harness,
        read(harness, "octocat/hello/issues/all/7/title"),
        vec![
            answer(
                200,
                &[("ETag", r#""item-7b""#)],
                &item(7, "Seven again", "open", "2026-11-18T10:00:00Z", 1, false),
            ),
            stop,
        ],
    )
    .await;
    assert_eq!(requests[1].url, api(ITEM_7_SINCE));
    stopped.unwrap_err()
}

/// The page of the comments of item 7 since the time of its first answer.
const ITEM_7_SINCE: &str =
    "/repos/octocat/hello/issues/7/comments?per_page=100&since=2026-11-18T09:00:00Z&page=1";

/// The end of the later answer of item 7 from the page of its comments since
/// its time: that page, with its comment, and its IDs.
async fn later_answer_continued(harness: &RuntimeHarness) {
    let (title, requests) = answering(
        harness,
        read(harness, "octocat/hello/issues/all/7/title"),
        vec![
            ok(&json!([comment(70, 7, false, "late")])),
            graphql(&json!({ "item": ids(&[(70, "2026-11-18T10:00:00Z")], None) })),
        ],
    )
    .await;
    assert_eq!(title.unwrap(), "Seven again");
    assert_eq!(requests[0].url, api(ITEM_7_SINCE));
    assert_eq!(
        served(
            harness,
            read(harness, "octocat/hello/issues/all/7/comments/70/body.md")
        )
        .await
        .unwrap(),
        "late"
    );
}

/// A later answer of an item stopped at a rate limit at the page of its
/// comments since its time serves the facts from before it in the window,
/// without a request, and continues from that page alone after the window.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_answer_of_an_item_stopped_at_a_rate_limit_continues_from_its_next_request() {
    let harness = github_harness();
    let stopped = later_answer_stopped(
        &harness,
        empty(403, &[("x-ratelimit-remaining", "0"), ("retry-after", "2")]),
    )
    .await;
    assert!(
        matches!(stopped, NsError::RateLimited { .. }),
        "{stopped:?}"
    );
    assert_eq!(
        served(&harness, read(&harness, "octocat/hello/issues/all/7/title"))
            .await
            .unwrap(),
        "Seven"
    );

    tokio::time::sleep(Duration::from_millis(2_200)).await;
    later_answer_continued(&harness).await;
}

/// A later answer of an item stopped by the network at the page of its
/// comments since its time gives each next read the error of the network of
/// that page alone, and the next read continues it from that page.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_answer_of_an_item_stopped_by_the_network_continues_from_its_next_request() {
    let harness = github_harness();
    let stopped = later_answer_stopped(&harness, empty(502, &[])).await;
    assert!(matches!(stopped, NsError::Network), "{stopped:?}");

    let (stopped, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/7/title"),
        vec![empty(502, &[])],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    assert_eq!(requests[0].url, api(ITEM_7_SINCE));

    later_answer_continued(&harness).await;
}

/// The first answer of an item, which the lookup of its folder starts for a
/// read of its `title` or of its `comments/`, takes its `GET` alone. Stopped
/// by the network at that `GET`, it gives each such read its error until its
/// end, and the next read continues it with that `GET` alone; its
/// `comments/` then takes the pages of its comments.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_answer_of_an_item_stopped_by_the_network_gives_its_error_until_its_end() {
    let harness = github_harness();
    let get = api("/repos/octocat/hello/issues/3");
    let comments = "octocat/hello/issues/all/3/comments";
    let (title, title_requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/3/title"),
        vec![empty(502, &[])],
    )
    .await;
    let (listing, listing_requests) =
        answering(&harness, names(&harness, comments), vec![empty(502, &[])]).await;
    for (stopped, requests) in [
        (title.map(drop), title_requests),
        (listing.map(drop), listing_requests),
    ] {
        assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, get);
    }

    let (title, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/3/title"),
        vec![ok(&item(
            3,
            "Three",
            "open",
            "2026-11-18T09:00:00Z",
            1,
            false,
        ))],
    )
    .await;
    assert_eq!(title.unwrap(), "Three");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, get);

    let (listing, requests) = answering(
        &harness,
        names(&harness, comments),
        vec![ok(&json!([comment(30, 3, false, "first")]))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["30"]);
    assert_eq!(
        requests[0].url,
        api("/repos/octocat/hello/issues/3/comments?per_page=100&page=1")
    );
}

/// A read of the `body.md` of a comment by its path alone, as by a filehandle
/// held across a change of the projection, looks up each segment from the
/// root: the item takes its answer, and the comment, which the `comments/` of
/// the item lacks before it is complete, takes the comments of the item
/// first, so that the comment serves.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_of_a_comment_by_its_path_alone_takes_the_comments_of_its_item() {
    let harness = github_harness();
    let comments = "octocat/hello/issues/all/3/comments";
    let (body, requests) = answering(
        &harness,
        read_at(&harness, &format!("{comments}/31/body.md")),
        vec![
            ok(&item(3, "Three", "open", "2026-11-18T09:00:00Z", 2, false)),
            ok(&json!([
                comment(30, 3, false, "first"),
                comment(31, 3, false, "second"),
            ])),
        ],
    )
    .await;
    assert_eq!(body.unwrap(), "second");
    assert_eq!(requests[0].url, api("/repos/octocat/hello/issues/3"));
    assert_eq!(
        requests[1].url,
        api("/repos/octocat/hello/issues/3/comments?per_page=100&page=1")
    );
    assert_eq!(
        served(&harness, names(&harness, comments)).await.unwrap(),
        ["30", "31"]
    );
}

/// An item that an answer of its feed removed is not found within the
/// deadline of that answer, without a request, and is asked for again past
/// it with one `GET` of the item.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_item_is_asked_for_again_only_past_the_deadline_of_its_feed() {
    let harness = github_harness();
    let (title, _) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/8/title"),
        vec![answer(
            200,
            &[("ETag", r#""item-8""#)],
            &item(8, "Eight", "open", "2026-11-18T09:00:00Z", 0, false),
        )],
    )
    .await;
    assert_eq!(title.unwrap(), "Eight");

    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        lookup(&harness, "octocat/hello/issues/all/8"),
        vec![empty(404, &[])],
    )
    .await;
    assert!(found.unwrap().is_missing());
    assert_eq!(
        request_header(&requests[0], "if-none-match"),
        Some(r#""item-8""#)
    );
    for path in ["issues/all/8", "issues/open/8"] {
        assert!(
            served(&harness, lookup(&harness, &format!("octocat/hello/{path}")))
                .await
                .unwrap()
                .is_missing(),
            "{path}"
        );
    }

    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        lookup(&harness, "octocat/hello/issues/all/8"),
        vec![empty(404, &[])],
    )
    .await;
    assert!(found.unwrap().is_missing());
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, api("/repos/octocat/hello/issues/8"));
}

/// An answer of GitHub GraphQL about the repository with `null` for each item
/// it cannot resolve, with an error of the type `NOT_FOUND`.
fn graphql_not_found(repository: &Value) -> HttpResponse {
    ok(&json!({
        "data": { "repository": repository },
        "errors": [{
            "type": "NOT_FOUND",
            "message": "Could not resolve to an issue or pull request.",
        }],
    }))
}

/// A first full listing that stops at a comment by its ID continues from that
/// comment alone at each next read of its folders: the read of a network that
/// fails again takes its error. Meanwhile the `comments/` of the item is not
/// complete, so that a read of it takes its comments whole from the item, and
/// the listing then takes no comment by its ID that this read brought.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_full_listing_stopped_at_a_comment_by_its_id_continues_from_it() {
    let harness = github_harness();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            ok(&json!([item(
                1,
                "One",
                "open",
                "2026-11-18T09:00:00Z",
                3,
                false
            )])),
            ok(&json!([comment(10, 1, false, "first")])),
            graphql(&counted(&[(1, 3)])),
            graphql(&json!({
                "item": ids(
                    &[
                        (10, "2026-11-18T08:00:00Z"),
                        (11, "2026-11-18T08:10:00Z"),
                        (12, "2026-11-18T08:20:00Z"),
                    ],
                    None,
                ),
            })),
            ok(&comment(11, 1, false, "second")),
            empty(502, &[]),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    for (request, id) in [(&requests[4], 11), (&requests[5], 12)] {
        assert_eq!(
            request.url,
            api(&format!("/repos/octocat/hello/issues/comments/{id}"))
        );
    }

    let (stopped, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/pulls/all"),
        vec![empty(502, &[])],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    assert_eq!(
        requests[0].url,
        api("/repos/octocat/hello/issues/comments/12")
    );

    let (comments, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all/1/comments"),
        vec![ok(&json!([
            comment(10, 1, false, "first"),
            comment(11, 1, false, "second"),
            comment(12, 1, false, "third"),
        ]))],
    )
    .await;
    assert_eq!(comments.unwrap(), ["10", "11", "12"]);
    assert_eq!(
        requests[0].url,
        api("/repos/octocat/hello/issues/1/comments?per_page=100&page=1")
    );

    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![graphql(&counted(&[]))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1"]);
    assert_eq!(requests.len(), 1);
    assert!(
        graphql_of(&requests[0])
            .0
            .contains("page: pullRequests(first: 100, after: $after)")
    );
    assert_eq!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/all/1/comments")
        )
        .await
        .unwrap(),
        ["10", "11", "12"]
    );
    assert_eq!(
        served(
            &harness,
            read(&harness, "octocat/hello/issues/all/1/comments/12/body.md")
        )
        .await
        .unwrap(),
        "third"
    );
}

/// A check that stops at a comment by its ID continues from that comment
/// alone at each next read: the read of a network that fails again takes its
/// error. A comment that left its item before its ID was asked, `404`, does
/// not join the item.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_stopped_at_a_comment_by_its_id_continues_from_it() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &json!([item(1, "One", "open", "2026-11-18T09:00:00Z", 1, false)]),
        &json!([comment(10, 1, false, "first")]),
        &[(1, 1)],
        &[],
    )
    .await;

    tokio::time::sleep(PAST_DEADLINE).await;
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            ok(&json!([item(
                1,
                "One again",
                "open",
                "2026-11-18T10:00:00Z",
                4,
                false
            )])),
            ok(&json!([])),
            graphql(&json!({
                "issues": { "totalCount": 1 },
                "pullRequests": { "totalCount": 0 },
                "i1": ids(
                    &[
                        (10, "2026-11-18T08:00:00Z"),
                        (11, "2026-11-18T09:30:00Z"),
                        (12, "2026-11-18T09:40:00Z"),
                        (13, "2026-11-18T09:50:00Z"),
                    ],
                    None,
                ),
            })),
            ok(&comment(11, 1, false, "second")),
            empty(502, &[]),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    for (request, id) in [(&requests[3], 11), (&requests[4], 12)] {
        assert_eq!(
            request.url,
            api(&format!("/repos/octocat/hello/issues/comments/{id}"))
        );
    }

    let (stopped, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/1/title"),
        vec![empty(502, &[])],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    assert_eq!(
        requests[0].url,
        api("/repos/octocat/hello/issues/comments/12")
    );

    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![ok(&comment(12, 1, false, "third")), empty(404, &[])],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1"]);
    for (request, id) in [(&requests[0], 12), (&requests[1], 13)] {
        assert_eq!(
            request.url,
            api(&format!("/repos/octocat/hello/issues/comments/{id}"))
        );
    }
    assert_eq!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/all/1/comments")
        )
        .await
        .unwrap(),
        ["10", "11", "12"]
    );
    for (file, expected) in [
        ("issues/all/1/title", "One again"),
        ("issues/all/1/comments/11/body.md", "second"),
        ("issues/open/1/comments/12/body.md", "third"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &format!("octocat/hello/{file}")))
                .await
                .unwrap(),
            expected,
            "{file}"
        );
    }
}

/// A later answer of an item that stops at a comment by its ID continues from
/// that comment alone at each next read: the read of a network that fails
/// again takes its error.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_answer_of_an_item_stopped_at_a_comment_by_its_id_continues_from_it() {
    let harness = github_harness();
    let (title, _) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/7/title"),
        vec![answer(
            200,
            &[("ETag", r#""item-7""#)],
            &item(7, "Seven", "open", "2026-11-18T09:00:00Z", 0, false),
        )],
    )
    .await;
    assert_eq!(title.unwrap(), "Seven");

    tokio::time::sleep(PAST_DEADLINE).await;
    let (stopped, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/7/title"),
        vec![
            answer(
                200,
                &[("ETag", r#""item-7b""#)],
                &item(7, "Seven again", "open", "2026-11-18T10:00:00Z", 2, false),
            ),
            ok(&json!([])),
            graphql(&json!({
                "item": ids(
                    &[(71, "2026-11-18T09:30:00Z"), (72, "2026-11-18T09:40:00Z")],
                    None,
                ),
            })),
            ok(&comment(71, 7, false, "first")),
            empty(502, &[]),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    for (request, id) in [(&requests[3], 71), (&requests[4], 72)] {
        assert_eq!(
            request.url,
            api(&format!("/repos/octocat/hello/issues/comments/{id}"))
        );
    }

    let (stopped, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/7/title"),
        vec![empty(502, &[])],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    assert_eq!(
        requests[0].url,
        api("/repos/octocat/hello/issues/comments/72")
    );

    let (title, requests) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/7/title"),
        vec![ok(&comment(72, 7, false, "second"))],
    )
    .await;
    assert_eq!(title.unwrap(), "Seven again");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        api("/repos/octocat/hello/issues/comments/72")
    );
    assert_eq!(
        served(
            &harness,
            names(&harness, "octocat/hello/issues/all/7/comments")
        )
        .await
        .unwrap(),
        ["71", "72"]
    );
}

/// The first full listing removes an item with another count than its
/// `comments/` that GitHub GraphQL cannot resolve at its IDs, `null` with an
/// error of the type `NOT_FOUND`.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_full_listing_removes_an_item_that_github_graphql_cannot_resolve() {
    let harness = github_harness();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            ok(&json!([
                item(1, "One", "open", "2026-11-18T08:00:00Z", 2, false),
                item(2, "Two", "open", "2026-11-18T09:00:00Z", 0, false),
            ])),
            ok(&json!([comment(10, 1, false, "first")])),
            graphql(&counted(&[(1, 2), (2, 0)])),
            graphql_not_found(&json!({ "item": null })),
            graphql(&counted(&[])),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["2"]);
    assert_eq!(graphql_of(&requests[3]).1["number"], 1);
    for path in ["issues/all/1", "issues/open/1"] {
        assert!(
            served(&harness, lookup(&harness, &format!("octocat/hello/{path}")))
                .await
                .unwrap()
                .is_missing(),
            "{path}"
        );
    }
}

/// A check removes a changed item that GitHub GraphQL cannot resolve, `null`
/// with an error of the type `NOT_FOUND`, in the `POST` of the counts and at
/// a later page of its IDs, and counts it as removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_removes_a_changed_item_that_github_graphql_cannot_resolve() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &json!([
            item(1, "One", "open", "2026-11-18T08:00:00Z", 1, false),
            item(2, "Two", "open", "2026-11-18T08:30:00Z", 1, false),
            item(3, "Three", "open", "2026-11-18T09:00:00Z", 0, false),
        ]),
        &json!([
            comment(10, 1, false, "first"),
            comment(20, 2, false, "second")
        ]),
        &[(1, 1), (2, 1), (3, 0)],
        &[],
    )
    .await;

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            ok(&json!([
                item(1, "One", "open", "2026-11-18T10:00:00Z", 2, false),
                item(2, "Two", "open", "2026-11-18T10:30:00Z", 2, false),
            ])),
            ok(&json!([])),
            graphql_not_found(&json!({
                "issues": { "totalCount": 1 },
                "pullRequests": { "totalCount": 0 },
                "i1": null,
                "i2": ids(&[(20, "2026-11-18T08:30:00Z")], Some("ids-2")),
            })),
            graphql_not_found(&json!({ "item": null })),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["3"]);
    assert_eq!(requests.len(), 4);
    let (_, variables) = graphql_of(&requests[3]);
    assert_eq!(variables["number"], 2);
    assert_eq!(variables["after"], "ids-2");
    for path in [
        "issues/all/1",
        "issues/open/1",
        "issues/all/2",
        "issues/open/2",
    ] {
        assert!(
            served(&harness, lookup(&harness, &format!("octocat/hello/{path}")))
                .await
                .unwrap()
                .is_missing(),
            "{path}"
        );
    }
}

/// A later answer of an item removes it when GitHub GraphQL cannot resolve it
/// at its IDs, `null` with an error of the type `NOT_FOUND`.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_answer_of_an_item_removes_it_when_github_graphql_cannot_resolve_it() {
    let harness = github_harness();
    let (title, _) = answering(
        &harness,
        read(&harness, "octocat/hello/issues/all/8/title"),
        vec![answer(
            200,
            &[("ETag", r#""item-8""#)],
            &item(8, "Eight", "open", "2026-11-18T09:00:00Z", 0, false),
        )],
    )
    .await;
    assert_eq!(title.unwrap(), "Eight");

    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        lookup(&harness, "octocat/hello/issues/all/8"),
        vec![
            answer(
                200,
                &[("ETag", r#""item-8b""#)],
                &item(8, "Eight again", "open", "2026-11-18T10:00:00Z", 1, false),
            ),
            ok(&json!([])),
            graphql_not_found(&json!({ "item": null })),
        ],
    )
    .await;
    assert!(found.unwrap().is_missing());
    assert_eq!(
        request_header(&requests[0], "if-none-match"),
        Some(r#""item-8""#)
    );
    assert_eq!(graphql_of(&requests[2]).1["number"], 8);
}

/// A rate limit of GitHub whose window does not parse fails as an invalid
/// answer, at the REST API and at GitHub GraphQL, and one whose reset passed
/// has a window of zero.
#[tokio::test(flavor = "multi_thread")]
async fn a_rate_limit_whose_window_does_not_parse_fails_as_an_invalid_answer() {
    let harness = github_harness();
    let (stopped, _) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![empty(
            403,
            &[("x-ratelimit-remaining", "0"), ("retry-after", "soon")],
        )],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Invalid)), "{stopped:?}");

    let rate_limited = |reset: &str| {
        answer(
            200,
            &[("x-ratelimit-reset", reset)],
            &json!({
                "data": null,
                "errors": [{ "type": "RATE_LIMITED", "message": "API rate limit exceeded" }],
            }),
        )
    };
    let (stopped, _) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![
            ok(&json!([item(
                1,
                "One",
                "open",
                "2026-11-18T09:00:00Z",
                0,
                false
            )])),
            ok(&json!([])),
            rate_limited("later"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Invalid)), "{stopped:?}");

    let passed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .checked_sub(Duration::from_secs(60))
        .unwrap()
        .as_secs()
        .to_string();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "octocat/hello/issues/all"),
        vec![rate_limited(&passed)],
    )
    .await;
    assert_eq!(
        stopped,
        Err(NsError::RateLimited {
            retry_after: Some(Duration::ZERO)
        })
    );
    graphql_of(&requests[0]);
}

/// `open/` of a mount without its provider serves the facts of `all/` from
/// the projection.
#[tokio::test(flavor = "multi_thread")]
async fn open_of_a_mount_without_its_provider_serves_the_facts_of_all() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &json!([item(1, "One", "open", "2026-11-18T09:00:00Z", 1, false)]),
        &json!([comment(10, 1, false, "first")]),
        &[(1, 1)],
        &[],
    )
    .await;

    let namespace = harness.without_providers().unwrap();
    let path = |path: &'static str| {
        let namespace = Arc::clone(&namespace);
        async move {
            let mut answer = namespace.lookup(Path::root(), "github").await.unwrap();
            for segment in path.split('/') {
                answer = namespace.lookup(answer.path, segment).await.unwrap();
            }
            answer.path
        }
    };
    let title = namespace
        .read(path("octocat/hello/issues/open/1/title").await, 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(title.bytes, b"One");
    let comments = namespace
        .readdir(
            path("octocat/hello/issues/open/1/comments").await,
            DirCursor::start(),
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        comments
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["10"]
    );
}
