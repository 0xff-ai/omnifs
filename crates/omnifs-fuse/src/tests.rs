//! FUSE adapter tests over the engine `Namespace` surface.
//!
//! WHAT THESE PROVE: the FUSE op boundary's translation of the plain-data
//! namespace surface into kernel identity and reply payloads, driven in the
//! order the kernel calls it (`opendir` -> `lookup` -> `open` -> `read`):
//!   - root enumeration and descent through `do_opendir`/`do_lookup`,
//!   - kernel-inode allocation and dedup (a path keeps one inode across ops),
//!   - the whole-vs-ranged read dispatch: a `Whole` file materializes once into
//!     the per-`fh` buffer and slices locally; a `Ranged` file reads through and
//!     reuses the namespace's single provider open,
//!   - the pagination controls (`@next`/`@all`) arriving as ordinary file nodes
//!     that open and read exactly once.
//!
//! WHAT THEY DO NOT PROVE: the kernel reply plumbing itself (fuser's `Reply*`
//! sinks have no cross-crate constructor). The op-boundary methods return plain
//! data, which the thin fuser callbacks marshal; end-to-end reply marshaling is
//! covered by the live-container matrix, not here. Invalidation/live-growth
//! event semantics are proved at the namespace level in
//! `omnifs-engine/tests/namespace_surface.rs`.

#![allow(clippy::wildcard_imports)]

use super::Frontend;
use super::common::{DirSnapshot, ROOT_INO};
use crate::new_notifier_handle;
use omnifs_engine::{GitCloner, HostContext, MountRuntimes, Namespace, TreeNamespace};
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

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
    ns: Arc<TreeNamespace>,
    _registry: Arc<MountRuntimes>,
    _cache_dir: TempDir,
    _config_dir: TempDir,
    _providers_dir: TempDir,
}

/// Build a `Frontend` over a `TreeNamespace` using the single-mount context
/// so that paths are mount-relative (`hello/feed`) without a top-level mount dir.
fn build_harness() -> FuseHarness {
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());
    let providers_dir = tempfile::tempdir().expect("providers dir");

    let test_src = wasm_artifact_path("test_provider.wasm");
    assert!(
        test_src.exists(),
        "test_provider.wasm missing at {}. Run `just build providers` first.",
        test_src.display()
    );
    let test_bytes = std::fs::read(&test_src).expect("read test provider");
    let artifact =
        omnifs_workspace::provider::Artifact::from_bytes("test_provider.wasm", test_bytes)
            .expect("parse test provider artifact");
    let id = artifact.id();
    let store = omnifs_workspace::provider::ProviderStore::new(providers_dir.path());
    store.retain(&artifact).expect("retain test provider");

    let mount_config = format!(
        r#"{{
                "provider": {{ "id": "{id}", "meta": {{ "name": "test-provider" }} }},
                "mount": "test",
                "capabilities": {{ "domains": ["httpbin.org"] }},
                "config": {{}}
            }}"#
    );

    let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")).unwrap());
    let mounts_dir = tempfile::tempdir().expect("mounts dir");
    std::fs::write(mounts_dir.path().join("test.json"), mount_config.as_bytes())
        .expect("write mount spec");
    let desired =
        omnifs_workspace::mounts::Registry::load(mounts_dir.path()).expect("load mount snapshot");
    let registry = Arc::new(
        MountRuntimes::load(
            HostContext::new(
                cache_dir.path(),
                &paths.config_dir,
                providers_dir.path(),
                &paths.credentials_file,
            ),
            cloner,
            &desired,
            &tokio::runtime::Handle::current(),
        )
        .expect("registry init"),
    );

    let rt = tokio::runtime::Handle::current();
    let runtime = registry.get("test").expect("load test mount");

    let ns = TreeNamespace::single("test".to_string(), runtime, rt.clone());
    let fs = Frontend::new(
        rt,
        Arc::clone(&ns) as Arc<dyn Namespace>,
        new_notifier_handle(),
    );

    FuseHarness {
        fs,
        ns,
        _registry: registry,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
        _providers_dir: providers_dir,
    }
}

impl FuseHarness {
    async fn opendir(&self, ino: u64) -> DirSnapshot {
        self.fs.do_opendir(ino).await.expect("opendir")
    }

