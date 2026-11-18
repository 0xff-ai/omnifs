//! Namespace-visible object feeds: the test provider's shelf feed answers for
//! the books of a shelf, the book feed for a title before the first answer of
//! its shelf ends.

use std::time::Duration;

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, Namespace, NsError};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};

const CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test"}"#;

/// Past the deadline of a feed, `DYNAMIC_TTL_MILLIS` after its last answer.
const PAST_DEADLINE: Duration = Duration::from_millis(3_200);

async fn path(harness: &RuntimeHarness, segments: &[&str]) -> Path {
    let mut path = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap()
        .path;
    for segment in segments {
        path = harness.namespace.lookup(path, segment).await.unwrap().path;
    }
    path
}

async fn names(harness: &RuntimeHarness, segments: &[&str]) -> Result<Vec<String>, NsError> {
    let dir = path(harness, segments).await;
    let page = harness
        .namespace
        .readdir(dir, DirCursor::start(), 0)
        .await?;
    Ok(page.entries.into_iter().map(|entry| entry.name).collect())
}

async fn read(harness: &RuntimeHarness, segments: &[&str]) -> String {
    let file = path(harness, segments).await;
    let read = harness.namespace.read(file, 0, u32::MAX).await.unwrap();
    String::from_utf8(read.bytes).unwrap()
}

async fn log(harness: &RuntimeHarness) -> Vec<String> {
    read(harness, &["hello", "shelf-log"])
        .await
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_feed_starts_from_its_starting_path_and_answers_once_past_its_deadline() {
    let harness = make_initialized_runtime(CONFIG);

    // The first listing of `books` starts the shelf feed, whose first
    // answer publishes its three parts before the listing serves.
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "2"]
    );
    assert_eq!(
        log(&harness).await,
        ["oak - books", "oak page-1 books", "oak page-2 books"]
    );

    // Fed facts answer filesystems with the zero lifetime of dynamic entries.
    let shelf = path(&harness, &["shelves", "oak"]).await;
    for (parent, name) in [
        (shelf.clone(), "books"),
        (path(&harness, &["shelves", "oak", "books"]).await, "1"),
        (
            path(&harness, &["shelves", "oak", "books", "1"]).await,
            "title",
        ),
    ] {
        let lookup = harness.namespace.lookup(parent, name).await.unwrap();
        assert_eq!(lookup.attrs().unwrap().ttl, Duration::ZERO, "{name}");
    }
    assert_eq!(
        read(&harness, &["shelves", "oak", "books", "1", "title"]).await,
        "Book 1"
    );
    // A fed anchor lists its faces.
    assert_eq!(
        names(&harness, &["shelves", "oak", "books", "1"])
            .await
            .unwrap(),
        ["book.json", "title"]
    );

    // Within its deadline the feed is not asked again; past it, an answer
    // with its token stands.
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "2"]
    );
    assert_eq!(
        log(&harness).await[3..],
        ["oak version-0 -"],
        "one answer past the deadline, from the token of the first answer"
    );

    // An upstream change is taken by the next answer past the deadline: the
    // host gave the feed its listing of `books`, and the part renames, adds,
    // and removes books.
    read(&harness, &["hello", "shelf-change"]).await;
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "2"]
    );
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "3"]
    );
    assert_eq!(read(&harness, &["shelves", "oak", "seen"]).await, "1,2");
    assert_eq!(
        read(&harness, &["shelves", "oak", "books", "1", "title"]).await,
        "Book 1 v1"
    );
    let books = path(&harness, &["shelves", "oak", "books"]).await;
    assert!(
        harness
            .namespace
            .lookup(books, "2")
            .await
            .unwrap()
            .is_missing()
    );
    assert_eq!(
        log(&harness).await[3..],
        ["oak version-0 -", "oak version-0 -"],
        "the change answered the next call past the deadline"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_feed_keeps_its_token_and_its_inner_states_answer_meanwhile() {
    let harness = make_initialized_runtime(CONFIG);

    // The first answer stops after its first part: the listing that started
    // it fails with the error of the stop.
    assert!(matches!(
        names(&harness, &["shelves", "flaky", "books"]).await,
        Err(NsError::Network)
    ));

    // The book of the published part serves from the projection, through
    // the inner state its row gave it, without a call of its feed.
    assert_eq!(
        read(&harness, &["shelves", "flaky", "books", "1", "title"]).await,
        "Book 1"
    );
    assert_eq!(log(&harness).await, ["flaky - books", "flaky page-1 books"]);

    // Past the deadline of that inner state, before its shelf feed ended,
    // the book's own feed answers for its title from the inner token.
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        read(&harness, &["shelves", "flaky", "books", "1", "title"]).await,
        "Book 1 (own)"
    );
    assert_eq!(log(&harness).await.last().unwrap(), "flaky/1 row-1 -");

    // The next listing of `books` continues the first answer from the token
    // of its last published part.
    assert_eq!(
        names(&harness, &["shelves", "flaky", "books"])
            .await
            .unwrap(),
        ["1", "2"]
    );
    assert_eq!(
        log(&harness).await[3..],
        ["flaky page-1 books", "flaky page-2 books"]
    );
}
