//! Test-provider: host guest-loader fixture on the object-shaped surface.
//!
//! Synthetic canned data exercises §14 invariants (identity collapse, canonical
//! source leaves, eager/lazy preload, collection preload, conditional load,
//! negatives, flatten, structural routes, pagination, rate limits, mount
//! isolation, intent-tagged invalidation, subtree handoff).

#![allow(clippy::needless_pass_by_value)]
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use core::fmt;
use core::str::FromStr;
use core::time::Duration;

use omnifs_sdk::browse::FileContent;
use omnifs_sdk::handler::BoxFuture;
use omnifs_sdk::object::{Key, Load};
use omnifs_sdk::prelude::*;

#[derive(Clone)]
#[config]
struct Config {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateFilter {
    Open,
    All,
}

impl FromStr for StateFilter {
    type Err = ProviderError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "all" => Ok(Self::All),
            other => Err(ProviderError::invalid_input(format!(
                "unknown state filter {other:?}"
            ))),
        }
    }
}

impl fmt::Display for StateFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::All => "all",
        })
    }
}

impl PathSegment for StateFilter {
    fn choices() -> Option<&'static [&'static str]> {
        Some(&["open", "all"])
    }
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
    fn title(&self) -> Result<FileContent> {
        Ok(FileContent::new(self.title.as_bytes())
            .with_content_type(ContentType::Custom("text/plain")))
    }

    fn state(&self) -> Result<FileContent> {
        Ok(FileContent::new(self.state.as_bytes())
            .with_content_type(ContentType::Custom("text/plain")))
    }

    fn body(&self) -> Result<FileContent> {
        Ok(FileContent::new(self.body.as_deref().unwrap_or(""))
            .with_content_type(ContentType::Markdown))
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
/// upstream body (#3) and are filter-independent so `open/N` and `all/N` share
/// identity (#1).
fn canned_item(number: u64) -> Result<(Item, Canonical)> {
    let bytes = format!(
        r#"{{"number":{number},"title":"Item {number}","body":"Body {number}","state":"open"}}"#
    )
    .into_bytes();
    let value = Item::parse_canonical(&bytes)?;
    let validator = Some(Validator::from(format!("item-{number}-v1")));
    Ok((value, Canonical { bytes, validator }))
}

#[omnifs_sdk::path_captures]
struct ItemKey {
    #[allow(dead_code)] // route facet (#1); identity uses `number` only
    filter: Facet<StateFilter>,
    number: u64,
}

impl Key for ItemKey {
    type Object = Item;
    type State = State;

    async fn load(&self, _cx: &Cx<State>, since: Option<Validator>) -> Result<Load<Item>> {
        if self.number == 404 {
            return Ok(Load::NotFound);
        }
        let (value, canonical) = canned_item(self.number)?;
        if since.is_some() && since == canonical.validator {
            return Ok(Load::Unchanged);
        }
        Ok(Load::fresh_from(value, canonical))
    }
}

#[omnifs_sdk::path_captures]
struct ItemListKey {
    filter: Facet<StateFilter>,
}

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
        let item_key = key.item(number);
        let base = format!("items/{filter}/{number}");
        let leaves = ["item.json", "item.md", "title", "state", "body", "comments"]
            .into_iter()
            .map(|leaf| format!("/{base}/{leaf}"))
            .collect();
        projection = projection.store_canonical(
            item_key.anchor(),
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

impl ItemListKey {
    fn item(&self, number: u64) -> ItemKey {
        ItemKey {
            filter: Facet::<StateFilter>(self.filter.0),
            number,
        }
    }
}

// Parent captures declared inline; `#[flatten]` awaits `path_captures` helper-attribute registration in the SDK.
#[omnifs_sdk::path_captures]
struct ItemCommentKey {
    #[allow(dead_code)]
    filter: Facet<StateFilter>,
    number: u64,
    idx: u64,
}

async fn item_comment_read(_cx: Cx<State>, key: ItemCommentKey) -> Result<FileProjection> {
    if key.idx == 0 {
        return Err(ProviderError::invalid_input("comment index is 1-based"));
    }
    let body = format!("commenter-{}:\ncomment {}\n", key.idx, key.idx);
    Ok(FileProjection::body(body.into_bytes()).build())
}

async fn item_comments(_cx: DirCx<State>, _key: ItemKey) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive([
        Entry::file("1"),
        Entry::file("2"),
    ]))
}

