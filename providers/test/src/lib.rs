//! Test-provider: host guest-loader fixture on the object-shaped SDK surface.
//!
//! Synthetic canned data exercises the object SDK end to end: object canonical,
//! representation, and derive faces; a child-object `comments` collection with a
//! typed page cursor; scoped intent-tagged invalidation from `on_tick`;
//! object-load prefetch via `preload_object`; an object alias; deferred, ranged,
//! and live files; paged and partial listings with validators; rate limits; and
//! a subtree handoff.
//!
//! It is the conformance fixture every host integration test drives, so every
//! pinned behavior is preserved on the new surface: the root, hello, and items
//! listings; the ranged contracts; negative lookup; tree-777 rejection; and feed
//! pagination.

#![allow(clippy::needless_pass_by_value)]
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use core::time::Duration;

use omnifs_sdk::handler::BoxFuture;
use omnifs_sdk::object::Load;
use omnifs_sdk::prelude::*;

#[derive(Clone)]
#[config]
struct Config {
    /// Expose a provider-backed mount-root .gitignore for FUSE regression tests.
    #[serde(default)]
    root_ignore: bool,
}

#[derive(Clone, Default)]
struct State {
    fresh_full_reads: u64,
}

const LARGE_RANGED_SIZE: u64 = 64 * 1024 * 1024 + 1;

#[omnifs_sdk::path_captures]
struct DynamicCaptures {
    name: String,
}

// ===========================================================================
// Object family: `Item`
// ===========================================================================

#[omnifs_sdk::path_segment]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[strum(serialize_all = "snake_case")]
enum StateFilter {
    Open,
    All,
}

#[omnifs_sdk::object(kind = "test.item", key = ItemKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Item {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
}

impl Item {
    /// Object SDK derive signature: `fn(&Item, &ItemKey) -> Result<FileProjection>`.
    fn title(&self, _key: &ItemKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.title.as_bytes(), TextFormat::Raw).build())
    }

    fn state(&self, _key: &ItemKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.state.as_bytes(), TextFormat::Raw).build())
    }

    fn body(&self, _key: &ItemKey) -> Result<FileProjection> {
        Ok(FileProjection::inline(self.body.as_deref().unwrap_or(""))
            .content_type(ContentType::Markdown)
            .build())
    }

    /// Object-level stream face (R6): a live ranged leaf under the item anchor.
    /// It opens through `open-file` and serves volatile tail chunks (`tail -f`),
    /// exercising the object Stream dispatch the conformance fixture must cover.
    #[allow(clippy::unused_async)]
    async fn log(_cx: Cx<State>, _key: ItemKey) -> Result<StreamFile> {
        Ok(StreamFile::new(LiveTailReader).live())
    }

    /// The hand-written load `#[object]` forwards `Object::load` to. Item 404 is
    /// absent (negative lookup); a matching `since` validator is `Unchanged`
    /// (conditional load); otherwise fresh, and loading item 7 also preloads its
    /// sibling item 8 from the same payload (object-load prefetch).
    #[allow(clippy::unused_async)]
    async fn load(_cx: &Cx<State>, key: &ItemKey, since: Option<Validator>) -> Result<Load<Self>> {
        if key.number == 404 {
            return Ok(Load::NotFound);
        }
        let (value, canonical) = canned_item(key.number)?;
        if since.is_some() && since == canonical.validator {
            return Ok(Load::Unchanged);
        }
        let mut load = Load::fresh(value, canonical);
        if key.number == 7 {
            // Same-type sibling preload (Oura-shaped): one fetch materializes a
            // neighboring item's canonical at its own anchor.
            let (_, sibling_canonical) = canned_item(8)?;
            load = load.preload_object(ObjectEntry::fresh(
                ItemKey {
                    filter: Facet::<StateFilter>(key.filter.0),
                    number: 8,
                },
                sibling_canonical,
            ));
        }
        Ok(load)
    }
}

