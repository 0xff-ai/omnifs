//! FUSE integration tests: pagination, synthetic controls, rate-limit surfacing.

#![allow(dead_code, clippy::wildcard_imports)]

use super::Frontend;
use super::common::{DirSnapshot, FullReadTarget, InodeBody, root_ignore_meta, split_parent_leaf};
use super::read_helpers::data_slice;
use fuser::Errno;
use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::Path;
use omnifs_core::view::{DirentRecord, DirentsPayload, EntryKind, EntryMeta};
use omnifs_host::HostContext;
use omnifs_host::cloner::GitCloner;
use omnifs_host::pagination;
use omnifs_host::path_key::PathKey;
use omnifs_host::registry::ProviderRegistry;
use omnifs_wit::provider::types as wit_types;
use omnifs_wit::provider::types::ListChildrenResult;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

fn test_path(s: &str) -> Path {
    Path::parse(s).expect("test path")
}

// ---- FUSE-path pagination harness -------------------------------------
//
// WHAT THIS HARNESS PROVES: the FUSE op boundary, driven in the same order
// the kernel calls it (`opendir` -> `lookup` -> `open` -> `read` ->
// `release`). Each op delegates the listing/lookup/read DECISION to `Tree`; the
// adapter then builds the kernel snapshot/inode/reply state on the neutral
// result. The harness drives those op-boundary methods and asserts the
// observable kernel-facing result:
//   - `opendir_op` (snapshot building from `Tree::list`, including the
//     `@next`/`@all` controls and mount-root ignore files as inode-allocated
//     synthetic entries, and the serve-stale-on-rate-limit path),
//   - `lookup_op` (synthetic-control resolution, root-ignore synthesis after a
//     negative provider result, ENOENT for dead controls),
//   - `open_op` / `open_synthetic` (per-`fh` buffer materialization from
//     `Tree::read`, `synthetic` inode gating, full-deferred prefetch),
//   - the per-`fh` `file_cache` + `data_slice` read slicing,
//   - the inode-table `synthetic`-marker semantics (`get_or_alloc_ino_*`),
//     which FUSE keeps even though the byte sources moved into `Tree`.
//
// WHAT IT DOES NOT PROVE: the kernel reply plumbing itself. fuser's
// `Reply*` sinks have only a `pub(crate)` constructor and their test
// `ReplySender::Assert` is `#[cfg(test)]` inside fuser, so they cannot be
// constructed from this crate. The thin `reply.entry(..)`/`reply.data(..)`/
// `reply.error(..)` calls inside the trait methods (argument marshaling into
// the sink) are therefore not exercised here; everything that decides WHAT
// to reply is. End-to-end reply marshaling is covered by the live-container
// smoke harness, not by these unit tests.

fn wasm_artifact_path(file_name: &str) -> PathBuf {
    let workspace_root = StdPath::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("host crate must have a workspace parent")
        .parent()
        .expect("workspace root must exist");
    workspace_root
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(file_name)
}

struct FuseHarness {
    fs: Frontend,
    _cache_dir: TempDir,
    _config_dir: TempDir,
    _providers_dir: TempDir,
}

fn build_harness() -> FuseHarness {
    build_harness_with_provider_config("{}")
}

/// Build a `Frontend` backed by the test provider mounted as the root mount,
/// so paths are mount-relative (`hello/feed`).
fn build_harness_with_provider_config(provider_config: &str) -> FuseHarness {
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());
    let providers_dir = tempfile::tempdir().expect("providers dir");
    // The test provider goes into the provider store.
    let test_src = wasm_artifact_path("test_provider.wasm");
    assert!(
        test_src.exists(),
        "test_provider.wasm missing at {}. Run `just providers build` first.",
        test_src.display()
    );
    let test_bytes = std::fs::read(&test_src).expect("read test provider");
    let id = omnifs_workspace::ids::ProviderId::from_wasm_bytes(&test_bytes);
    let store = omnifs_workspace::provider::ProviderStore::new(providers_dir.path());
    store
        .put_if_absent(&id, &test_bytes)
        .expect("put test provider");
    store
        .install(
            id,
            omnifs_workspace::ids::ProviderMeta {
                name: omnifs_workspace::ids::ProviderName::new("test-provider").unwrap(),
                version: None,
            },
            "test_provider.wasm".into(),
        )
        .expect("install test provider");

    let mount_config = format!(
        r#"{{
                "provider": {{ "id": "{id}", "meta": {{ "name": "test-provider" }} }},
                "mount": "test",
                "root_mount": true,
                "capabilities": {{ "domains": ["httpbin.org"] }},
                "config": {provider_config}
            }}"#
    );

    let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
    let registry = ProviderRegistry::new(
        HostContext::new(
            cache_dir.path(),
            &paths.config_dir,
            providers_dir.path(),
            &paths.credentials_file,
        ),
        cloner,
    )
    .expect("registry init");

    let rt = tokio::runtime::Handle::current();
    let spec = omnifs_workspace::mounts::Spec::parse(&mount_config).expect("parse mount spec");
    registry.add_mount(&spec, &rt).expect("add test mount");
    let fs = Frontend::new(rt, Arc::new(registry));

    FuseHarness {
        fs,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
        _providers_dir: providers_dir,
    }
}

