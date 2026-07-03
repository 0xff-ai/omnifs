//! Characterization: live-file growth through the kernel-free Tree surface.
//!
//! N2 freezes CURRENT behavior. There is no live-follow coverage in omnifs-itest
//! today (the FUSE follow-pump tests are Linux-only), so this pins the neutral
//! `Tree` surface a renderer's `tail -f` loop drives: opening the test provider's
//! `Stability::Live` `/hello/volatile-tail` yields a ranged handle whose reported
//! size is unknown, a follow read observes freshly appended bytes at successive
//! offsets, and the shared `probe_live_growth` advances the observed end
//! monotonically (the learned size a renderer reports for a growing file).
//!
//! The `LiveTailReader` route serves `tail:{offset}\n` for any offset and never
//! signals EOF, modelling an upstream that keeps growing while observed.

#![cfg(not(target_os = "wasi"))]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use omnifs_core::path::Path;
use omnifs_core::view::{FileAttrsCache, FileSize, ReadMode, Stability};
use omnifs_host::Runtime;
use omnifs_itest::{RuntimeHarness, make_engine, make_runtime};
use omnifs_tree::{Node, RequestCtx, Tree, probe_live_growth};
use tempfile::TempDir;

struct LiveTree {
    tree: Tree,
    runtime: Arc<Runtime>,
    ctx: RequestCtx,
    _clone_dir: TempDir,
    _cache_dir: TempDir,
    _config_dir: TempDir,
}

fn live_tree() -> LiveTree {
    let engine = make_engine();
    let RuntimeHarness {
        clone_dir,
        cache_dir,
        config_dir,
        runtime,
        ..
    } = make_runtime(&engine);
    let runtime = Arc::new(runtime);
    let tree = Tree::for_runtime(Arc::clone(&runtime), "test");
    LiveTree {
        tree,
        runtime,
        ctx: RequestCtx::default(),
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
    }
}

fn path(s: &str) -> Path {
    Path::parse(s).unwrap()
}

/// The deferred-ranged `Node` a renderer hands to `Tree::open` for a live leaf.
/// A lookup only leaves a `Deferred(Full)` placeholder; the renderer classifies
/// the leaf as ranged, and `open` fixes the real (Live/Unknown) attrs from the
/// provider's `open-file` result.
fn live_node(path_str: &str) -> Node {
    let attrs =
        FileAttrsCache::deferred(FileSize::Unknown, ReadMode::Ranged, Stability::Live, None)
            .expect("live deferred attrs");
    Node::provider_file("test".to_string(), path(path_str), Some(attrs))
}

/// A live file opens to a ranged handle reporting `Live` stability and an unknown
/// size; a follow read observes freshly appended bytes at each new offset, and
/// the shared growth probe advances the observed end monotonically.
#[tokio::test(flavor = "multi_thread")]
async fn live_file_grows_and_follow_read_observes_appended_bytes() {
    let t = live_tree();
    let handle = t
        .tree
        .open(&live_node("/hello/volatile-tail"), &t.ctx)
        .await
        .expect("open volatile-tail")
        .expect("volatile-tail is a ranged source");

    assert_eq!(
        handle.attrs().stability(),
        Stability::Live,
        "the leaf opens as Live"
    );
    assert_eq!(
        handle.attrs().size(),
        FileSize::Unknown,
        "a live tail has no fixed size"
    );

    // Follow read: successive offsets return fresh tail bytes (never EOF), the
    // shape a follower sees as the file appends.
    let head = handle.read(0, 128).await.expect("read tail head");
    assert_eq!(head.bytes, b"tail:0\n");
    assert!(!head.eof, "a live tail read never reports EOF");
    let appended = handle.read(7, 128).await.expect("read appended tail");
    assert_eq!(
        appended.bytes, b"tail:7\n",
        "a later offset yields freshly appended bytes"
    );
    assert!(!appended.eof);

    // Learned-size promotion: the shared growth probe advances the observed end
    // monotonically. Each `tail:{offset}\n` chunk is 7 bytes for single-digit
    // offsets, so the end walks 0 -> 7 -> 14.
    let observed = handle.observed_end();
    assert_eq!(
        observed.load(Ordering::Relaxed),
        0,
        "the observed end starts at zero"
    );

    let first = probe_live_growth(
        t.runtime.as_ref(),
        handle.provider_handle(),
        &observed,
        65_536,
    )
    .await
    .expect("first growth probe");
    assert_eq!(first, Some(7), "the first probe learns 7 bytes at offset 0");
    assert_eq!(observed.load(Ordering::Relaxed), 7);

    let second = probe_live_growth(
        t.runtime.as_ref(),
        handle.provider_handle(),
        &observed,
        65_536,
    )
    .await
    .expect("second growth probe");
    assert_eq!(
        second,
        Some(14),
        "the second probe advances the end past the first"
    );
    assert_eq!(observed.load(Ordering::Relaxed), 14);

    // The observed end never regresses across further probes.
    let mut last = observed.load(Ordering::Relaxed);
    for _ in 0..3 {
        let end = probe_live_growth(
            t.runtime.as_ref(),
            handle.provider_handle(),
            &observed,
            65_536,
        )
        .await
        .expect("further growth probe")
        .expect("a live tail always grows");
        assert!(
            end > last,
            "the observed end grows monotonically: {end} > {last}"
        );
        last = end;
    }

    handle.close().expect("close live handle");
}