impl Representable<Markdown> for Item {
    fn represent(&self) -> Vec<u8> {
        format!(
            "# {}\n\n{}\n",
            self.title,
            self.body.as_deref().unwrap_or("")
        )
        .into_bytes()
    }
}

/// Canned `(value, canonical)` for one item number. Canonical bytes are the raw
/// upstream body and are filter-independent so `open/N` and `all/N` share
/// identity.
fn canned_item(number: u64) -> Result<(Item, Canonical)> {
    let bytes = format!(
        r#"{{"number":{number},"title":"Item {number}","body":"Body {number}","state":"open"}}"#
    )
    .into_bytes();
    let value = Item::decode(&bytes)?;
    let validator = Some(Validator::from(format!("item-{number}-v1")));
    Ok((value, Canonical::new(bytes, validator)))
}

#[omnifs_sdk::path_captures]
struct ItemKey {
    #[allow(dead_code)] // route facet; identity uses `number` only
    filter: Facet<StateFilter>,
    number: u64,
}

/// The `/items/{filter}` collection-dir key: carries the `{filter}` facet so the
/// list method lists the same items under either filter (identity collapse).
#[omnifs_sdk::path_captures]
struct ItemListKey {
    filter: Facet<StateFilter>,
}

/// The `/items/{filter}` listing. `items` is a synthetic dir with no parent
/// object, so this is a raw handler (not a typed object-dir collection): it
/// lists items 7 and 8 and stores each item's canonical plus its eager derived
/// leaves at listing time, so a later read of any item leaf serves warm
/// (collection preload). The typed `Collection`/`Cursor` surface is exercised by
/// the object-dir `comments` collection.
async fn item_list(_cx: DirCx<State>, key: ItemListKey) -> Result<DirProjection> {
    let filter = key.filter.0.to_string();
    let rows = [7_u64, 8]
        .into_iter()
        .map(|number| canned_item(number).map(|(item, canonical)| (number, item, canonical)))
        .collect::<Result<Vec<_>>>()?;

    let mut projection = DirProjection::exhaustive(
        rows.iter()
            .map(|(number, _, _)| Entry::dir(number.to_string())),
    );

    for (number, item, canonical) in rows {
        let item_key = ItemKey {
            filter: Facet::<StateFilter>(key.filter.0),
            number,
        };
        let base = format!("items/{filter}/{number}");
        // The canonical-view leaves are the canonical/representation/derived
        // file leaves only. `comments` is a collection DIR, not a view of the
        // item canonical, so it must not appear here.
        let leaves = ["item.json", "item.md", "title", "state", "body"]
            .into_iter()
            .map(|leaf| format!("/{base}/{leaf}"))
            .collect();
        projection = projection.store_canonical(
            item_key.anchor(Item::kind()),
            canonical.validator.clone(),
            canonical.bytes,
            leaves,
        );
        projection = projection.preload_file(
            format!("{base}/title"),
            FileProjection::inline(item.title.as_bytes().to_vec()).build(),
        );
        projection = projection.preload_file(
            format!("{base}/state"),
            FileProjection::inline(item.state.as_bytes().to_vec()).build(),
        );
    }

    Ok(projection)
}

// ===========================================================================
// Object family: `Comment` (child-object collection under an item)
// ===========================================================================

#[omnifs_sdk::object(kind = "test.comment", key = ItemCommentKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Comment {
    idx: u64,
    author: String,
    body: String,
}

impl Comment {
    fn author(&self, _key: &ItemCommentKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.author.as_bytes(), TextFormat::Raw).build())
    }

    #[allow(clippy::unused_async)]
    async fn load(
        _cx: &Cx<State>,
        key: &ItemCommentKey,
        _since: Option<Validator>,
    ) -> Result<Load<Self>> {
        if key.idx == 0 {
            return Err(ProviderError::invalid_input("comment index is 1-based"));
        }
        let (value, canonical) = canned_comment(key.idx)?;
        Ok(Load::fresh(value, canonical))
    }
}

impl Representable<Markdown> for Comment {
    fn represent(&self) -> Vec<u8> {
        format!("**{}**\n\n{}\n", self.author, self.body).into_bytes()
    }
}