impl FuseHarness {
    const MOUNT: &'static str = "test";

    /// Drive the kernel `opendir` op boundary: allocate the directory inode and
    /// build its snapshot through `Tree::list` (the same `opendir_op` the fuser
    /// `opendir` callback calls). The snapshot carries the provider children plus
    /// the host-synthesized `@next`/`@all` controls and mount-root ignore files.
    async fn opendir(&self, path: &str) -> DirSnapshot {
        self.try_opendir(path).await.expect("opendir")
    }

    async fn try_opendir(&self, path: &str) -> Result<DirSnapshot, Errno> {
        let path = Path::parse(path).expect("test path");
        let ino = self
            .fs
            .get_or_alloc_ino(Self::MOUNT, &path, EntryKind::Directory, 0);
        self.fs.opendir_op(Self::MOUNT, ino, &path, None).await
    }

    fn seed_stale_dirents(&self, path: &str, names: &[&str]) {
        let payload = DirentsPayload {
            entries: names
                .iter()
                .map(|name| DirentRecord {
                    name: (*name).to_string(),
                    meta: EntryMeta::directory(),
                })
                .collect(),
            exhaustive: false,
            validator: None,
            next_cursor: None,
            paginated: false,
        };
        let record = CacheRecord::new(
            RecordKind::Dirents,
            payload.serialize().expect("serialize stale dirents"),
        );
        let runtime = self.fs.runtime_for_mount(Self::MOUNT).expect("runtime");
        let path = test_path(path);
        runtime
            .cache()
            .cache_put(&path, RecordKind::Dirents, None, &record);
    }

    /// `lookup` for a child, returning `Some(ino)` on a positive hit and
    /// `None` on ENOENT. Drives the `lookup_op` op boundary (the same
    /// `Tree::resolve_child` the fuser `lookup` callback calls): the cache-first
    /// lookup, the `@next`/`@all` control resolution, and the mount-root ignore
    /// synthesis after a negative provider result all live in `Tree`.
    async fn lookup(&self, parent: &str, name: &str) -> Option<u64> {
        let parent_path = Path::parse(parent).expect("test parent path");
        let child = parent_path.join(name).expect("test child segment");
        let ino_for = |fs: &Frontend| {
            fs.path_to_inode
                .get(&PathKey::with_mount_str(Self::MOUNT, child.clone()).expect("mount name"))
                .map(|r| *r)
        };
        match self
            .fs
            .lookup_op(Self::MOUNT, &parent_path, name, None)
            .await
        {
            Ok(_) => ino_for(&self.fs),
            Err(_) => None,
        }
    }

    /// Mirror the kernel `open` then `read` path for a control/ignore leaf:
    /// run the open op (which materializes the per-`fh` buffer through
    /// `Tree::read`), then serve `read` at the given offsets via `data_slice`.
    /// Returns the bytes served per read, concatenated, plus the allocated `fh`.
    async fn open_and_read(&self, path: &str, reads: &[(u64, u32)]) -> (u64, Vec<u8>) {
        let target = self.read_target(path).await;
        let fh = target.fh;
        self.fs
            .open_op(&target, None)
            .await
            .expect("open of a synthetic control/ignore file materializes the per-fh buffer");

        let mut out = Vec::new();
        for &(offset, size) in reads {
            let cached = self.fs.file_cache.get(&fh).expect("per-fh buffer present");
            out.extend_from_slice(data_slice(&cached, offset, size));
        }
        (fh, out)
    }

