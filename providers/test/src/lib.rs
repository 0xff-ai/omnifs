//! Test-provider: host guest-loader fixture on the object-shaped SDK surface.
//!
//! Synthetic canned data exercises the object SDK end to end: object canonical,
//! representation, and computed faces; a child-object `comments` collection with a
//! typed page cursor; scoped intent-tagged invalidation from `on_tick`;
//! object-load prefetch via `preload_object`; an object alias; deferred, ranged,
//! and live files; an object tree handoff; paged and partial listings with
//! validators; rate limits; and object aliases.
//!
//! It is the conformance fixture every host integration test drives, so every
//! pinned behavior is preserved on the new surface: the root, hello, and items
//! listings; the ranged contracts; negative lookup; and feed
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

#[omnifs_sdk::path_captures]
struct SlowCaptures {
    ms: u64,
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
    /// Object SDK computed signature: `fn(&Item, &ItemKey) -> Result<FileProjection>`.
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

    /// A live ranged leaf under the item anchor.
    /// It opens through `open-file`, exercising the object Stream dispatch the
    /// conformance fixture must cover. Unlike `hello/ranged` (`LiveTailReader`,
    /// probed only at fixed offsets), a recursive walk (`grep -r`) opens every
    /// leaf under the item anchor including this one and reads it to
    /// completion, so its reader must actually reach EOF (`ItemLogReader`)
    /// rather than fabricate bytes forever.
    #[allow(clippy::unused_async)]
    async fn log(_cx: Cx<State>, _key: ItemKey) -> Result<StreamFile> {
        Ok(StreamFile::new(ItemLogReader).live())
    }

    /// Returns an intentionally unknown handle so host validation can exercise
    /// the retained object-directory tree-handoff boundary.
    #[allow(clippy::unused_async)]
    async fn unknown_tree(_cx: Cx<State>, _key: ItemKey) -> Result<TreeRef> {
        Ok(TreeRef::new(777))
    }