    /// Resolve `name` under `parent_ino` to its inode.
    async fn lookup(&self, parent_ino: u64, name: &str) -> u64 {
        let (ino, _attr, _ttl) = self.fs.do_lookup(parent_ino, name).await.expect("lookup");
        ino
    }

    /// Open `ino` and read the requested ranges, concatenating the bytes.
    async fn open_and_read(&self, ino: u64, reads: &[(u64, u32)]) -> Vec<u8> {
        let fh = self.fs.alloc_fh();
        self.fs.do_open(ino, fh).await.expect("open");
        let mut out = Vec::new();
        for &(offset, size) in reads {
            out.extend(self.fs.do_read(ino, fh, offset, size).await.expect("read"));
        }
        self.fs.do_release(fh);
        out
    }

    fn names(snapshot: &DirSnapshot) -> Vec<String> {
        snapshot.iter().map(|(_, name, _)| name.clone()).collect()
    }

    fn contains(snapshot: &DirSnapshot, name: &str) -> bool {
        snapshot.iter().any(|(_, n, _)| n == name)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_enumerates_and_descends() {
    let h = build_harness();

    let root = h.opendir(ROOT_INO).await;
    assert!(
        FuseHarness::contains(&root, "hello"),
        "root lists hello: {:?}",
        FuseHarness::names(&root)
    );

    let hello = h.lookup(ROOT_INO, "hello").await;
    let listing = h.opendir(hello).await;
    assert!(
        FuseHarness::contains(&listing, "message"),
        "hello lists message: {:?}",
        FuseHarness::names(&listing)
    );

    // A path keeps one inode across a readdir and a lookup.
    let message_via_lookup = h.lookup(hello, "message").await;
    let message_ino = listing
        .iter()
        .find(|(_, name, _)| name == "message")
        .map(|(ino, _, _)| *ino)
        .expect("message in listing");
    assert_eq!(
        message_via_lookup, message_ino,
        "readdir and lookup mint the same inode for a path"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_file_buffers_once_and_slices() {
    let h = build_harness();
    let hello = h.lookup(ROOT_INO, "hello").await;
    let message = h.lookup(hello, "message").await;

    // A whole read serves the payload; a sliced read comes from the same buffer.
    let whole = h.open_and_read(message, &[(0, 64)]).await;
    assert_eq!(whole, b"Hello, world!");

    let spliced = h.open_and_read(message, &[(2, 4), (0, 5)]).await;
    assert_eq!(&spliced, b"llo,Hello", "slices come from the per-fh buffer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ranged_read_reads_through_and_reuses_one_handle() {
    let h = build_harness();
    let hello = h.lookup(ROOT_INO, "hello").await;
    let ranged = h.lookup(hello, "ranged").await;

    let fh = h.fs.alloc_fh();
    h.fs.do_open(ranged, fh).await.expect("open ranged");
    assert!(
        h.fs.ranged_fhs.contains_key(&fh),
        "a ranged file binds a read-through handle, not a whole buffer"
    );

    let mid = h.fs.do_read(ranged, fh, 2, 4).await.expect("mid read");
    assert_eq!(mid, b"cdef");
    let head = h.fs.do_read(ranged, fh, 0, 3).await.expect("head read");
    assert_eq!(head, b"abc");
    assert_eq!(
        h.ns.ranged_open_count(),
        1,
        "the two read-throughs reuse the namespace's single provider open"
    );
    h.fs.do_release(fh);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pagination_control_is_a_readable_file_node() {
    let h = build_harness();
    let hello = h.lookup(ROOT_INO, "hello").await;
    let feed = h.lookup(hello, "feed").await;

    let page0 = h.opendir(feed).await;
    let names = FuseHarness::names(&page0);
    assert!(
        names.contains(&"@next".to_string()) && names.contains(&"@all".to_string()),
        "the feed surfaces pagination controls: {names:?}"
    );
    assert!(
        names.contains(&"item-0".to_string()),
        "the feed surfaces its first items: {names:?}"
    );

    // Opening @next reads the control's status exactly once (whole buffer).
    let next = h.lookup(feed, "@next").await;
    let status = h.open_and_read(next, &[(0, 4096)]).await;
    assert!(!status.is_empty(), "opening @next yields its status bytes");
}
