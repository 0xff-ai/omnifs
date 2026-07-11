//! Characterization: cache effect application order and object deletion.
//!
//! `object_cache_test.rs` covers object-vs-listing invalidation and the write
//! fence; this file covers two ordering properties of `EffectApplier`:
//!
//! - one effect batch applies canonical stores, fs writes, and the dirents merge
//!   together; and
//! - invalidations run LAST within a batch, so an `Invalidation::Object` in the
//!   same batch deletes the writes it targets. Deletion is physical (index and
//!   bytes gone, a subsequent cold read misses), not a generation-only fence.

use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::RecordKind;
use omnifs_engine::view::DirentsPayload;
use omnifs_itest::make_initialized_runtime;
use omnifs_wit::provider::types::{
    ByteSource, CanonicalStore, Effects, FileAttrs, FileOut, FileSize, FsKind, FsWrite,
    Invalidation, LogicalId, Stability,
};

const CONFIG: &str = r#"
{
    "provider": "test_provider.wasm",
    "mount": "test",
    "capabilities": { "domains": ["httpbin.org"] }
}
"#;

fn p(value: &str) -> Path {
    Path::parse(value).unwrap()
}

/// A distinct logical object id keyed by `tag` (captures make the id unique).
fn object_id(tag: &str) -> LogicalId {
    LogicalId {
        kind: "test.object".to_string(),
        captures: vec![omnifs_wit::provider::types::IdCapture {
            name: "tag".to_string(),
            value: tag.to_string(),
        }],
    }
}

fn file_out(bytes: &[u8]) -> FileOut {
    FileOut {
        content_type: None,
        attrs: FileAttrs {
            size: FileSize::Exact(bytes.len() as u64),
            stability: Stability::Stable,
            version_token: None,
        },
        bytes: ByteSource::Inline(bytes.to_vec()),
    }
}

fn canonical_store(id: &LogicalId, leaf: &str, bytes: &[u8]) -> CanonicalStore {
    CanonicalStore {
        id: id.clone(),
        validator: None,
        bytes: bytes.to_vec(),
        view_leaves: vec![leaf.to_string()],
    }
}

fn dir_write(path: &str) -> FsWrite {
    FsWrite {
        id: None,
        path: path.to_string(),
        kind: FsKind::Directory(false),
    }
}

fn file_write(id: Option<LogicalId>, path: &str, bytes: &[u8]) -> FsWrite {
    FsWrite {
        id,
        path: path.to_string(),
        kind: FsKind::File(file_out(bytes)),
    }
}

fn dirent_names(harness: &omnifs_itest::RuntimeHarness, path: &str) -> Vec<String> {
    let record = harness
        .runtime
        .cache()
        .cache_get(&p(path), RecordKind::Dirents, None)
        .expect("dirents must be cached");
    DirentsPayload::deserialize(&record.payload)
        .expect("dirents decode")
        .entries
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// One effect batch carrying a canonical store, fs file writes, and a directory
/// write applies all three: the canonical object lands, the plain fs leaves are
/// cached, and the parent directory's dirents merge to carry the new children.
#[test]
fn one_batch_applies_canonical_fs_and_dirents_merge() {
    let harness = make_initialized_runtime(CONFIG);
    let id = object_id("canon");
    let effects = Effects {
        canonical: vec![canonical_store(&id, "/o/r/item.json", b"{\"n\":42}")],
        fs: vec![
            dir_write("/d"),
            file_write(None, "/d/a", b"aaa"),
            file_write(None, "/d/b", b"bbb"),
        ],
        invalidations: Vec::new(),
    };

    harness
        .runtime
        .apply_effects_for_test(&effects, harness.runtime.cache().current_generation());

    // Canonical store landed.
    let canonical = harness
        .runtime
        .cache()
        .cached_canonical_for(&p("/o/r/item.json"))
        .expect("canonical object stored from the batch");
    assert_eq!(canonical.bytes, b"{\"n\":42}");

    // fs file leaves cached.
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p("/d/a"), RecordKind::File, None)
            .is_some(),
        "fs file write landed as a view leaf"
    );
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p("/d/b"), RecordKind::File, None)
            .is_some()
    );

    // Dirents merge produced the parent listing carrying both children.
    let names = dirent_names(&harness, "/d");
    assert!(
        names.contains(&"a".to_string()) && names.contains(&"b".to_string()),
        "dirents merge carries both fs children, got {names:?}"
    );
}