fn canned_comment(idx: u64) -> Result<(Comment, Canonical)> {
    let bytes = format!(r#"{{"idx":{idx},"author":"commenter-{idx}","body":"comment {idx}"}}"#)
        .into_bytes();
    let value = Comment::decode(&bytes)?;
    let validator = Some(Validator::from(format!("comment-{idx}-v1")));
    Ok((value, Canonical::new(bytes, validator)))
}

#[omnifs_sdk::path_captures]
struct ItemCommentKey {
    #[allow(dead_code)]
    filter: Facet<StateFilter>,
    #[allow(dead_code)]
    number: u64,
    idx: u64,
}

/// The `/items/{filter}/{number}/comments` collection-dir key, carrying the
/// `{filter}` and `{number}` captures the list method needs.
#[omnifs_sdk::path_captures]
struct ItemCommentsListKey {
    filter: Facet<StateFilter>,
    number: u64,
}

/// The `comments` collection: paged child `Comment` objects. Page 0 lists
/// comment 1 with a resume cursor (typed `Page` + validator); the resumed page
/// lists comment 2 as `Partial` (open, no cursor). Exercises typed
/// Collection/Cursor page + partial + validator + fresh child canonical.
#[allow(clippy::unused_async)]
async fn item_comments(
    key: ItemCommentsListKey,
    cx: ListCx<PageCursor, State>,
) -> Result<Collection<Comment, PageCursor>> {
    let page = cx.cursor().map_or(0, |c| c.0);
    let idx = page + 1;
    let (value, canonical) = canned_comment(idx)?;
    let entry = CollectionEntry::fresh(
        ItemCommentKey {
            filter: Facet::<StateFilter>(key.filter.0),
            number: key.number,
            idx,
        },
        value,
        canonical,
    );
    if page == 0 {
        Ok(Collection::page([entry])
            .next(PageCursor(1))
            .with_validator(format!("comments-page-{page}")))
    } else {
        Ok(Collection::partial([entry]))
    }
}

// ===========================================================================
// Provider
// ===========================================================================

#[omnifs_sdk::provider(
    id = "test-provider",
    display_name = "A test provider with canned data",
    mount = "test",
    capabilities(
        domain(
            "httpbin.org",
            "Synthetic fixture endpoint exercised by host guest-loader tests."
        ),
        memory_mb(16, "Canned-data fixture needs only a small heap."),
    ),
    events(timer(Duration::from_mins(1), Self::on_tick))
)]
impl TestProvider {
    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        if config.root_ignore {
            r.file("/.gitignore").handler(root_ignore)?;
        }

        // Object family `Item`: facet collapse, canonical + representation +
        // derive faces, and a paged child-object `comments` collection.
        r.dir("/items").handler(items_root)?;
        r.dir("/items/{filter}").handler(item_list)?;
        r.object::<Item>("/items/{filter}/{number}", |o| {
            o.stable();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").derive(Item::title)?;
            o.file("state").derive(Item::state)?;
            o.file("body").lazy().derive(Item::body)?;
            // Object-level stream face (R6): a live ranged leaf opened through
            // open-file -> object Stream dispatch.
            o.file("log").stream(Item::log)?;
            o.dir("comments").collection(item_comments)?;
            Ok(())
        })?;

        // Child `Comment` objects, plus an alias mounting the same object spec
        // at a second template (same identity, deeper than any pinned listing).
        let comment = r.object::<Comment>("/items/{filter}/{number}/comments/{idx}", |o| {
            o.stable();
            o.file("comment.json").canonical::<Json>()?;
            o.file("comment.md").representation::<Markdown>()?;
            o.file("author").derive(Comment::author)?;
            Ok(())
        })?;
        r.alias("/items/{filter}/{number}/replies/{idx}", &comment)?;

