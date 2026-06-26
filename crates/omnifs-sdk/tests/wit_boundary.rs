//! WIT-boundary tests for the provider object SDK.
//!
//! Each test builds a fixture provider's `Router<()>`, drives a browse method
//! (`lookup_child` / `list_children` / `read_file` / `open_file`) through the
//! callout suspend/resume loop, lowers the SDK result via its public
//! `into_*_and_effects()` / `into_wit()`, and asserts on the exact WIT records.
//! These assertions encode the CORRECT behavior the SDK must exhibit at the
//! host boundary; they are the contract the three fixes restore.

// Fixture shapes are dictated by SDK trait signatures: a derive fn must return
// `Result<FileProjection>` (the `DeriveFn` contract) even when infallible, and
// `Object::load` returns an `impl Future` written as a manual async block.
#![allow(clippy::unnecessary_wraps, clippy::manual_async_fn)]

use omnifs_sdk::__wit::provider::types as wit_types;
use omnifs_sdk::browse::{CachedCanonical, List, ReadOutcome};
use omnifs_sdk::captures::{Captures, FromCaptures};
use omnifs_sdk::collection::{Collection, CollectionEntry, ListCx, NoCursor, PageCursor};
use omnifs_sdk::cx::Cx;
use omnifs_sdk::error::{ProviderError, Result};
use omnifs_sdk::file_attrs::{Size, Stability};
use omnifs_sdk::handler::MemoryRangeReader;
use omnifs_sdk::identity::{Facet, IdentityCaptures};
use omnifs_sdk::object::{
    Canonical, FacetAxis, FacetMetadata, Key, Load, Object, ObjectEntry, ObjectKind, Validator,
};
use omnifs_sdk::projection::{FileProjection, StreamFile};
use omnifs_sdk::repr::{Json, Markdown, Representable};
use omnifs_sdk::router::Router;
use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::rc::Rc;
use std::str::FromStr;
use std::task::{Context, Poll, Waker};

// ===========================================================================
// Drive loop
// ===========================================================================

/// Poll a future to completion, draining yielded callouts and pushing canned
/// HTTP results back so a handler that issues a callout resumes. Fixtures whose
/// loads are synchronous never yield, so the loop completes on the first poll.
fn drive<T>(cx: &Cx<()>, fut: impl Future<Output = Result<T>>) -> T {
    let mut fut = Box::pin(fut);
    let waker = Waker::noop();
    let mut ctx = Context::from_waker(waker);
    loop {
        match fut.as_mut().poll(&mut ctx) {
            Poll::Ready(result) => return result.expect("handler returned an error"),
            Poll::Pending => {
                let yielded = cx.take_yielded_callouts();
                assert!(
                    !yielded.is_empty(),
                    "future is pending but yielded no callouts: deadlock"
                );
                for callout in yielded {
                    let result = match callout {
                        wit_types::Callout::GitOpenRepo(_) => {
                            wit_types::CalloutResult::GitRepoOpened(wit_types::GitRepoInfo {
                                repo: 7,
                                tree: 7,
                            })
                        },
                        wit_types::Callout::OpenArchive(_) => {
                            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened {
                                tree: 7,
                            })
                        },
                        _ => wit_types::CalloutResult::HttpResponse(wit_types::HttpResponse {
                            status: 200,
                            headers: Vec::new(),
                            body: b"callout-body".to_vec(),
                        }),
                    };
                    cx.push_delivered(result);
                }
            },
        }
    }
}

/// Like [`drive`] but returns the handler's `Result` instead of panicking on
/// an error, for tests that assert a path is rejected.
fn drive_result<T>(cx: &Cx<()>, fut: impl Future<Output = Result<T>>) -> Result<T> {
    let mut fut = Box::pin(fut);
    let waker = Waker::noop();
    let mut ctx = Context::from_waker(waker);
    loop {
        match fut.as_mut().poll(&mut ctx) {
            Poll::Ready(result) => return result,
            Poll::Pending => {
                let yielded = cx.take_yielded_callouts();
                assert!(
                    !yielded.is_empty(),
                    "future is pending but yielded no callouts: deadlock"
                );
                for _callout in yielded {
                    cx.push_delivered(wit_types::CalloutResult::HttpResponse(
                        wit_types::HttpResponse {
                            status: 200,
                            headers: Vec::new(),
                            body: b"callout-body".to_vec(),
                        },
                    ));
                }
            },
        }
    }
}

fn cx() -> Cx<()> {
    Cx::new(1, Rc::new(RefCell::new(())))
}

/// Lower a read outcome to the WIT record plus the effects.
fn read_wit(outcome: ReadOutcome) -> (wit_types::ReadFileOutcome, wit_types::Effects) {
    let (out, effects) = outcome.into_result_and_effects();
    (out, effects.into_wit())
}

fn list_wit(list: List) -> (wit_types::ListChildrenResult, wit_types::Effects) {
    let (out, effects) = list.into_result_and_effects();
    (out, effects.into_wit())
}