    /// Build the `FullReadTarget` the `open`/`read` op boundary consumes:
    /// resolve `path` to an inode (walking parent dirents so each segment is
    /// allocated), allocate a fresh `fh`, and snapshot the inode-cached
    /// projection attrs + inode body the open dispatch reads.
    async fn read_target(&self, path: &str) -> FullReadTarget {
        let ino = self
            .lookup_path(path)
            .await
            .expect("path resolves before open");
        let (attrs, body) = self
            .fs
            .inodes
            .get(&ino)
            .map_or((None, InodeBody::Provider), |e| {
                (e.attrs.clone(), e.body.clone())
            });
        FullReadTarget {
            ino,
            fh: self.fs.alloc_fh(),
            mount_name: Self::MOUNT.to_string(),
            path: Path::parse(path).expect("test path"),
            body,
            attrs,
        }
    }

    /// Resolve a multi-segment mount-relative path to an inode, walking
    /// parent dirents so each segment is allocated.
    async fn lookup_path(&self, path: &str) -> Option<u64> {
        let path = Path::parse(path).ok()?;
        let (parent, leaf) = split_parent_leaf(&path)?;
        // Ensure the parent is listed so its dirents (and controls) exist.
        self.opendir(parent.as_str()).await;
        self.lookup(parent.as_str(), &leaf).await
    }

    fn release(&self, fh: u64) {
        self.fs.file_cache.remove(&fh);
    }

    async fn prefetch_mutable_unversioned_full(&self, path: &str) -> (u64, Vec<u8>) {
        let target = self.read_target(path).await;
        let fh = target.fh;
        self.fs
            .open_op(&target, None)
            .await
            .expect("unknown full file prefetches on open");
        let bytes = self
            .fs
            .file_cache
            .get(&fh)
            .expect("prefetch populates per-fh buffer")
            .clone();
        (fh, bytes)
    }

    fn inode_size(&self, ino: u64) -> u64 {
        self.fs.inodes.get(&ino).expect("inode exists").size
    }

    /// Item (non-`@`) entry names in a directory snapshot.
    fn item_names(snapshot: &DirSnapshot) -> Vec<String> {
        snapshot
            .iter()
            .map(|(_, name, _)| name.clone())
            .filter(|name| !omnifs_host::pagination::is_reserved_provider_leaf(name))
            .collect()
    }