        // Preserved structural surface (route-shaped coverage).
        r.dir("/hello").handler(hello_dir)?;
        r.file("/hello/message").handler(message)?;
        r.file("/hello/greeting").handler(greeting)?;
        r.file("/hello/projected").handler(projected)?;
        r.file("/hello/lazy").handler(lazy)?;
        r.file("/hello/fresh-full").handler(fresh_full)?;
        r.file("/hello/ranged").ranged().handler(ranged)?;
        r.file("/hello/unknown-ranged")
            .ranged()
            .handler(unknown_ranged)?;
        r.file("/hello/large-ranged")
            .ranged()
            .handler(large_ranged)?;
        r.file("/hello/volatile-tail")
            .ranged()
            .handler(volatile_tail)?;
        r.dir("/hello/bundle").handler(bundle)?;
        r.dir("/hello/feed").handler(feed)?;
        r.dir("/hello/unbounded").handler(unbounded_feed)?;
        r.dir("/hello/throttled").handler(throttled)?;
        r.dir("/hello/snapshot").handler(snapshot)?;
        r.dir("/hello/snapshot/comments")
            .handler(snapshot_comments)?;

        r.dir("/scoped").handler(scoped)?;
        r.file("/scoped/item").handler(scoped_item)?;

        r.dir("/dynamic/{name}").handler(dynamic)?;

        r.treeref("/checkout").handler(checkout)?;

        Ok(State::default())
    }

    /// Intent-tagged invalidation: evict the cached `/scoped/item` listing and
    /// item 7's canonical object (and its facet-expanded view leaves).
    #[allow(clippy::unused_async)]
    async fn on_tick(_cx: Cx<State>) -> Result<Invalidation> {
        Ok(Invalidation::new()
            .listing_path("/scoped/item")
            .object::<Item>(&ItemKey {
                filter: Facet::<StateFilter>(StateFilter::Open),
                number: 7,
            }))
    }
}

// ===========================================================================
// Object collection root
// ===========================================================================

async fn items_root(_cx: DirCx<State>) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive([
        Entry::dir("open"),
        Entry::dir("all"),
    ]))
}

async fn root_ignore(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"provider ignore\n".to_vec()).build())
}

// ===========================================================================
// /hello directory
// ===========================================================================

async fn hello_dir(_cx: DirCx<State>) -> Result<DirProjection> {
    Ok(
        DirProjection::open(core::iter::empty::<Entry>()).preload_dir(
            "hello/bundle",
            DirProjection::open([])
                .preload_file("title", FileProjection::inline(b"title".to_vec()).build())
                .preload_file("body", FileProjection::inline(b"body".to_vec()).build())
                .preload_file("empty", FileProjection::inline(Vec::new()).build()),
        ),
    )
}

async fn message(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"Hello, world!".to_vec()).build())
}

async fn greeting(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"Hi there!\n".to_vec()).build())
}

async fn projected(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"title\n".to_vec())
        .preload_file(
            "hello/body",
            FileProjection::inline(b"body\n".to_vec()).build(),
        )
        .preload_file(
            "hello/state",
            FileProjection::inline(b"open\n".to_vec()).build(),
        )
        .build())
}

async fn lazy(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"lazy\n".to_vec()).build())
}

async fn fresh_full(cx: Cx<State>) -> Result<FileProjection> {
    let read = cx.state_mut(|state| {
        state.fresh_full_reads += 1;
        state.fresh_full_reads
    });
    Ok(
        FileProjection::body(format!("fresh-full-{read}\n").into_bytes())
            .dynamic()
            .build(),
    )
}

// ===========================================================================
// Ranged / unknown-size / live files
// ===========================================================================

async fn ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(MemoryRangeReader::new(
        b"abcdefghijklmnopqrstuvwxyz".to_vec(),
    ))
    .size(Size::Exact(26))
    .dynamic()
    .version("alphabet-v1")
    .build())
}

async fn unknown_ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(
        FileProjection::ranged(MemoryRangeReader::new(b"unknown-size\n".to_vec()))
            .size(Size::Unknown)
            .stable()
            .build(),
    )
}

async fn large_ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(LargeRangedReader)
        .size(Size::Exact(LARGE_RANGED_SIZE))
        .stable()
        .build())
}