fn found(outcome: &wit_types::ReadFileOutcome) -> &wit_types::ReadFileResult {
    match outcome {
        wit_types::ReadFileOutcome::Found(result) => result,
        wit_types::ReadFileOutcome::NotFound(_) => panic!("expected a found read outcome"),
    }
}

fn canonical_with_leaf<'a>(
    effects: &'a wit_types::Effects,
    leaf: &str,
) -> &'a wit_types::CanonicalStore {
    effects
        .canonical
        .iter()
        .find(|store| store.view_leaves.iter().any(|l| l == leaf))
        .unwrap_or_else(|| panic!("no canonical store has view leaf {leaf:?}"))
}

// ===========================================================================
// Item object: canonical JSON + markdown representation + a derive leaf
// ===========================================================================

#[derive(serde::Serialize, serde::Deserialize)]
struct Item {
    title: String,
}

struct ItemKey {
    id: String,
}

impl FromCaptures for ItemKey {
    fn from_captures(c: &Captures) -> Result<Self> {
        Ok(Self {
            id: c
                .get("id")
                .ok_or_else(|| ProviderError::invalid_input("missing id"))?
                .to_string(),
        })
    }
}
impl IdentityCaptures for ItemKey {
    fn identity_captures(&self) -> Vec<(&'static str, String)> {
        vec![("id", self.id.clone())]
    }
}
impl FacetMetadata for ItemKey {
    fn facet_axes() -> &'static [FacetAxis] {
        &[]
    }
}
impl Key for ItemKey {}