/// Invalidations run LAST within a batch: an `Invalidation::Object` in the same
/// batch as the canonical + fs writes that create that object deletes them. A
/// second object written in the same batch but not invalidated survives, proving
/// the writes did land and only the same-batch invalidation removed the target.
#[test]
fn invalidations_run_after_writes_in_one_batch() {
    let harness = make_initialized_runtime(CONFIG);
    let target = object_id("target");
    let survivor = object_id("survivor");

    let effects = Effects {
        canonical: vec![
            canonical_store(&target, "/objects/target/view", b"target-bytes"),
            canonical_store(&survivor, "/objects/survivor/view", b"survivor-bytes"),
        ],
        fs: vec![file_write(
            Some(target.clone()),
            "/objects/target/file",
            b"leaf",
        )],
        invalidations: vec![Invalidation::Object(target.clone())],
    };

    harness
        .runtime
        .apply_effects_for_test(&effects, harness.runtime.cache().current_generation());

    // The same-batch Object(target) invalidation ran after the writes: had it run
    // first, `target` would not yet be indexed, nothing would be deleted, and both
    // the canonical view and the fs leaf would survive. Their absence pins the
    // "invalidations last" ordering.
    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p("/objects/target/view"))
            .is_none(),
        "same-batch object invalidation deletes the canonical it targets"
    );
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p("/objects/target/file"), RecordKind::File, None)
            .is_none(),
        "same-batch object invalidation deletes the fs leaf it targets"
    );

    // The uninvalidated object proves the batch's canonical writes did apply.
    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p("/objects/survivor/view"))
            .is_some(),
        "an object not named by any invalidation survives the batch"
    );
}

/// An `Invalidation::Object` physically deletes the durable object: the canonical
/// bytes and the id->path index are both gone, so a subsequent cold read misses.
/// This is deletion, not a generation-only fence (a fence would leave the bytes
/// and index in place while rejecting only stale writes).
#[test]
fn object_invalidation_deletes_durable_object_not_just_fences() {
    let harness = make_initialized_runtime(CONFIG);
    let id = object_id("durable");
    let leaf = "/objects/durable/view";

    harness.runtime.apply_effects_for_test(
        &Effects {
            canonical: vec![canonical_store(&id, leaf, b"durable-bytes")],
            fs: Vec::new(),
            invalidations: Vec::new(),
        },
        harness.runtime.cache().current_generation(),
    );
    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(leaf))
            .is_some(),
        "canonical object is durably stored before invalidation"
    );
    // Capture the object's id bytes through the forward index (the host's
    // `ObjectId` type is crate-private), to probe the reverse index after delete.
    let id_bytes = harness
        .runtime
        .cache()
        .id_of_path(&p(leaf))
        .expect("the leaf is indexed to the object before invalidation");

    harness.runtime.apply_effects_for_test(
        &Effects {
            canonical: Vec::new(),
            fs: Vec::new(),
            invalidations: vec![Invalidation::Object(id.clone())],
        },
        harness.runtime.cache().current_generation(),
    );

    // A subsequent cold read misses: the durable canonical is gone.
    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(leaf))
            .is_none(),
        "object invalidation deletes the durable canonical bytes"
    );
    // Physical deletion, not a fence: the reverse index carries no paths and the
    // forward index no longer maps the leaf.
    assert!(
        harness.runtime.cache().paths_for_id(&id_bytes).is_empty(),
        "object invalidation removes the id->path index entirely"
    );
    assert!(
        harness.runtime.cache().id_of_path(&p(leaf)).is_none(),
        "the leaf is no longer indexed to the deleted object"
    );
}