async fn volatile_tail(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(LiveTailReader)
        .size(Size::Unknown)
        .live()
        .build())
}

// ===========================================================================
// Static sub-directories
// ===========================================================================

async fn bundle(_cx: DirCx<State>) -> Result<DirProjection> {
    Ok(
        DirProjection::exhaustive([Entry::file("title"), Entry::file("body")])
            .preload_file(
                "hello/bundle/title",
                FileProjection::inline(b"title".to_vec()).build(),
            )
            .preload_file(
                "hello/bundle/body",
                FileProjection::inline(b"body".to_vec()).build(),
            ),
    )
}

async fn feed(cx: DirCx<State>) -> Result<DirProjection> {
    let page = cx.page_cursor(0);
    let entries = [
        Entry::dir(format!("item-{}", page * 2)),
        Entry::dir(format!("item-{}", page * 2 + 1)),
    ];
    if page >= 2 {
        Ok(DirProjection::open(entries))
    } else {
        Ok(DirProjection::paged(entries, Cursor::Page(page + 1))
            .with_validator(format!("feed-page-{page}")))
    }
}

async fn unbounded_feed(cx: DirCx<State>) -> Result<DirProjection> {
    let page = cx.page_cursor(0);
    let entries = [
        Entry::dir(format!("u-{}", page * 2)),
        Entry::dir(format!("u-{}", page * 2 + 1)),
    ];
    Ok(DirProjection::paged(entries, Cursor::Page(page + 1)))
}

async fn throttled(_cx: DirCx<State>) -> Result<DirProjection> {
    Err(ProviderError::rate_limited("test throttle").with_retry_after(Some(Duration::from_secs(2))))
}

async fn snapshot(_cx: DirCx<State>) -> Result<DirProjection> {
    Ok(
        DirProjection::exhaustive([Entry::file("status")]).preload_file(
            "hello/snapshot/status",
            FileProjection::inline(b"open\n".to_vec()).build(),
        ),
    )
}

async fn snapshot_comments(_cx: DirCx<State>) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive(core::iter::empty::<Entry>()))
}

// ===========================================================================
// /scoped
// ===========================================================================

async fn scoped(_cx: DirCx<State>) -> Result<DirProjection> {
    Ok(
        DirProjection::exhaustive([Entry::file("item")]).preload_file(
            "scoped/item",
            FileProjection::inline(b"scoped\n".to_vec()).build(),
        ),
    )
}

async fn scoped_item(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"scoped\n".to_vec()).build())
}

// ===========================================================================
// /dynamic/{name}
// ===========================================================================

async fn dynamic(_cx: DirCx<State>, captures: DynamicCaptures) -> Result<DirProjection> {
    let value_path = format!("dynamic/{}/value", captures.name);
    let mut content = captures.name.into_bytes();
    content.push(b'\n');
    Ok(DirProjection::exhaustive([Entry::file("value")])
        .preload_file(value_path, FileProjection::inline(content).build()))
}

// ===========================================================================
// /checkout subtree handoff
// ===========================================================================

async fn checkout(_cx: Cx<State>) -> Result<TreeRef> {
    Ok(TreeRef::new(777))
}

// ===========================================================================
// Live-tail range reader for the live file
// ===========================================================================

#[derive(Clone, Debug)]
struct LiveTailReader;

impl RangeReader for LiveTailReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let body = format!("tail:{offset}\n");
            let mut bytes = body.into_bytes();
            bytes.truncate(length as usize);
            Ok(FileChunk::new(bytes, false))
        })
    }
}

#[derive(Clone, Debug)]
struct LargeRangedReader;

impl RangeReader for LargeRangedReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let remaining = LARGE_RANGED_SIZE.saturating_sub(offset);
            let len = remaining.min(u64::from(length));
            let bytes = vec![b'L'; usize::try_from(len).expect("chunk length fits usize")];
            Ok(FileChunk::new(
                bytes,
                offset.saturating_add(len) >= LARGE_RANGED_SIZE,
            ))
        })
    }
}