// ===========================================================================
// Provider
// ===========================================================================

#[omnifs_sdk::provider(
    metadata = "omnifs.provider.json",
    version = "0.1.0",
    events(timer(Duration::from_secs(60), Self::on_tick))
)]
impl TestProvider {
    type Config = Config;
    type State = State;

    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        if config.root_ignore {
            r.file("/.gitignore").handler(root_ignore)?;
        }

        // Object family `Item`: facet collapse (#1), collection preload (#5).
        r.dir("/items").handler(items_root)?;
        r.dir("/items/{filter}").handler(item_list)?;
        r.object::<Item>("/items/{filter}/{number}", |o| {
            o.representations("item", (Markdown,))?;
            o.file("title").project(Item::title)?;
            o.file("state").project(Item::state)?;
            o.file("body").project(Item::body)?;
            o.dir("comments").handler(item_comments)?;
            o.file("comments/{idx}").handler(item_comment_read)?;
            Ok(())
        })?;

        // Preserved structural surface (route-shaped coverage).
        r.dir("/hello").handler(hello_dir)?;
        r.file("/hello/message").handler(message)?;
        r.file("/hello/greeting").handler(greeting)?;
        r.file("/hello/projected").handler(projected)?;
        r.file("/hello/lazy").handler(lazy)?;
        r.file("/hello/fresh-full").handler(fresh_full)?;
        r.file("/hello/ranged").handler(ranged)?;
        r.file("/hello/unknown-ranged").handler(unknown_ranged)?;
        r.file("/hello/large-ranged").handler(large_ranged)?;
        r.file("/hello/volatile-tail").handler(volatile_tail)?;
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

    #[allow(clippy::unused_async)]
    async fn on_tick(_cx: Cx<State>) -> Result<Effects> {
        let mut effects = Effects::new();
        effects.invalidate_listing_path("/scoped/item");
        effects.invalidate_object(
            &ItemKey {
                filter: Facet::<StateFilter>(StateFilter::Open),
                number: 7,
            }
            .anchor(),
        );
        Ok(effects)
    }
}

// ===========================================================================
// Object collection root (#18)
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
            .mutable()
            .build(),
    )
}

// ===========================================================================
// Ranged / unknown-size / volatile files
// ===========================================================================

async fn ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(MemoryRangeReader::new(
        b"abcdefghijklmnopqrstuvwxyz".to_vec(),
    ))
    .size(Size::Exact(26))
    .mutable()
    .version("alphabet-v1")
    .build())
}

async fn unknown_ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(
        FileProjection::ranged(MemoryRangeReader::new(b"unknown-size\n".to_vec()))
            .size(Size::Unknown)
            .immutable()
            .build(),
    )
}

async fn large_ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(LargeRangedReader)
        .size(Size::Exact(LARGE_RANGED_SIZE))
        .immutable()
        .build())
}

async fn volatile_tail(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(LiveTailReader)
        .size(Size::Unknown)
        .volatile()
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
// Live-tail range reader for the volatile file
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct OpenFacet;

    impl FromStr for OpenFacet {
        type Err = ProviderError;

        fn from_str(_s: &str) -> Result<Self> {
            Ok(Self)
        }
    }

    impl fmt::Display for OpenFacet {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("context")
        }
    }

    impl PathSegment for OpenFacet {}

    #[omnifs_sdk::path_captures]
    struct OpenFacetKey {
        #[allow(dead_code)]
        context: Facet<OpenFacet>,
        id: u64,
    }

    #[test]
    fn path_captures_generates_finite_facet_metadata() {
        let axes = ItemKey::facet_axes();
        assert_eq!(axes.len(), 1);
        assert_eq!(axes[0].capture_name, "filter");
        assert_eq!(axes[0].choices, &["open", "all"]);

        let key = ItemKey {
            filter: Facet(StateFilter::Open),
            number: 7,
        };
        assert_eq!(key.identity_captures(), vec![("number", "7".to_string())]);
    }

    #[test]
    fn path_captures_skips_open_ended_facet_metadata() {
        assert!(OpenFacetKey::facet_axes().is_empty());

        let key = OpenFacetKey {
            context: Facet(OpenFacet),
            id: 9,
        };
        assert_eq!(key.identity_captures(), vec![("id", "9".to_string())]);
    }
}