impl Object for Item {
    type Key = ItemKey;
    type State = ();
    type Canonical = Json;
    fn load(
        _cx: &Cx<()>,
        key: &ItemKey,
        _since: Option<Validator>,
    ) -> impl Future<Output = Result<Load<Self>>> {
        let id = key.id.clone();
        async move {
            Ok(Load::fresh(
                Item { title: id.clone() },
                Canonical::new(format!(r#"{{"title":"{id}"}}"#).into_bytes(), None),
            ))
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        omnifs_sdk::object::decode_json(bytes)
    }
    fn kind() -> ObjectKind {
        ObjectKind("test.item")
    }
}

impl Representable<Markdown> for Item {
    fn represent(&self) -> Vec<u8> {
        format!("# {}", self.title).into_bytes()
    }
}

impl Item {
    fn notes(&self, _key: &ItemKey) -> Result<FileProjection> {
        // Distinct bytes from the markdown render so a misroute is observable.
        Ok(FileProjection::body(format!("NOTES for {}", self.title)).build())
    }
}

fn item_router() -> Router<()> {
    let mut r = Router::<()>::new();
    r.object::<Item>("/items/{id}", |o| {
        o.dynamic();
        o.file("item.json").canonical::<Json>()?;
        o.file("item.md").representation::<Markdown>()?;
        // A DERIVE leaf whose name shares the `.md` extension with the
        // representation: it must route to the derive fn, not the render.
        o.file("notes.md").derive(Item::notes)?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();
    r
}

#[test]
fn object_fresh_read_emits_canonical_store_and_found() {
    let r = item_router();
    let cx = cx();
    let outcome = drive(&cx, r.read_file(&cx, "/items/x/item.json", "", None));
    let (out, effects) = read_wit(outcome);

    // ReadFileOutcome::Found with ByteSource::Canonical.
    let result = found(&out);
    assert!(
        matches!(result.bytes, wit_types::ByteSource::Canonical),
        "the canonical leaf serves byte-source::canonical, got {:?}",
        result.bytes
    );
    // A canonical store whose view leaves include the read path.
    let store = canonical_with_leaf(&effects, "/items/x/item.json");
    assert_eq!(store.id.kind, "test.item");
    assert!(
        store.view_leaves.iter().any(|l| l == "/items/x/item.md"),
        "view leaves cover the markdown representation too: {:?}",
        store.view_leaves
    );
}

#[test]
fn warm_read_renders_from_pushed_bytes_no_canonical_effect() {
    let r = item_router();
    let cx = cx();
    // Build the host-pushed canonical for the same anchor id.
    let id = wit_types::LogicalId {
        kind: "test.item".into(),
        captures: vec![wit_types::IdCapture {
            name: "id".into(),
            value: "x".into(),
        }],
    };
    let cached = CachedCanonical::from_wit(wit_types::CanonicalInput {
        id,
        validator: None,
        bytes: br#"{"title":"warm"}"#.to_vec(),
    });
    let outcome = drive(&cx, r.read_file(&cx, "/items/x/item.md", "", Some(cached)));
    let (out, effects) = read_wit(outcome);

    assert!(
        effects.canonical.is_empty(),
        "a warm read emits NO canonical store: {:?}",
        effects.canonical
    );
    let result = found(&out);
    let wit_types::ByteSource::Inline(bytes) = &result.bytes else {
        panic!(
            "warm markdown render is inline bytes, got {:?}",
            result.bytes
        );
    };
    assert_eq!(bytes, b"# warm", "rendered from the pushed canonical bytes");
}

#[test]
fn derive_leaf_sharing_md_extension_runs_derive_not_representation() {
    let r = item_router();
    let cx = cx();
    let outcome = drive(&cx, r.read_file(&cx, "/items/x/notes.md", "", None));
    let (out, _effects) = read_wit(outcome);
    let result = found(&out);
    let wit_types::ByteSource::Inline(bytes) = &result.bytes else {
        panic!("derive leaf serves inline bytes, got {:?}", result.bytes);
    };
    assert_eq!(
        bytes, b"NOTES for x",
        "notes.md must run the derive fn, not the markdown render (#3, MAJOR fix)"
    );
}

// ===========================================================================
// File-object + preload_object (Oura-shaped)
// ===========================================================================

#[derive(serde::Serialize, serde::Deserialize)]
struct Day {
    day: String,
}

struct DayKey {
    day: String,
    collection: String,
}

impl FromCaptures for DayKey {
    fn from_captures(c: &Captures) -> Result<Self> {
        Ok(Self {
            day: c
                .get("day")
                .ok_or_else(|| ProviderError::invalid_input("missing day"))?
                .to_string(),
            collection: c
                .get("collection")
                .ok_or_else(|| ProviderError::invalid_input("missing collection"))?
                .to_string(),
        })
    }
}
impl IdentityCaptures for DayKey {
    fn identity_captures(&self) -> Vec<(&'static str, String)> {
        vec![
            ("day", self.day.clone()),
            ("collection", self.collection.clone()),
        ]
    }
}
impl FacetMetadata for DayKey {
    fn facet_axes() -> &'static [FacetAxis] {
        &[]
    }
}
impl Key for DayKey {}

impl Object for Day {
    type Key = DayKey;
    type State = ();
    type Canonical = Json;
    fn load(
        _cx: &Cx<()>,
        key: &DayKey,
        _since: Option<Validator>,
    ) -> impl Future<Output = Result<Load<Self>>> {
        let day = key.day.clone();
        let collection = key.collection.clone();
        async move {
            // The requested day plus its sibling day, both from one fetch.
            let sibling_day = "2024-01-02";
            let sibling = ObjectEntry::fresh(
                DayKey {
                    day: sibling_day.into(),
                    collection: collection.clone(),
                },
                Canonical::new(format!(r#"{{"day":"{sibling_day}"}}"#).into_bytes(), None),
            );
            Ok(Load::fresh(
                Day { day: day.clone() },
                Canonical::new(format!(r#"{{"day":"{day}"}}"#).into_bytes(), None),
            )
            .preload_object(sibling))
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        omnifs_sdk::object::decode_json(bytes)
    }
    fn kind() -> ObjectKind {
        ObjectKind("test.day")
    }
}

fn day_router() -> Router<()> {
    let mut r = Router::<()>::new();
    r.file_object::<Day>("/{day}/{collection}", |o| {
        o.dynamic();
        // The file-object anchor IS this file: declare the single canonical
        // face directly on the block (no leaf name).
        o.canonical::<Json>()?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();
    r
}

#[test]
fn file_object_preload_object_keeps_full_sibling_anchor_path() {
    let r = day_router();
    let cx = cx();
    let outcome = drive(&cx, r.read_file(&cx, "/2024-01-01/sleep", "", None));
    let (_out, effects) = read_wit(outcome);

    // The requested day's canonical, anchored at the read path itself.
    let requested = canonical_with_leaf(&effects, "/2024-01-01/sleep");
    assert_eq!(requested.id.kind, "test.day");

    // The sibling day's canonical, anchored at the FULL sibling path
    // "/2024-01-02/sleep" (NOT a truncated "/2024-01-02"). This is the
    // blocker FIX 2 restores: a file-object anchor is the read path, so the
    // anchor base must not strip the last segment.
    let sibling = canonical_with_leaf(&effects, "/2024-01-02/sleep");
    assert_eq!(
        sibling.view_leaves,
        vec!["/2024-01-02/sleep".to_string()],
        "sibling view leaves must be the full sibling anchor, not truncated"
    );
    assert!(
        !effects
            .canonical
            .iter()
            .any(|s| s.view_leaves.iter().any(|l| l == "/2024-01-02")),
        "no canonical may be anchored at the truncated path /2024-01-02"
    );
}

// ===========================================================================
// Nested collection (parent Repo -> issues/{filter}/{number} child Issue)
// ===========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    Open,
    All,
}
impl FromStr for Filter {
    type Err = ProviderError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "all" => Ok(Self::All),
            other => Err(ProviderError::invalid_input(format!("bad filter {other}"))),
        }
    }
}
impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::All => write!(f, "all"),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Repo {
    name: String,
}
struct RepoKey {
    owner: String,
    repo: String,
}
impl FromCaptures for RepoKey {
    fn from_captures(c: &Captures) -> Result<Self> {
        Ok(Self {
            owner: c
                .get("owner")
                .ok_or_else(|| ProviderError::invalid_input("missing owner"))?
                .to_string(),
            repo: c
                .get("repo")
                .ok_or_else(|| ProviderError::invalid_input("missing repo"))?
                .to_string(),
        })
    }
}
impl IdentityCaptures for RepoKey {
    fn identity_captures(&self) -> Vec<(&'static str, String)> {
        vec![("owner", self.owner.clone()), ("repo", self.repo.clone())]
    }
}
impl FacetMetadata for RepoKey {
    fn facet_axes() -> &'static [FacetAxis] {
        &[]
    }
}
impl Key for RepoKey {}
impl Object for Repo {
    type Key = RepoKey;
    type State = ();
    type Canonical = Json;
    fn load(
        _cx: &Cx<()>,
        key: &RepoKey,
        _since: Option<Validator>,
    ) -> impl Future<Output = Result<Load<Self>>> {
        let repo = key.repo.clone();
        async move {
            Ok(Load::fresh(
                Repo { name: repo.clone() },
                Canonical::new(format!(r#"{{"name":"{repo}"}}"#).into_bytes(), None),
            ))
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        omnifs_sdk::object::decode_json(bytes)
    }
    fn kind() -> ObjectKind {
        ObjectKind("test.repo")
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Issue {
    number: String,
}
struct IssueKey {
    owner: String,
    repo: String,
    number: String,
    #[allow(dead_code)]
    filter: Facet<Filter>,
}
impl FromCaptures for IssueKey {
    fn from_captures(c: &Captures) -> Result<Self> {
        Ok(Self {
            owner: c
                .get("owner")
                .ok_or_else(|| ProviderError::invalid_input("missing owner"))?
                .to_string(),
            repo: c
                .get("repo")
                .ok_or_else(|| ProviderError::invalid_input("missing repo"))?
                .to_string(),
            number: c
                .get("number")
                .ok_or_else(|| ProviderError::invalid_input("missing number"))?
                .to_string(),
            filter: Facet(
                c.get("filter")
                    .ok_or_else(|| ProviderError::invalid_input("missing filter"))?
                    .parse()?,
            ),
        })
    }
}
impl IdentityCaptures for IssueKey {
    fn identity_captures(&self) -> Vec<(&'static str, String)> {
        vec![
            ("owner", self.owner.clone()),
            ("repo", self.repo.clone()),
            ("number", self.number.clone()),
        ]
    }
}
impl FacetMetadata for IssueKey {
    fn facet_axes() -> &'static [FacetAxis] {
        static AXES: [FacetAxis; 1] = [FacetAxis {
            capture_name: "filter",
            choices: &["open", "all"],
        }];
        &AXES
    }
}
impl Key for IssueKey {}
impl Object for Issue {
    type Key = IssueKey;
    type State = ();
    type Canonical = Json;
    fn load(
        _cx: &Cx<()>,
        key: &IssueKey,
        _since: Option<Validator>,
    ) -> impl Future<Output = Result<Load<Self>>> {
        let number = key.number.clone();
        async move {
            Ok(Load::fresh(
                Issue {
                    number: number.clone(),
                },
                Canonical::new(format!(r#"{{"number":"{number}"}}"#).into_bytes(), None),
            ))
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        omnifs_sdk::object::decode_json(bytes)
    }
    fn kind() -> ObjectKind {
        ObjectKind("test.issue")
    }
}

impl Representable<Markdown> for Issue {
    fn represent(&self) -> Vec<u8> {
        format!("# issue {}", self.number).into_bytes()
    }
}
impl Issue {
    fn title(&self, _key: &IssueKey) -> Result<FileProjection> {
        Ok(FileProjection::inline(format!("title {}", self.number)).build())
    }
}

/// The collection-dir key for `issues/{filter}`: it carries the `{filter}`
/// sub-capture (which `RepoKey` does not), so the list method can read it.
struct IssueListKey {
    owner: String,
    repo: String,
    filter: Filter,
}
impl FromCaptures for IssueListKey {
    fn from_captures(c: &Captures) -> Result<Self> {
        Ok(Self {
            owner: c
                .get("owner")
                .ok_or_else(|| ProviderError::invalid_input("missing owner"))?
                .to_string(),
            repo: c
                .get("repo")
                .ok_or_else(|| ProviderError::invalid_input("missing repo"))?
                .to_string(),
            filter: c
                .get("filter")
                .ok_or_else(|| ProviderError::invalid_input("missing filter"))?
                .parse()?,
        })
    }
}

async fn list_issues(
    key: IssueListKey,
    _cx: ListCx<NoCursor, ()>,
) -> Result<Collection<Issue, NoCursor>> {
    // The listed child number varies by the {filter} sub-capture, proving the
    // method reads the collection-dir captures, not the parent anchor key.
    let number = match key.filter {
        Filter::Open => "7",
        Filter::All => "9",
    };
    Ok(Collection::complete([CollectionEntry::fresh(
        IssueKey {
            owner: key.owner.clone(),
            repo: key.repo.clone(),
            number: number.into(),
            filter: Facet(key.filter),
        },
        Issue {
            number: number.into(),
        },
        Canonical::new(format!(r#"{{"number":"{number}"}}"#).into_bytes(), None),
    )]))
}

fn nested_collection_router() -> Router<()> {
    let mut r = Router::<()>::new();
    r.object::<Repo>("/{owner}/{repo}", |o| {
        o.dynamic();
        o.file("repo.json").canonical::<Json>()?;
        o.dir("issues/{filter}").collection(list_issues)?;
        Ok(())
    })
    .unwrap();
    r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
        o.dynamic();
        o.file("item.json").canonical::<Json>()?;
        o.file("item.md").representation::<Markdown>()?;
        o.file("title.txt").derive(Issue::title)?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();
    r
}

#[test]
fn nested_collection_stores_child_view_leaves_facet_expanded() {
    let r = nested_collection_router();
    let cx = cx();
    let list = drive(&cx, r.list_children(&cx, "/o/r/issues/open", None, None));
    let (out, effects) = list_wit(list);

    let wit_types::ListChildrenResult::Entries(listing) = out else {
        panic!("collection lists entries");
    };
    let dir7 = listing
        .entries
        .iter()
        .find(|e| e.name == "7")
        .expect("listing has a dir entry named 7");
    assert!(matches!(dir7.kind, wit_types::EntryKind::Directory));

    // The canonical store for issue 7 must carry the CHILD's canonical-view
    // leaf paths, facet-expanded across {filter} -> {open, all}, NOT the bare
    // child dir "/o/r/issues/open/7".
    let store = effects
        .canonical
        .iter()
        .find(|s| s.id.kind == "test.issue")
        .expect("a canonical store for the issue child");
    let mut leaves = store.view_leaves.clone();
    leaves.sort();
    let mut expected = vec![
        "/o/r/issues/open/7/item.json".to_string(),
        "/o/r/issues/all/7/item.json".to_string(),
        "/o/r/issues/open/7/item.md".to_string(),
        "/o/r/issues/all/7/item.md".to_string(),
        "/o/r/issues/open/7/title.txt".to_string(),
        "/o/r/issues/all/7/title.txt".to_string(),
    ];
    expected.sort();
    assert_eq!(
        leaves, expected,
        "child view leaves must be the child's canonical-view leaves, facet-expanded (#5, BLOCKER)"
    );
    assert!(
        !store.view_leaves.iter().any(|l| l == "/o/r/issues/open/7"),
        "the bare child dir must NOT be a view leaf"
    );
}

// ===========================================================================
// Anchor collection (parent Owner anchor == child Repo template)
// ===========================================================================

#[derive(serde::Serialize, serde::Deserialize)]
struct Owner {
    name: String,
}
struct OwnerKey {
    owner: String,
}
impl FromCaptures for OwnerKey {
    fn from_captures(c: &Captures) -> Result<Self> {
        Ok(Self {
            owner: c
                .get("owner")
                .ok_or_else(|| ProviderError::invalid_input("missing owner"))?
                .to_string(),
        })
    }
}
impl IdentityCaptures for OwnerKey {
    fn identity_captures(&self) -> Vec<(&'static str, String)> {
        vec![("owner", self.owner.clone())]
    }
}
impl FacetMetadata for OwnerKey {
    fn facet_axes() -> &'static [FacetAxis] {
        &[]
    }
}
impl Key for OwnerKey {}
impl Object for Owner {
    type Key = OwnerKey;
    type State = ();
    type Canonical = Json;
    fn load(
        _cx: &Cx<()>,
        key: &OwnerKey,
        _since: Option<Validator>,
    ) -> impl Future<Output = Result<Load<Self>>> {
        let owner = key.owner.clone();
        async move {
            Ok(Load::fresh(
                Owner {
                    name: owner.clone(),
                },
                Canonical::new(format!(r#"{{"name":"{owner}"}}"#).into_bytes(), None),
            ))
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        omnifs_sdk::object::decode_json(bytes)
    }
    fn kind() -> ObjectKind {
        ObjectKind("test.owner")
    }
}

async fn list_repos(
    key: OwnerKey,
    _cx: ListCx<NoCursor, ()>,
) -> Result<Collection<Repo, NoCursor>> {
    Ok(Collection::complete([CollectionEntry::fresh(
        RepoKey {
            owner: key.owner.clone(),
            repo: "gvfs".into(),
        },
        Repo {
            name: "gvfs".into(),
        },
        Canonical::new(br#"{"name":"gvfs"}"#.to_vec(), None),
    )]))
}

fn anchor_collection_router() -> Router<()> {
    let mut r = Router::<()>::new();
    r.object::<Owner>("/{owner}", |o| {
        o.dynamic();
        o.file("owner.json").canonical::<Json>()?;
        o.dir("{repo}").collection(list_repos)?;
        Ok(())
    })
    .unwrap();
    r.object::<Repo>("/{owner}/{repo}", |o| {
        o.dynamic();
        o.file("repo.json").canonical::<Json>()?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();
    r
}

#[test]
fn anchor_collection_seals_and_merges_repo_names_with_owner_faces() {
    let r = anchor_collection_router();
    let cx = cx();
    let list = drive(&cx, r.list_children(&cx, "/o", None, None));
    let (out, effects) = list_wit(list);

    let wit_types::ListChildrenResult::Entries(listing) = out else {
        panic!("owner anchor lists entries");
    };
    let mut names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["gvfs", "owner.json"],
        "owner anchor merges the repo collection names with its own faces"
    );

    // The fresh repo entry's canonical store carries the child's view leaf
    // "/o/gvfs/repo.json".
    let store = effects
        .canonical
        .iter()
        .find(|s| s.id.kind == "test.repo")
        .expect("a canonical store for the repo child");
    assert_eq!(
        store.view_leaves,
        vec!["/o/gvfs/repo.json".to_string()],
        "anchor-collection child view leaf is the child's canonical leaf path"
    );
}

async fn list_repos_paged(
    key: OwnerKey,
    cx: ListCx<PageCursor, ()>,
) -> Result<Collection<Repo, PageCursor>> {
    let page = cx.cursor().map_or(0, |c| c.0);
    Ok(Collection::page([CollectionEntry::fresh(
        RepoKey {
            owner: key.owner.clone(),
            repo: format!("repo{page}"),
        },
        Repo {
            name: format!("repo{page}"),
        },
        Canonical::new(format!(r#"{{"name":"repo{page}"}}"#).into_bytes(), None),
    )])
    .next(PageCursor(page + 1)))
}

#[test]
fn anchor_collection_page_is_partial_and_carries_cursor() {
    let mut r = Router::<()>::new();
    r.object::<Owner>("/{owner}", |o| {
        o.dynamic();
        o.file("owner.json").canonical::<Json>()?;
        o.dir("{repo}").collection(list_repos_paged)?;
        Ok(())
    })
    .unwrap();
    r.object::<Repo>("/{owner}/{repo}", |o| {
        o.dynamic();
        o.file("repo.json").canonical::<Json>()?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();

    let cx = cx();
    let list = drive(&cx, r.list_children(&cx, "/o", None, None));
    let (out, _effects) = list_wit(list);
    let wit_types::ListChildrenResult::Entries(listing) = out else {
        panic!("owner anchor lists entries");
    };
    assert!(
        !listing.exhaustive,
        "a paged anchor collection makes the parent listing non-exhaustive, not falsely complete"
    );
    let Some(wit_types::Cursor::Opaque(token)) = &listing.next_cursor else {
        panic!(
            "the anchor collection's resume cursor must be carried through, got {:?}",
            listing.next_cursor
        );
    };
    assert_eq!(
        token, "1",
        "the typed page cursor round-trips through the wire"
    );
}

// ===========================================================================
// Direct face (callout)
// ===========================================================================

async fn item_live(cx: Cx<()>, key: ItemKey) -> Result<FileProjection> {
    // Issue a callout so the drive loop must suspend and resume.
    let resp = cx.http().get("https://example/").send().await?;
    let body = resp.into_body();
    Ok(FileProjection::body(format!("live:{}:{}", key.id, body.len())).build())
}

#[test]
fn direct_face_runs_handler_and_returns_inline_no_canonical() {
    let mut r = Router::<()>::new();
    r.object::<Item>("/items/{id}", |o| {
        o.dynamic();
        o.file("item.json").canonical::<Json>()?;
        o.file("live").direct(item_live)?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();

    let cx = cx();
    let outcome = drive(&cx, r.read_file(&cx, "/items/42/live", "", None));
    let (out, effects) = read_wit(outcome);
    assert!(
        effects.canonical.is_empty(),
        "a direct face emits no canonical store"
    );
    let result = found(&out);
    let wit_types::ByteSource::Inline(bytes) = &result.bytes else {
        panic!("direct face returns inline bytes, got {:?}", result.bytes);
    };
    // "callout-body" is 12 bytes.
    assert_eq!(bytes, b"live:42:12");
}

// ===========================================================================
// Registration / seal errors
// ===========================================================================

#[test]
fn collection_of_unregistered_child_kind_fails_seal() {
    let mut r = Router::<()>::new();
    r.object::<Repo>("/{owner}/{repo}", |o| {
        o.dynamic();
        o.file("repo.json").canonical::<Json>()?;
        o.dir("issues/{filter}").collection(list_issues)?;
        Ok(())
    })
    .unwrap();
    // Issue is never registered as its own object route.
    let err = r.seal().unwrap_err();
    assert_eq!(
        err.kind(),
        omnifs_sdk::error::ProviderErrorKind::InvalidInput,
        "a collection whose child object kind is unregistered fails seal"
    );
}

// A child object kind with no canonical face, used to drive the
// no-canonical-when-fresh seal failure.
#[derive(serde::Serialize, serde::Deserialize)]
struct Bare {
    id: String,
}
impl Object for Bare {
    type Key = ItemKey;
    type State = ();
    type Canonical = Json;
    fn load(
        _cx: &Cx<()>,
        _key: &ItemKey,
        _since: Option<Validator>,
    ) -> impl Future<Output = Result<Load<Self>>> {
        async move { Ok(Load::NotFound) }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        omnifs_sdk::object::decode_json(bytes)
    }
    fn kind() -> ObjectKind {
        ObjectKind("test.bare")
    }
}

async fn list_bare(_key: RepoKey, _cx: ListCx<NoCursor, ()>) -> Result<Collection<Bare, NoCursor>> {
    Ok(Collection::complete([]))
}

#[test]
fn collection_of_child_without_canonical_face_fails_seal() {
    let mut r = Router::<()>::new();
    r.object::<Repo>("/{owner}/{repo}", |o| {
        o.dynamic();
        o.file("repo.json").canonical::<Json>()?;
        o.dir("bare/{id}").collection(list_bare)?;
        Ok(())
    })
    .unwrap();
    // Bare has a direct face only, no canonical: a fresh collection cannot
    // store its canonical.
    r.object::<Bare>("/{owner}/{repo}/bare/{id}", |o| {
        o.dynamic();
        o.file("live").direct(bare_live)?;
        Ok(())
    })
    .unwrap();
    let err = r.seal().unwrap_err();
    assert_eq!(
        err.kind(),
        omnifs_sdk::error::ProviderErrorKind::InvalidInput,
        "a collection of a child with no canonical face fails seal"
    );
}

async fn bare_live(_cx: Cx<()>, key: ItemKey) -> Result<FileProjection> {
    Ok(FileProjection::body(format!("bare:{}", key.id)).build())
}

// ===========================================================================
// Tree face (FIX A): an object dir tree face dispatches the subtree handoff
// ===========================================================================

/// A tree dir face's method: issue a git-open callout (the drive loop answers
/// it) and hand the subtree off.
async fn repo_tree(cx: Cx<()>, key: RepoKey) -> Result<omnifs_sdk::handler::TreeRef> {
    let opened = cx
        .git()
        .open_repo(
            format!("github.com/{}/{}", key.owner, key.repo),
            format!("git@github.com:{}/{}.git", key.owner, key.repo),
        )
        .await?;
    Ok(omnifs_sdk::handler::TreeRef::new(opened.tree))
}

fn tree_face_router() -> Router<()> {
    let mut r = Router::<()>::new();
    r.object::<Repo>("/{owner}/{repo}", |o| {
        o.dynamic();
        o.file("repo.json").canonical::<Json>()?;
        o.dir("repo").tree(repo_tree)?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();
    r
}

#[test]
fn object_tree_face_lookup_returns_subtree_handoff() {
    let r = tree_face_router();
    let cx = cx();
    // A lookup at the tree path runs the treeref handler (git-open callout) and
    // returns the host-resolved subtree handle, not a static dir.
    let lookup = drive(&cx, r.lookup_child(&cx, "/torvalds/linux", "repo"));
    let (out, _effects) = lookup.into_result_and_effects();
    assert!(
        matches!(out, wit_types::LookupChildResult::Subtree(7)),
        "the tree face must dispatch a subtree handoff, got {out:?} (FIX A)"
    );
}

#[test]
fn object_tree_face_list_returns_subtree_handoff() {
    let r = tree_face_router();
    let cx = cx();
    let list = drive(
        &cx,
        r.list_children(&cx, "/torvalds/linux/repo", None, None),
    );
    let (out, _effects) = list.into_result_and_effects();
    assert!(
        matches!(out, wit_types::ListChildrenResult::Subtree(7)),
        "listing the tree path returns the subtree handoff, got {out:?} (FIX A)"
    );
}

// ===========================================================================
// Collection sub-capture (FIX B): the list method reads the {filter} capture
// ===========================================================================

#[test]
fn nested_collection_list_varies_by_subcapture() {
    let r = nested_collection_router();
    let cx = cx();

    let open = drive(&cx, r.list_children(&cx, "/o/r/issues/open", None, None));
    let (open_out, _e) = open.into_result_and_effects();
    let wit_types::ListChildrenResult::Entries(open_listing) = open_out else {
        panic!("collection lists entries");
    };
    let open_names: Vec<&str> = open_listing
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();

    let all = drive(&cx, r.list_children(&cx, "/o/r/issues/all", None, None));
    let (all_out, _e) = all.into_result_and_effects();
    let wit_types::ListChildrenResult::Entries(all_listing) = all_out else {
        panic!("collection lists entries");
    };
    let all_names: Vec<&str> = all_listing
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();

    assert_eq!(
        open_names,
        vec!["7"],
        "the open filter lists issue 7 (method read {{filter}})"
    );
    assert_eq!(
        all_names,
        vec!["9"],
        "the all filter lists issue 9 (FIX B: the collection method reads the {{filter}} sub-capture, not the parent anchor key)"
    );
}

// ===========================================================================
// Choices face (FIX C): the choices dir lists its fixed names
// ===========================================================================

fn choices_router() -> Router<()> {
    let mut r = Router::<()>::new();
    r.object::<Repo>("/{owner}/{repo}", |o| {
        o.dynamic();
        o.file("repo.json").canonical::<Json>()?;
        o.dir("issues").choices(&["open", "all"])?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();
    r
}

#[test]
fn choices_face_lists_fixed_dir_entries_exhaustive() {
    let r = choices_router();
    let cx = cx();
    let list = drive(&cx, r.list_children(&cx, "/o/r/issues", None, None));
    let (out, _effects) = list.into_result_and_effects();
    let wit_types::ListChildrenResult::Entries(listing) = out else {
        panic!("the choices dir lists entries, got something else");
    };
    let mut names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["all", "open"],
        "the choices dir lists exactly its fixed names (FIX C)"
    );
    assert!(
        listing.exhaustive,
        "the choices listing is exhaustive (the fixed name set is the whole story)"
    );
    assert!(
        listing
            .entries
            .iter()
            .all(|e| matches!(e.kind, wit_types::EntryKind::Directory)),
        "each choice is a directory entry"
    );
}

// ===========================================================================
// Object stream face (R6): a live ranged leaf under an object anchor opens
// through `open_file` and serves chunks from its RangeReader.
// ===========================================================================

#[derive(serde::Serialize, serde::Deserialize)]
struct Pod {
    name: String,
}

struct PodKey {
    name: String,
}

impl FromCaptures for PodKey {
    fn from_captures(c: &Captures) -> Result<Self> {
        Ok(Self {
            name: c
                .get("name")
                .ok_or_else(|| ProviderError::invalid_input("missing name"))?
                .to_string(),
        })
    }
}
impl IdentityCaptures for PodKey {
    fn identity_captures(&self) -> Vec<(&'static str, String)> {
        vec![("name", self.name.clone())]
    }
}
impl FacetMetadata for PodKey {
    fn facet_axes() -> &'static [FacetAxis] {
        &[]
    }
}
impl Key for PodKey {}

impl Object for Pod {
    type Key = PodKey;
    type State = ();
    type Canonical = Json;
    fn load(
        _cx: &Cx<()>,
        key: &PodKey,
        _since: Option<Validator>,
    ) -> impl Future<Output = Result<Load<Self>>> {
        let name = key.name.clone();
        async move {
            Ok(Load::fresh(
                Pod { name: name.clone() },
                Canonical::new(format!(r#"{{"name":"{name}"}}"#).into_bytes(), None),
            ))
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        omnifs_sdk::object::decode_json(bytes)
    }
    fn kind() -> ObjectKind {
        ObjectKind("test.pod")
    }
}

impl Pod {
    /// A live log stream: a ranged reader over volatile tail bytes. The reader
    /// is in-memory here, but the face is marked `Live` (the only stability a
    /// stream face may carry beyond stable/dynamic).
    fn log(_cx: Cx<()>, key: PodKey) -> impl Future<Output = Result<StreamFile>> {
        async move {
            let bytes = format!("log line for {}\n", key.name).into_bytes();
            Ok(StreamFile::new(MemoryRangeReader::new(bytes)).live())
        }
    }
}

fn pod_router() -> Router<()> {
    let mut r = Router::<()>::new();
    r.object::<Pod>("/pods/{name}", |o| {
        o.dynamic();
        o.file("pod.json").canonical::<Json>()?;
        o.file("log").stream(Pod::log)?;
        Ok(())
    })
    .unwrap();
    r.seal().unwrap();
    r
}

#[test]
fn object_stream_face_opens_live_session_and_serves_a_chunk() {
    let r = pod_router();
    let cx = cx();

    // The stream leaf under an object anchor must NOT be a `read_file` target.
    let read = drive_result(&cx, r.read_file(&cx, "/pods/api/log", "", None));
    assert!(
        read.is_err(),
        "a stream face must be opened through open_file, not read_file"
    );

    // open_file resolves the Stream leaf under the object anchor and runs the
    // face's open handler.
    let opened = drive(&cx, r.open_file(&cx, "/pods/api/log"));

    // The opened session reports the stream's declared attrs: Live + Unknown.
    assert_eq!(
        opened.attrs.stability,
        Stability::Live,
        "a live stream face opens with Stability::Live"
    );
    assert_eq!(
        opened.attrs.size,
        Size::Unknown,
        "a live stream's size is unknown until read"
    );

    // The reader serves a chunk: bytes plus an eof flag (the in-memory tail is
    // fully readable in one chunk).
    let chunk = drive(&cx, async { opened.reader.read_chunk(&cx, 0, 4096).await });
    assert_eq!(chunk.content, b"log line for api\n");
    assert!(chunk.eof, "the in-memory tail returns eof on a full read");
}