    fn snapshot_names(snapshot: &DirSnapshot) -> Vec<String> {
        snapshot.iter().map(|(_, name, _)| name.clone()).collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limited_listing_serves_stale_cache() {
    let h = build_harness();
    h.seed_stale_dirents("/hello/throttled", &["cached-a", "cached-b"]);

    let snapshot = h
        .try_opendir("/hello/throttled")
        .await
        .expect("rate-limited listing serves stale dirents");

    assert_eq!(
        FuseHarness::snapshot_names(&snapshot),
        vec!["cached-a".to_string(), "cached-b".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_window_is_recorded_and_short_circuits_provider() {
    let h = build_harness();
    h.seed_stale_dirents("/hello/throttled", &["cached-during-window"]);

    let first = h
        .try_opendir("/hello/throttled")
        .await
        .expect("initial 429 serves stale dirents");
    assert_eq!(
        FuseHarness::snapshot_names(&first),
        vec!["cached-during-window".to_string()]
    );

    let runtime = h.fs.runtime_for_mount(FuseHarness::MOUNT).expect("runtime");
    assert!(
        runtime.rate_limited_until().is_some(),
        "provider 429 records the mount-level rate-limit window"
    );

    // A second opendir while the rate-limit window is open serves the stale
    // dirents from cache rather than calling the provider and getting EAGAIN.
    let cached = h
        .try_opendir("/hello/throttled")
        .await
        .expect("open rate-limit window serves stale dirents");
    assert_eq!(
        FuseHarness::snapshot_names(&cached),
        vec!["cached-during-window".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limited_listing_without_cache_still_eagains() {
    let h = build_harness();

    let err = h
        .try_opendir("/hello/throttled")
        .await
        .expect_err("no-cache rate-limited listing stays EAGAIN");

    assert_eq!(i32::from(err), i32::from(Errno::EAGAIN));
    let runtime = h.fs.runtime_for_mount(FuseHarness::MOUNT).expect("runtime");
    assert!(
        runtime.rate_limited_until().is_some(),
        "the no-cache path still records the rate-limit window"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cat_next_advances_exactly_one_page_and_survives_partial_reads() {
    let h = build_harness();

    // Page 0 listing carries the cursor and synthesizes @next/@all.
    let page0 = h.opendir("/hello/feed").await;
    let names = FuseHarness::snapshot_names(&page0);
    assert!(names.contains(&"item-0".to_string()));
    assert!(names.contains(&"item-1".to_string()));
    assert!(names.contains(&"@next".to_string()), "controls present");
    assert!(names.contains(&"@all".to_string()));
    let item_count = |s: &DirSnapshot| {
        s.iter()
            .filter(|(_, n, _)| !omnifs_host::pagination::is_reserved_provider_leaf(n))
            .count()
    };
    assert_eq!(item_count(&page0), 2);

    // open("@next") + a split read: offset 0 (partial), then a second read
    // at a nonzero offset. The status string must come back intact and the
    // feed must advance EXACTLY one page (a per-offset re-run would advance
    // multiple pages and splice slices).
    let status_full = {
        // First, read the whole status with one big read to know its bytes.
        let (fh, full) = h.open_and_read("/hello/feed/@next", &[(0, 4096)]).await;
        h.release(fh);
        full
    };
    // The single @next above already advanced page 1. Re-seed a fresh
    // harness to test the split-read advancement in isolation.
    let h = build_harness();
    h.opendir("/hello/feed").await;
    let half = u32::try_from(status_full.len() / 2).unwrap();
    let (fh, spliced) = h
        .open_and_read(
            "/hello/feed/@next",
            &[
                (0, half),
                (u64::from(half), 4096),
                // A trailing zero-length EOF read must serve empty, not re-run.
                (u64::try_from(status_full.len()).unwrap(), 4096),
            ],
        )
        .await;
    assert_eq!(
        spliced, status_full,
        "split reads reassemble the same status string"
    );
    h.release(fh);

    // The feed advanced exactly one page: items grew from 2 to 4, cursor 1->2.
    let after = h.opendir("/hello/feed").await;
    assert_eq!(
        item_count(&after),
        4,
        "exactly one page (two items) was appended despite three reads"
    );
    assert!(
        FuseHarness::snapshot_names(&after).contains(&"@next".to_string()),
        "controls remain while a cursor remains"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustion_drops_controls_and_keeps_accumulated_entries() {
    let h = build_harness();
    h.opendir("/hello/feed").await;

    // Advance to exhaustion: pages 1 and 2. Page 2 is terminal.
    for _ in 0..2 {
        let (fh, status) = h.open_and_read("/hello/feed/@next", &[(0, 4096)]).await;
        assert!(!status.is_empty());
        h.release(fh);
    }

    // The completed feed lists every accumulated item with NO controls,
    // served from cache (regression for the "reset to page 0" bug: the
    // terminal page is non-exhaustive with no cursor, so opendir must trust
    // the `paginated` marker instead of refetching page 0).
    let final_snapshot = h.opendir("/hello/feed").await;
    let names = FuseHarness::snapshot_names(&final_snapshot);
    for i in 0..6 {
        assert!(
            names.contains(&format!("item-{i}")),
            "item-{i} present in completed feed; got {names:?}"
        );
    }
    assert!(
        !names
            .iter()
            .any(|n| omnifs_host::pagination::is_reserved_provider_leaf(n)),
        "controls drop once the feed is exhausted; got {names:?}"
    );

    // A lookup of a now-dead control is ENOENT (no stale inode resurrected).
    assert_eq!(
        h.lookup("/hello/feed", "@next").await,
        None,
        "@next is ENOENT after exhaustion"
    );

    // A further open of @next fails cleanly (ENOENT), not a provider read.
    let ino = h.fs.get_or_alloc_ino(
        FuseHarness::MOUNT,
        &test_path("/hello/feed/@next"),
        EntryKind::File,
        0,
    );
    let target = FullReadTarget {
        ino,
        fh: h.fs.alloc_fh(),
        mount_name: FuseHarness::MOUNT.to_string(),
        path: test_path("/hello/feed/@next"),
        body: InodeBody::Provider,
        attrs: None,
    };
    assert!(
        matches!(h.fs.open_synthetic(&target, None).await, Err(e) if i32::from(e) == i32::from(Errno::ENOENT)),
        "opening a dead control is ENOENT, never a provider read_file"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_root_ignore_opens_without_provider_read() {
    let h = build_harness();
    // The root listing has no real .gitignore, so the host synthesizes one.
    h.opendir("/").await;
    let ino = h.lookup("/", ".gitignore").await;
    assert!(ino.is_some(), ".gitignore resolves at the root");

    let (fh, content) = h.open_and_read("/.gitignore", &[(0, 4096)]).await;
    assert_eq!(content, pagination::IGNORE_CONTENT.as_bytes());
    h.release(fh);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutable_unversioned_full_prefetch_is_per_handle_not_durable() {
    let h = build_harness();
    let path = "/hello/fresh-full";

    let (first_fh, first) = h.prefetch_mutable_unversioned_full(path).await;
    assert_eq!(first, b"fresh-full-1\n");
    h.release(first_fh);
    let runtime = h.fs.runtime_for_mount(FuseHarness::MOUNT).expect("runtime");
    assert!(
        runtime
            .cache()
            .cache_get(&test_path(path), RecordKind::File, None)
            .is_none(),
        "unversioned dynamic full-read bytes must not be written to durable view cache",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn learned_full_read_size_survives_cached_non_exact_refresh() {
    let h = build_harness();
    let path = "/hello/fresh-full";
    let ino = h
        .lookup_path(path)
        .await
        .expect("path resolves before open");
    assert_eq!(
        h.inode_size(ino),
        1,
        "unknown full-deferred files start with the stat sentinel"
    );

    let (fh, bytes) = h.prefetch_mutable_unversioned_full(path).await;
    assert_eq!(bytes, b"fresh-full-1\n");
    h.release(fh);
    assert_eq!(
        h.inode_size(ino),
        u64::try_from(bytes.len()).unwrap(),
        "full-read prefetch publishes the learned exact size"
    );

    // A later listing re-describes the file with a kind-derived placeholder
    // (unknown size, default stability). Replaying that metadata must not erase
    // the exact size learned from the complete read.
    let refreshed = h
        .lookup_path(path)
        .await
        .expect("path resolves after refresh");
    assert_eq!(refreshed, ino, "refresh reuses the existing inode");
    assert_eq!(
        h.inode_size(refreshed),
        u64::try_from(bytes.len()).unwrap(),
        "cached non-exact metadata does not downgrade learned size"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_all_stops_at_the_page_cap_on_an_unbounded_feed() {
    // The `unbounded` feed always returns a next cursor, so without the cap
    // `@all` would loop forever. It must stop at exactly
    // MAX_PAGINATION_PAGES and report the capped status.
    let h = build_harness();
    let page0 = h.opendir("/hello/unbounded").await;
    assert_eq!(
        FuseHarness::item_names(&page0).len(),
        2,
        "page 0 has two items before @all"
    );

    let (fh, status) = h.open_and_read("/hello/unbounded/@all", &[(0, 8192)]).await;
    let status = String::from_utf8(status).unwrap();
    h.release(fh);

    let cap = usize::try_from(omnifs_host::pagination::MAX_PAGINATION_PAGES).unwrap();
    assert!(
        status.contains(&format!("capped at {cap} pages")),
        "@all reports the cap, not completion; got {status:?}"
    );
    assert!(
        !status.contains("complete"),
        "an unbounded feed never reports completion; got {status:?}"
    );

    // page 0 (2) + exactly `cap` pages loaded by @all (2 each). The feed did
    // not run away: it stopped at the bound, with the control still present
    // because a cursor remains.
    let snapshot = h.opendir("/hello/unbounded").await;
    let item_count = FuseHarness::item_names(&snapshot).len();
    assert_eq!(
        item_count,
        2 + 2 * cap,
        "@all loaded exactly the cap of pages and no more; got {item_count}"
    );
    assert!(
        FuseHarness::snapshot_names(&snapshot).contains(&"@next".to_string()),
        "controls remain because the unbounded feed still has a cursor"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preload_merge_into_paged_dir_preserves_pagination_state() {
    // Regression for Fix 1: an fs-effect/preload that merges a child into a
    // PAGED directory must NOT clear the directory's `next_cursor`/
    // `paginated`/`validator`. `merge_projected_dirs` (the fs-effect writer)
    // is a non-exhaustive MERGE, so it has to carry the prior record's
    // pagination state forward; before the fix it wrote `next_cursor: None,
    // paginated: false`, silently killing `@next`/`@all` and refetching
    // page 0.
    let h = build_harness();
    h.opendir("/hello/feed").await; // stores paginated page-0 dirents (cursor -> 1)
    let runtime = h.fs.runtime_for_mount(FuseHarness::MOUNT).expect("runtime");

    // The accumulated record is paginated with a live cursor.
    let before = DirentsPayload::deserialize(
        &runtime
            .cache()
            .cache_get(&test_path("/hello/feed"), RecordKind::Dirents, None)
            .expect("dirents before")
            .payload,
    )
    .expect("payload before");
    assert!(before.paginated, "feed is paginated before the merge");
    assert!(
        before.next_cursor.is_some(),
        "feed still carries a resume cursor before the merge"
    );

    // Apply a non-exhaustive fs-effect that merges a brand-new child
    // directory into the paged feed (mirrors a preload/projection).
    let effects = wit_types::Effects {
        canonical: Vec::new(),
        fs: vec![
            wit_types::FsWrite {
                id: None,
                path: "/hello/feed".to_string(),
                kind: wit_types::FsKind::Directory(false),
            },
            wit_types::FsWrite {
                id: None,
                path: "/hello/feed/preloaded".to_string(),
                kind: wit_types::FsKind::Directory(false),
            },
        ],
        invalidations: Vec::new(),
    };
    runtime.apply_effects_for_test(&effects, 0);

    let after = DirentsPayload::deserialize(
        &runtime
            .cache()
            .cache_get(&test_path("/hello/feed"), RecordKind::Dirents, None)
            .expect("dirents after")
            .payload,
    )
    .expect("payload after");
    assert!(
        after.paginated,
        "the merge preserved the `paginated` marker"
    );
    assert!(
        after.next_cursor.is_some(),
        "the merge preserved the resume cursor (controls survive)"
    );
    let names: Vec<&str> = after.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"preloaded"),
        "the merged child was added; got {names:?}"
    );
    assert!(
        names.contains(&"item-0") && names.contains(&"item-1"),
        "page-0 items survive the merge; got {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_effect_projection_rejects_reserved_control_leaf() {
    // Regression for Fix 1's centralized `@` filter in
    // `ProjectionAccumulator::add`: an fs-effect must never project a leaf
    // that shadows a `@next`/`@all` control entry.
    let h = build_harness();
    h.opendir("/hello/feed").await;
    let runtime = h.fs.runtime_for_mount(FuseHarness::MOUNT).expect("runtime");

    let effects = wit_types::Effects {
        canonical: Vec::new(),
        fs: vec![
            wit_types::FsWrite {
                id: None,
                path: "/hello/feed".to_string(),
                kind: wit_types::FsKind::Directory(false),
            },
            wit_types::FsWrite {
                id: None,
                path: "/hello/feed/@next".to_string(),
                kind: wit_types::FsKind::Directory(false),
            },
        ],
        invalidations: Vec::new(),
    };
    runtime.apply_effects_for_test(&effects, 0);

    // The reserved-leaf write was dropped: no per-entry lookup record was
    // written for the shadowing path.
    assert!(
        runtime
            .cache()
            .cache_get(&test_path("/hello/feed/@next"), RecordKind::Lookup, None)
            .is_none(),
        "a reserved '@'-prefixed fs-effect leaf is never cached"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_gitignore_wins_over_synthetic_marker() {
    // Regression for Fix 2 through the actual lookup path: if a stale
    // synthetic inode already exists and the provider really projects a
    // mount-root `.gitignore`, lookup must consult the provider, reuse the
    // inode, and clear the synthetic marker. This exercises
    // `lookup_check_caches` -> `lookup_via_provider`, not just inode
    // allocation in isolation.
    let h = build_harness_with_provider_config(r#"{ "root_ignore": true }"#);

    let meta = root_ignore_meta();
    let synth_ino =
        h.fs.get_or_alloc_ino_synthetic(FuseHarness::MOUNT, &test_path("/.gitignore"), meta);
    assert!(
        h.fs.inodes
            .get(&synth_ino)
            .is_some_and(|e| e.body.is_synthetic()),
        "stale host-synthesized .gitignore starts synthetic"
    );

    let resolved_ino = h
        .lookup("/", ".gitignore")
        .await
        .expect(".gitignore resolves");
    assert_eq!(
        resolved_ino, synth_ino,
        "provider lookup reuses the existing path inode"
    );
    assert!(
        h.fs.inodes
            .get(&resolved_ino)
            .is_some_and(|e| !e.body.is_synthetic()),
        "provider lookup clears the synthetic marker"
    );

    let target = FullReadTarget {
        ino: resolved_ino,
        fh: h.fs.alloc_fh(),
        mount_name: FuseHarness::MOUNT.to_string(),
        path: test_path("/.gitignore"),
        body: InodeBody::Provider,
        attrs: None,
    };
    assert!(
        h.fs.open_synthetic(&target, None).await.unwrap().is_none(),
        "a provider-backed .gitignore is not served by the synthetic ignore path"
    );

    let runtime = h.fs.runtime_for_mount(FuseHarness::MOUNT).expect("runtime");
    let result = runtime
        .namespace()
        .read_file(
            &test_path("/.gitignore"),
            Path::parse("/.gitignore")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .expect("provider read succeeds");
    match result.bytes {
        wit_types::ByteSource::Inline(bytes) => {
            assert_eq!(bytes, b"provider ignore\n");
        },
        other => panic!("expected inline provider ignore content, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_root_ignore_survives_dirents_refresh() {
    // Regression for Fix 2: an origin-agnostic refresh (a cached
    // dirents/control replay through `get_or_alloc_ino_meta`) must NOT flip a
    // still-synthetic node back to provider-origin. Only a genuine resolution
    // clears `synthetic`.
    let h = build_harness();
    h.opendir("/").await;
    let synth_ino = h
        .lookup("/", ".gitignore")
        .await
        .expect(".gitignore resolves");
    assert!(
        h.fs.inodes
            .get(&synth_ino)
            .is_some_and(|e| e.body.is_synthetic()),
        "synthetic before refresh"
    );

    // A refresh (NodeOrigin::Refresh) of the same path must leave the flag.
    let meta = root_ignore_meta();
    let refreshed =
        h.fs.get_or_alloc_ino_meta(FuseHarness::MOUNT, &test_path("/.gitignore"), meta);
    assert_eq!(refreshed, synth_ino, "refresh reuses the inode");
    assert!(
        h.fs.inodes
            .get(&refreshed)
            .is_some_and(|e| e.body.is_synthetic()),
        "an origin-agnostic refresh keeps the synthetic marker"
    );

    // And the synthetic file still opens with the fixed ignore content,
    // never a provider read.
    let (fh, content) = h.open_and_read("/.gitignore", &[(0, 4096)]).await;
    assert_eq!(content, pagination::IGNORE_CONTENT.as_bytes());
    h.release(fh);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_next_accumulates_every_page_with_no_loss() {
    // Race PRESSURE (not deterministic proof): two `@next` reads fire
    // concurrently against the same paged directory and the per-path
    // pagination lock must serialize the read-modify-write so
    // page0 + page1 + page2 accumulate in order. This test does NOT force the
    // bad interleaving (both threads snapshotting the base record before
    // either stores), so it can pass even with a buggy lock under a benign
    // schedule; the deterministic proof of the no-loss invariant is
    // `continuation_page_does_not_overwrite_accumulated_dirents` (a single
    // continuation must never overwrite the accumulated dirents record).
    // This case adds runtime stress on top of that invariant.
    let h = build_harness();
    h.opendir("/hello/feed").await; // page 0: item-0, item-1 (cursor -> 1)

    // Drive both advances through the production open op boundary (what an
    // `open("@next")` runs: `Tree::read` paginates under the per-path lock and
    // invalidates the mem), from two tasks at once. Each task builds its own
    // `@next` open target so the concurrent read-modify-writes race.
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let fs = h.fs.clone();
        tasks.push(tokio::spawn(async move {
            let ino = fs.get_or_alloc_ino(
                FuseHarness::MOUNT,
                &test_path("/hello/feed/@next"),
                EntryKind::File,
                0,
            );
            let target = FullReadTarget {
                ino,
                fh: fs.alloc_fh(),
                mount_name: FuseHarness::MOUNT.to_string(),
                path: test_path("/hello/feed/@next"),
                body: InodeBody::Provider,
                attrs: None,
            };
            let _ = fs.open_op(&target, None).await;
        }));
    }
    for task in tasks {
        task.await.expect("@next task joins");
    }

    // After two ordered @next: page0 (item-0,1) + page1 (item-2,3) +
    // page2 (item-4,5). No page lost, none duplicated.
    let snapshot = h.opendir("/hello/feed").await;
    let mut items = FuseHarness::item_names(&snapshot);
    items.sort();
    let expected: Vec<String> = (0..6).map(|i| format!("item-{i}")).collect();
    assert_eq!(
        items, expected,
        "two concurrent @next accumulate every page exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuation_page_does_not_overwrite_accumulated_dirents() {
    // The no-transient-dirents invariant behind the concurrency fix: a raw
    // continuation `list_children(cursor=Some)` must NOT write the
    // directory's authoritative dirents record. Only `paginate_next` writes
    // the accumulated payload, so a racing reader can never observe a
    // page-only record for the path.
    let h = build_harness();
    h.opendir("/hello/feed").await; // stores accumulated page 0 (item-0, item-1)
    let runtime = h.fs.runtime_for_mount(FuseHarness::MOUNT).expect("runtime");

    // Fetch page 1 directly as a continuation. This returns item-2/item-3
    // but must leave the cached dirents for `hello/feed` unchanged.
    let result = runtime
        .namespace()
        .list_children(
            &test_path("/hello/feed"),
            None,
            Some(wit_types::Cursor::Page(1)),
            None,
        )
        .await;
    assert!(
        matches!(result, Ok(ListChildrenResult::Entries(_))),
        "continuation returns page 1 entries"
    );

    let record = runtime
        .cache()
        .cache_get(&test_path("/hello/feed"), RecordKind::Dirents, None)
        .expect("dirents record still cached");
    let dirents = DirentsPayload::deserialize(&record.payload).expect("dirents payload");
    let names: Vec<&str> = dirents.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"item-0") && names.contains(&"item-1"),
        "page-0 items survive the continuation; got {names:?}"
    );
    assert!(
        !names.contains(&"item-2") && !names.contains(&"item-3"),
        "the continuation's page-1 items did NOT overwrite the dirents record; got {names:?}"
    );
}

// Proves the omnifs side of `tail -f`: opening a live ranged file spawns a
// follow pump whose upstream probes advance the size `getattr` reports, between
// the follower's own reads. Composed with the standalone FUSE spike (which
// proved a polling `tail -f` reads forward whenever that size grows under
// TTL=0 + direct I/O), this closes the chain end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn volatile_follow_pump_advances_reported_size() {
    let h = build_harness();
    let path = "/hello/volatile-tail";
    let ino = h.lookup_path(path).await.expect("volatile-tail resolves");

    // Drive the real lookup->open path: the inode carries only the cheap
    // Deferred(Full) lookup placeholder, and `open_ranged_file` probes
    // `open_file` to discover the file is actually ranged + live.
    let (attrs, body) =
        h.fs.inodes
            .get(&ino)
            .map_or((None, InodeBody::Provider), |e| {
                (e.attrs.clone(), e.body.clone())
            });
    let fh = h.fs.alloc_fh();
    let target = FullReadTarget {
        ino,
        fh,
        mount_name: FuseHarness::MOUNT.to_string(),
        path: test_path(path),
        body,
        attrs,
    };
    let flags = h.fs.open_op(&target, None).await.expect("open ranged live");
    assert!(
        flags.contains(fuser::FopenFlags::FOPEN_DIRECT_IO),
        "ranged open serves direct I/O"
    );

    // The size getattr would report to the kernel: the read-promoted inode size
    // maxed with the follow pump's latest observed upstream end.
    let reported = |fs: &Frontend| -> u64 {
        let base = fs.inodes.get(&ino).map_or(0, |e| e.size);
        base.max(fs.follow_sizes.get(&ino).map_or(0, |v| *v))
    };

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let s1 = reported(&h.fs);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let s2 = reported(&h.fs);

    if let Some((_, pump)) = h.fs.follow_pumps.remove(&fh) {
        pump.abort();
    }

    assert!(
        s1 > 0,
        "follow pump advances the reported size within a tick (got {s1})"
    );
    assert!(
        s2 > s1,
        "follow pump keeps the size growing between a follower's reads ({s1} -> {s2})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn volatile_follow_pump_probes_from_foreground_eof() {
    use std::sync::atomic::Ordering;
    let h = build_harness();
    let path = "/hello/volatile-tail";
    let ino = h.lookup_path(path).await.expect("volatile-tail resolves");
    let (attrs, body) =
        h.fs.inodes
            .get(&ino)
            .map_or((None, InodeBody::Provider), |e| {
                (e.attrs.clone(), e.body.clone())
            });
    let fh = h.fs.alloc_fh();
    let target = FullReadTarget {
        ino,
        fh,
        mount_name: FuseHarness::MOUNT.to_string(),
        path: test_path(path),
        body,
        attrs,
    };
    h.fs.open_op(&target, None).await.expect("open ranged live");

    let foreground_eof = 4096;
    let ranged = h.fs.ranged_handles.get(&fh).expect("ranged handle present");
    ranged
        .handle
        .observed_end()
        .fetch_max(foreground_eof, Ordering::Relaxed);
    drop(ranged);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let reported = h.fs.follow_sizes.get(&ino).map_or(0, |v| *v);

    if let Some((_, pump)) = h.fs.follow_pumps.remove(&fh) {
        pump.abort();
    }

    assert!(
        reported > foreground_eof,
        "follow pump must probe from the foreground EOF, not from zero ({reported} <= {foreground_eof})"
    );
}
