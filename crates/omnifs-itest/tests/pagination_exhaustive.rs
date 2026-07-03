//! Characterization: the Tree-level `@next`/`@all` pagination controls.
//!
//! N2 freezes CURRENT behavior. `crates/omnifs-engine/tests/pagination_test.rs`
//! already drives `Runtime::paginate_{next,all}` directly and inspects the cache;
//! this file characterizes the SAME feed through the kernel-free `Tree` surface a
//! frontend sees: a first-page browse listing carries the synthetic `@next`/`@all`
//! controls, reading `@next` advances the parent's accumulated dirents, and a
//! re-listing reflects the grown feed until the controls disappear at exhaustion.
//! `@all` reaches the same complete set in one read.
//!
//! The test-provider's `/hello/feed` route yields two `item-*` entries per page
//! across three pages (pages 0 and 1 carry a resume cursor, page 2 is terminal),
//! so the exhaustive set is `item-0 .. item-5`.

#![cfg(not(target_os = "wasi"))]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use omnifs_core::path::Path;
use omnifs_engine::{Cursor, Entry, ListOutcome, Node, ReadResult, RequestCtx, Tree};
use omnifs_itest::{RuntimeHarness, make_engine, make_runtime};
use tempfile::TempDir;

/// A wasm test-provider loaded into a `Runtime`, wrapped in a `Tree` under mount
/// "test". Owns the harness temp dirs that must outlive the `Runtime`.
struct PagedTree {
    tree: Tree,
    ctx: RequestCtx,
    _clone_dir: TempDir,
    _cache_dir: TempDir,
    _config_dir: TempDir,
}

fn paged_tree() -> PagedTree {
    let engine = make_engine();
    let RuntimeHarness {
        clone_dir,
        cache_dir,
        config_dir,
        runtime,
        ..
    } = make_runtime(&engine);
    let tree = Tree::for_runtime(Arc::new(runtime), "test");
    PagedTree {
        tree,
        ctx: RequestCtx::default(),
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
    }
}

fn path(s: &str) -> Path {
    Path::parse(s).unwrap()
}

impl PagedTree {
    async fn resolve(&self, path_str: &str) -> Node {
        self.tree
            .resolve(&path(path_str), &self.ctx)
            .await
            .unwrap_or_else(|e| panic!("resolve {path_str}: {e}"))
    }

    /// List a directory node (browse listing, `cursor = None`) and return the
    /// listing's entries. Panics on a subtree handoff (the feed is a provider
    /// directory, never a backing subtree).
    async fn list(&self, node: &Node) -> Vec<Entry> {
        match self
            .tree
            .list(node, None::<Cursor>, &self.ctx)
            .await
            .unwrap_or_else(|e| panic!("list {}: {e}", node.path().as_str()))
        {
            ListOutcome::Listing(listing) => listing.entries,
            ListOutcome::Subtree(dir) => {
                panic!("expected a provider listing, got subtree {}", dir.display())
            },
        }
    }

    /// Read a node's bytes, asserting provider/synthetic bytes (never a subtree
    /// handoff). Returns the produced bytes (a control read returns a status line).
    async fn read(&self, node: &Node) -> Vec<u8> {
        match self
            .tree
            .read(node, &self.ctx)
            .await
            .unwrap_or_else(|e| panic!("read {}: {e}", node.path().as_str()))
        {
            ReadResult::Bytes { data, .. } => data,
            ReadResult::Subtree(dir) => panic!("expected bytes, got subtree {}", dir.display()),
        }
    }
}

/// Provider (non-synthetic) child names in listing order.
fn item_names(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| !e.is_synthetic())
        .map(|e| e.name.clone())
        .collect()
}

fn has_entry(entries: &[Entry], name: &str) -> bool {
    entries.iter().any(|e| e.name == name)
}

/// Reading `@next` repeatedly through the Tree surface drains the paged feed:
/// the browse listing accumulates each page, the `@next`/`@all` controls persist
/// while a resume cursor remains, and the terminal page clears both the cursor
/// and the controls. The final listing carries every fixture entry exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn reading_next_drains_feed_and_drops_controls() {
    let t = paged_tree();
    let feed = t.resolve("/hello/feed").await;
    assert!(feed.is_dir(), "/hello/feed resolves to a directory");

    // First-page browse listing: two items plus the synthetic controls.
    let page0 = t.list(&feed).await;
    assert_eq!(item_names(&page0), ["item-0", "item-1"], "page 0 items");
    assert!(has_entry(&page0, "@next"), "a paged listing carries @next");
    assert!(has_entry(&page0, "@all"), "a paged listing carries @all");

    // Drive `@next` through the Tree surface until the controls disappear. Each
    // read advances the parent's accumulated dirents; a re-listing reflects them.
    let mut latest = page0;
    let mut reads = 0;
    while has_entry(&latest, "@next") {
        reads += 1;
        assert!(
            reads <= 4,
            "feed must exhaust in a bounded number of @next reads"
        );
        let next = t.resolve("/hello/feed/@next").await;
        assert!(next.is_synthetic(), "@next resolves to a synthetic control");
        // The control read returns a human-readable status line, never provider
        // bytes; its side effect is growing the feed.
        let status = t.read(&next).await;
        assert!(!status.is_empty(), "a control read yields a status line");
        latest = t.list(&feed).await;
    }

    assert_eq!(
        reads, 2,
        "two @next reads drain a three-page feed (page 0 seeded)"
    );
    assert_eq!(
        item_names(&latest),
        ["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"],
        "the exhausted listing carries every fixture entry exactly once"
    );
    assert!(
        !has_entry(&latest, "@next"),
        "@next disappears at exhaustion"
    );
    assert!(!has_entry(&latest, "@all"), "@all disappears at exhaustion");
}

/// Reading `@all` once expands the feed to completion in a single control read,
/// reaching the same complete set that the `@next` loop reaches page by page.
#[tokio::test(flavor = "multi_thread")]
async fn reading_all_materializes_the_complete_set() {
    let t = paged_tree();
    let feed = t.resolve("/hello/feed").await;

    // Seed page 0 so the parent dirents carry the `@all` control.
    let page0 = t.list(&feed).await;
    assert_eq!(item_names(&page0), ["item-0", "item-1"]);
    assert!(has_entry(&page0, "@all"), "a paged listing carries @all");

    let all = t.resolve("/hello/feed/@all").await;
    assert!(all.is_synthetic(), "@all resolves to a synthetic control");
    let status = t.read(&all).await;
    assert!(!status.is_empty(), "the @all read yields a status line");

    let complete = t.list(&feed).await;
    assert_eq!(
        item_names(&complete),
        ["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"],
        "@all materializes the same complete set the @next loop reaches"
    );
    assert!(
        !has_entry(&complete, "@next"),
        "a fully expanded feed has no @next"
    );
    assert!(
        !has_entry(&complete, "@all"),
        "a fully expanded feed has no @all"
    );
}