    /// The hand-written load `#[object]` forwards `Object::load` to. Item 404 is
    /// absent (negative lookup); a matching `since` validator is `Unchanged`
    /// (conditional load); otherwise fresh, and loading item 7 also preloads its
    /// sibling item 8 from the same payload (object-load prefetch).
    #[allow(clippy::unused_async)]
    async fn load(cx: &Cx<State>, key: &ItemKey, since: Option<Validator>) -> Result<Load<Self>> {
        if key.number == 404 {
            return Ok(Load::NotFound);
        }
        if key.number == 9 {
            return load_remote_item(cx, key, since).await;
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

async fn load_remote_item(
    cx: &Cx<State>,
    key: &ItemKey,
    since: Option<Validator>,
) -> Result<Load<Item>> {
    let mut request = cx
        .http()
        .get(format!("https://httpbin.org/anything/items/{}", key.number));
    if let Some(validator) = since.as_ref() {
        request = request.header("if-none-match", validator.as_str());
    }
    let response = request.send().await?;
    match response.status().as_u16() {
        200 => {
            let validator = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(Validator::from);
            let bytes = response.into_body();
            let value = Item::decode(&bytes)?;
            Ok(Load::fresh(value, Canonical::new(bytes, validator)))
        },
        304 => Ok(Load::Unchanged),
        404 => Ok(Load::NotFound),
        status => Err(ProviderError::network(format!(
            "remote item returned status {status}"
        ))),
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
/// lists items 7 and 8 and stores each item's canonical plus its eager computed
/// leaves at listing time, so a later read of any item leaf serves warm
/// (collection preload). The typed `Collection`/`Cursor` surface is exercised by
/// the object-dir `comments` collection.
async fn item_list(_cx: DirCx<State>, key: ItemListKey) -> Result<DirListing> {
    let filter = key.filter.0.to_string();
    let rows = [7_u64, 8]
        .into_iter()
        .map(|number| canned_item(number).map(|(item, canonical)| (number, item, canonical)))
        .collect::<Result<Vec<_>>>()?;

    let mut projection = DirListing::exhaustive(
        rows.iter()
            .map(|(number, _, _)| Entry::dir(number.to_string())),
    );

    for (number, item, canonical) in rows {
        let item_key = ItemKey {
            filter: Facet::<StateFilter>(key.filter.0),
            number,
        };
        let base = format!("items/{filter}/{number}");
        // The canonical-view leaves are the canonical/representation/computed
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
    capabilities(domain(
        "httpbin.org",
        "Synthetic fixture endpoint exercised by host guest-loader tests."
    ),),
    limits(memory_mb(16, "Canned-data fixture needs only a small heap."),),
    events(timer(60, Self::on_tick))
)]
impl TestProvider {
    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        if config.root_ignore {
            r.file("/.gitignore").handler(root_ignore)?;
        }

        // Object family `Item`: facet collapse, canonical + representation +
        // computed faces, and a paged child-object `comments` collection.
        r.dir("/items").handler(items_root)?;
        r.dir("/items/{filter}").handler(item_list)?;
        r.object::<Item>("/items/{filter}/{number}", |o| {
            o.stable();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").computed(Item::title)?;
            o.file("state").computed(Item::state)?;
            o.file("body").lazy().computed(Item::body)?;
            // The live ranged leaf opens through object stream dispatch.
            o.file("log").stream(Item::log)?;
            o.dir("comments").collection(item_comments)?;
            o.dir("checkout").tree(Item::unknown_tree)?;
            Ok(())
        })?;

        // Child `Comment` objects, plus an alias mounting the same object spec
        // at a second template (same identity, deeper than any pinned listing).
        let comment = r.object::<Comment>("/items/{filter}/{number}/comments/{idx}", |o| {
            o.stable();
            o.file("comment.json").canonical::<Json>()?;
            o.file("comment.md").representation::<Markdown>()?;
            o.file("author").computed(Comment::author)?;
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
        // Each suspends on an HTTP callout. Two distinct paths (so reads do not
        // coalesce) let the host concurrency test hold two ops in flight at once
        // to prove independent ops interleave on one instance.
        r.file("/hello/remote-a").handler(remote)?;
        r.file("/hello/remote-b").handler(remote)?;
        // A read parked on a slow upstream: the frontend concurrency net holds
        // the callout answer for `{ms}` to prove one slow op cannot block the
        // mount.
        r.file("/slow/{ms}").ranged().handler(slow)?;
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
        r.file("/hello/live-log").ranged().handler(live_log)?;
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

async fn items_root(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::exhaustive([
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

async fn hello_dir(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::open(core::iter::empty::<Entry>()).preload_dir(
        "hello/bundle",
        DirListing::open([])
            .preload_file("title", FileProjection::inline(b"title".to_vec()).build())
            .preload_file("body", FileProjection::inline(b"body".to_vec()).build())
            .preload_file("empty", FileProjection::inline(Vec::new()).build()),
    ))
}

async fn message(cx: Cx<State>) -> Result<FileProjection> {
    let body = cx
        .version()
        .map_or("Hello, world!", |version| version.as_str());
    Ok(FileProjection::body(body.as_bytes().to_vec()).build())
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

/// Issues a single HTTP callout and returns its body verbatim. The handler
/// suspends on the async host import, so the host concurrency test can hold two
/// of these in flight simultaneously on one provider instance.
async fn remote(cx: Cx<State>) -> Result<FileProjection> {
    let resp = cx.http().get("https://httpbin.org/get").send().await?;
    Ok(FileProjection::body(resp.into_body()).dynamic().build())
}

/// A file whose read stays in flight for `{ms}` milliseconds (capped at 10s).
///
/// The file is ranged so the slowness lands on the read op itself: opening it
/// is cheap, and the first chunk read suspends on an HTTP callout that the
/// frontend concurrency tests hold open for the requested delay, with the
/// host's callout-capture harness playing the slow upstream. Keeping the open
/// fast matters on macOS NFS, whose client serializes OPENs mount-wide, so a
/// file that is slow to open would measure client protocol behavior instead of
/// frontend dispatch. Against a real network the URL is httpbin's delay
/// endpoint, whose 10-second ceiling matches the cap (it takes whole seconds,
/// so the requested delay rounds up).
async fn slow(_cx: Cx<State>, captures: SlowCaptures) -> Result<FileProjection> {
    let ms = captures.ms.min(10_000);
    Ok(FileProjection::ranged(SlowReader { ms })
        .size(Size::Unknown)
        .dynamic()
        .build())
}

/// Range reader for `slow/{ms}`: the first chunk performs the delayed HTTP
/// callout and serves the response body as the whole file.
#[derive(Clone, Debug)]
struct SlowReader {
    ms: u64,
}

impl RangeReader for SlowReader {
    fn read_chunk<'a>(
        &'a self,
        cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let secs = self.ms.div_ceil(1000);
            let resp = cx
                .http()
                .get(format!("https://httpbin.org/delay/{secs}"))
                .send()
                .await?;
            let body = resp.into_body();
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(body.len());
            let end = start.saturating_add(length as usize).min(body.len());
            Ok(FileChunk::new(body[start..end].to_vec(), end >= body.len()))
        })
    }
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

async fn live_log(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(GrowingLogReader)
        .size(Size::Unknown)
        .live()
        .build())
}

// ===========================================================================
// Static sub-directories
// ===========================================================================

async fn bundle(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(
        DirListing::exhaustive([Entry::file("title"), Entry::file("body")])
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

async fn feed(cx: DirCx<State>) -> Result<DirListing> {
    let page = cx.page_cursor(0);
    let entries = [
        Entry::dir(format!("item-{}", page * 2)),
        Entry::dir(format!("item-{}", page * 2 + 1)),
    ];
    if page >= 2 {
        Ok(DirListing::open(entries))
    } else {
        Ok(DirListing::paged(entries, Cursor::Page(page + 1))
            .with_validator(format!("feed-page-{page}")))
    }
}

async fn unbounded_feed(cx: DirCx<State>) -> Result<DirListing> {
    let page = cx.page_cursor(0);
    let entries = [
        Entry::dir(format!("u-{}", page * 2)),
        Entry::dir(format!("u-{}", page * 2 + 1)),
    ];
    Ok(DirListing::paged(entries, Cursor::Page(page + 1)))
}

async fn throttled(_cx: DirCx<State>) -> Result<DirListing> {
    Err(ProviderError::rate_limited("test throttle").with_retry_after(Some(Duration::from_secs(2))))
}

async fn snapshot(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(
        DirListing::exhaustive([Entry::file("status")]).preload_file(
            "hello/snapshot/status",
            FileProjection::inline(b"open\n".to_vec()).build(),
        ),
    )
}

async fn snapshot_comments(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::exhaustive(core::iter::empty::<Entry>()))
}

// ===========================================================================
// /scoped
// ===========================================================================

async fn scoped(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::exhaustive([Entry::file("item")]).preload_file(
        "scoped/item",
        FileProjection::inline(b"scoped\n".to_vec()).build(),
    ))
}

async fn scoped_item(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"scoped\n".to_vec()).build())
}

// ===========================================================================
// /dynamic/{name}
// ===========================================================================

async fn dynamic(_cx: DirCx<State>, captures: DynamicCaptures) -> Result<DirListing> {
    let value_path = format!("dynamic/{}/value", captures.name);
    let mut content = captures.name.into_bytes();
    content.push(b'\n');
    Ok(DirListing::exhaustive([Entry::file("value")])
        .preload_file(value_path, FileProjection::inline(content).build()))
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

/// Finite content for the `Item::log` object stream face.
///
/// Unlike [`LiveTailReader`] (fixed-offset probes only), a recursive walk
/// (`grep -r "$ROOT/items/open/7"`) opens this leaf like any other file and
/// reads it to completion, so it must actually signal EOF once its small,
/// fixed content is exhausted rather than fabricate bytes forever.
const ITEM_LOG_CONTENT: &str = "log line 0\nlog line 1\nlog line 2\n";

#[derive(Clone, Debug)]
struct ItemLogReader;

impl RangeReader for ItemLogReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let content = ITEM_LOG_CONTENT.as_bytes();
            let total = content.len() as u64;
            if offset >= total {
                return Ok(FileChunk::new(Vec::new(), true));
            }
            let start = usize::try_from(offset).expect("log offset fits usize");
            let end = offset.saturating_add(u64::from(length)).min(total);
            let end_usize = usize::try_from(end).expect("log end fits usize");
            Ok(FileChunk::new(
                content[start..end_usize].to_vec(),
                end >= total,
            ))
        })
    }
}

/// A genuinely growing live file for follow-mode conformance (`tail -f`).
///
/// Unlike [`LiveTailReader`], which fabricates bytes at ANY offset and never
/// signals EOF (so a reader scanning to end-of-file never terminates), this
/// models a real log: the current extent is wall-clock-driven (one fixed-width
/// line at first read, one more every 500ms, capped), a read below the extent
/// serves bytes bounded at the extent, and a read at or past the extent
/// returns the contract's "no more right now" shape: an empty chunk with
/// `eof = true`, which for a live file marks the current end without ending it
/// (`RangedHandle::read` grows the observed end monotonically and
/// `probe_live_growth` keeps polling).
#[derive(Clone, Debug)]
struct GrowingLogReader;

/// `line 000042\n`: 12 bytes per line, so extent math is exact.
const LOG_LINE_LEN: u64 = 12;
const LOG_LINE_CAP: u64 = 10_000;
const LOG_LINE_INTERVAL_MS: u64 = 500;

impl GrowingLogReader {
    /// Wall-clock epoch, fixed at the first read so growth is measured from
    /// first observation rather than provider start.
    fn epoch() -> std::time::SystemTime {
        static EPOCH: std::sync::OnceLock<std::time::SystemTime> = std::sync::OnceLock::new();
        *EPOCH.get_or_init(std::time::SystemTime::now)
    }

    /// Current extent in bytes: 1 line plus one per elapsed interval, capped.
    fn extent_now() -> u64 {
        let elapsed_ms = Self::epoch().elapsed().map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
        let lines = (1 + elapsed_ms / LOG_LINE_INTERVAL_MS).min(LOG_LINE_CAP);
        lines * LOG_LINE_LEN
    }
}

impl RangeReader for GrowingLogReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let extent = Self::extent_now();
            if offset >= extent {
                return Ok(FileChunk::new(Vec::new(), true));
            }
            let end = offset.saturating_add(u64::from(length)).min(extent);
            let first_line = offset / LOG_LINE_LEN;
            let last_line = (end - 1) / LOG_LINE_LEN;
            let mut lines = Vec::new();
            for n in first_line..=last_line {
                lines.extend_from_slice(format!("line {n:06}\n").as_bytes());
            }
            let skip = usize::try_from(offset - first_line * LOG_LINE_LEN)
                .expect("line-relative offset fits usize");
            let take = usize::try_from(end - offset).expect("chunk length fits usize");
            Ok(FileChunk::new(
                lines[skip..skip + take].to_vec(),
                end >= extent,
            ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_snapshot_matches() {
        let mut router = Router::<State>::new();
        TestProvider::start(Config { root_ignore: false }, &mut router).unwrap();
        let router = router.compile().unwrap();

        let actual = omnifs_sdk::serde_json::to_string_pretty(&router.routes()).unwrap();
        let expected = include_str!("../tests/routes.snapshot.json").trim_end();
        if actual != expected {
            eprintln!("{actual}");
        }
        assert_eq!(actual, expected);
    }
}
