//! Host and object-cache coherence tests.

use omnifs_core::path::Path;
use omnifs_engine::EngineError;
use omnifs_engine::test_support::ReadBytes;
use omnifs_engine::test_support::cache::{BatchRecord, RecordKind};
use omnifs_engine::test_support::clock::DYNAMIC_TTL_MILLIS;
use omnifs_engine::test_support::wit_protocol;
use omnifs_engine::view::{AttrPayload, FilePayload, LookupPayload};
use omnifs_itest::make_initialized_runtime;
use omnifs_wit::provider::types::{
    ByteSource, Effects, ErrorKind, FileAttrs, FileOut, FileSize, FsKind, FsWrite, IdCapture,
    Invalidation, LogicalId, PathOrPrefix, Stability,
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

fn issue_id() -> LogicalId {
    LogicalId {
        kind: "github.item".to_string(),
        captures: vec![
            IdCapture {
                name: "owner".to_string(),
                value: "o".to_string(),
            },
            IdCapture {
                name: "repo".to_string(),
                value: "r".to_string(),
            },
            IdCapture {
                name: "number".to_string(),
                value: "42".to_string(),
            },
        ],
    }
}

fn canonical_effect(id: &LogicalId, leaf: &str, bytes: &[u8], validator: Option<&str>) -> Effects {
    Effects {
        canonical: vec![omnifs_wit::provider::types::CanonicalStore {
            id: id.clone(),
            validator: validator.map(str::to_string),
            bytes: bytes.to_vec(),
            view_leaves: vec![leaf.to_string()],
        }],
        fs: Vec::new(),
        invalidations: Vec::new(),
    }
}

fn preload_file_effect(id: &LogicalId, path: &str, inline: &[u8]) -> Effects {
    Effects {
        canonical: Vec::new(),
        fs: vec![FsWrite {
            id: Some(id.clone()),
            path: path.to_string(),
            kind: FsKind::File(FileOut {
                content_type: None,
                attrs: FileAttrs {
                    size: FileSize::Exact(inline.len() as u64),
                    stability: Stability::Dynamic,
                    version_token: None,
                },
                bytes: ByteSource::Inline(inline.to_vec()),
            }),
        }],
        invalidations: Vec::new(),
    }
}

#[test]
fn canonical_eviction_drops_validator() {
    let harness = make_initialized_runtime(CONFIG);
    let id = issue_id();
    let leaf = "/o/r/issues/all/42/item.json";
    let bytes = br#"{"number":42}"#;
    let op_gen = harness.runtime.cache().current_generation();
    harness
        .runtime
        .apply_effects_for_test(&canonical_effect(&id, leaf, bytes, Some("etag")), op_gen);

    let cached = harness.runtime.cache().cached_canonical_for(&p(leaf));
    assert!(cached.is_some());
    let cached = cached.unwrap();
    assert_eq!(cached.bytes, bytes);
    assert_eq!(cached.validator.as_deref(), Some("etag"));

    let invalidate = Effects {
        invalidations: vec![Invalidation::Object(id.clone())],
        ..Effects {
            canonical: Vec::new(),
            fs: Vec::new(),
            invalidations: Vec::new(),
        }
    };
    harness
        .runtime
        .apply_effects_for_test(&invalidate, harness.runtime.cache().current_generation());

    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(leaf))
            .is_none()
    );
}

#[test]
fn fence_rejects_stale_preload_and_negative() {
    let harness = make_initialized_runtime(CONFIG);
    let id = issue_id();
    let leaf = "/o/r/issues/open/42/title";
    let op_gen0 = harness.runtime.cache().current_generation();

    let invalidate = Effects {
        invalidations: vec![Invalidation::Object(id.clone())],
        ..Effects {
            canonical: Vec::new(),
            fs: Vec::new(),
            invalidations: Vec::new(),
        }
    };
    harness
        .runtime
        .apply_effects_for_test(&invalidate, harness.runtime.cache().current_generation());

    harness
        .runtime
        .apply_effects_for_test(&preload_file_effect(&id, leaf, b"stale"), op_gen0);
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p(leaf), RecordKind::File, None)
            .is_none(),
        "stale preload must not land after object invalidation"
    );

    harness
        .runtime
        .apply_not_found_negative(&p(leaf), Some(&id), op_gen0, 1_000);
    assert!(
        harness
            .runtime
            .cache()
            .negative_for(&p(leaf), 1_500)
            .is_none(),
        "stale negative must not land after object invalidation"
    );
}

#[test]
fn stale_canonical_fenced_by_midflight_invalidation() {
    let harness = make_initialized_runtime(CONFIG);
    let id = issue_id();
    let leaf = "/o/r/issues/open/42/title";
    let op_gen0 = harness.runtime.cache().current_generation();

    let invalidate = Effects {
        invalidations: vec![Invalidation::Object(id.clone())],
        ..Effects {
            canonical: Vec::new(),
            fs: Vec::new(),
            invalidations: Vec::new(),
        }
    };
    harness
        .runtime
        .apply_effects_for_test(&invalidate, harness.runtime.cache().current_generation());

    harness.runtime.apply_effects_for_test(
        &canonical_effect(&id, leaf, b"stale canonical", None),
        op_gen0,
    );
    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(leaf))
            .is_none()
    );
}

#[test]
fn leaf_records_share_one_deadline() {
    let harness = make_initialized_runtime(CONFIG);
    let path = "/o/r/issues/open/42/title";
    let now = 1_000u64;
    let ttl = 3_000u64;

    let mut batch = Vec::new();
    let meta = wit_protocol::entry_meta_from_kind(&omnifs_wit::provider::types::EntryKind::File(
        FileOut {
            content_type: None,
            attrs: FileAttrs {
                size: FileSize::Exact(5),
                stability: Stability::Dynamic,
                version_token: None,
            },
            bytes: ByteSource::Inline(b"title".to_vec()),
        },
    ));
    let lookup = LookupPayload::Positive(meta.clone());
    batch.push(BatchRecord::new(
        Path::parse(path).unwrap(),
        RecordKind::Lookup,
        None,
        omnifs_engine::test_support::cache::Record::new(
            RecordKind::Lookup,
            lookup.serialize().unwrap(),
        ),
    ));
    batch.push(BatchRecord::new(
        Path::parse(path).unwrap(),
        RecordKind::Attr,
        None,
        omnifs_engine::test_support::cache::Record::new(
            RecordKind::Attr,
            AttrPayload { meta }.serialize().unwrap(),
        ),
    ));
    batch.push(BatchRecord::new(
        Path::parse(path).unwrap(),
        RecordKind::File,
        None,
        omnifs_engine::test_support::cache::Record::new(
            RecordKind::File,
            FilePayload::new(None, b"title".to_vec())
                .serialize()
                .unwrap(),
        ),
    ));

    let runtime = &harness.runtime;
    let op_gen = runtime.cache().current_generation();
    assert!(runtime.cache().cache_view_leaf(
        &p(path),
        &batch,
        Some(now.saturating_add(ttl)),
        op_gen,
    ));

    for kind in RecordKind::ALL {
        if kind == RecordKind::Dirents {
            continue;
        }
        assert!(
            runtime
                .cache()
                .view_get(&p(path), kind, None, now + 999)
                .is_some(),
            "kind {kind:?} should be fresh at t=1999"
        );
        assert!(
            runtime
                .cache()
                .view_get(&p(path), kind, None, now + 4_000)
                .is_none(),
            "kind {kind:?} should expire at t=5000 with the shared stamp"
        );
    }
}

#[tokio::test]
async fn plain_path_ignores_unrelated_indexed_validator() {
    let harness = make_initialized_runtime(CONFIG);
    let path = "/hello/message";

    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(path))
            .is_none()
    );

    let _ = harness
        .runtime
        .namespace()
        .read_file(&p(path), "application/octet-stream".to_string())
        .await
        .expect("cold read dispatches to provider");

    let id = LogicalId {
        kind: "test.unrelated".to_string(),
        captures: vec![],
    };
    harness.runtime.apply_effects_for_test(
        &canonical_effect(&id, path, b"unrelated canonical", Some("unrelated-v1")),
        harness.runtime.cache().current_generation(),
    );

    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(path))
            .is_some()
    );

    let read = harness
        .runtime
        .namespace()
        .read_file(&p(path), "application/octet-stream".to_string(), None)
        .await
        .expect("indexed read still dispatches to plain provider handler");
    let ReadBytes::Inline(bytes) = read.bytes else {
        panic!("plain handler must return inline bytes");
    };
    assert_eq!(bytes, b"Hello, world!");
}

#[test]
fn object_vs_listing_invalidation() {
    let harness = make_initialized_runtime(CONFIG);
    let id = issue_id();
    let open_leaf = "/o/r/issues/open/42/title";
    let all_leaf = "/o/r/issues/all/42/title";

    harness.runtime.apply_effects_for_test(
        &Effects {
            canonical: vec![omnifs_wit::provider::types::CanonicalStore {
                id: id.clone(),
                validator: None,
                bytes: b"{}".to_vec(),
                view_leaves: vec![open_leaf.to_string(), all_leaf.to_string()],
            }],
            fs: Vec::new(),
            invalidations: Vec::new(),
        },
        harness.runtime.cache().current_generation(),
    );

    harness.runtime.apply_effects_for_test(
        &Effects {
            invalidations: vec![Invalidation::Object(id.clone())],
            ..Effects {
                canonical: Vec::new(),
                fs: Vec::new(),
                invalidations: Vec::new(),
            }
        },
        harness.runtime.cache().current_generation(),
    );
    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(open_leaf))
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(all_leaf))
            .is_none()
    );

    harness.runtime.apply_effects_for_test(
        &canonical_effect(&id, open_leaf, b"{}", None),
        harness.runtime.cache().current_generation(),
    );
    harness.runtime.apply_effects_for_test(
        &preload_file_effect(&id, all_leaf, b"listed"),
        harness.runtime.cache().current_generation(),
    );

    harness.runtime.apply_effects_for_test(
        &Effects {
            invalidations: vec![Invalidation::Listing(PathOrPrefix::Prefix(
                "/o/r/issues/all".to_string(),
            ))],
            ..Effects {
                canonical: Vec::new(),
                fs: Vec::new(),
                invalidations: Vec::new(),
            }
        },
        harness.runtime.cache().current_generation(),
    );

    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p(all_leaf), RecordKind::File, None)
            .is_none(),
        "listing prefix invalidation evicts view leaves under the prefix"
    );
    assert!(
        harness
            .runtime
            .cache()
            .cached_canonical_for(&p(open_leaf))
            .is_some(),
        "object canonical must survive listing-only invalidation"
    );
}

#[test]
fn negative_returns_enoent_until_deadline_or_invalidate() {
    let harness = make_initialized_runtime(CONFIG);
    let id = issue_id();
    let path = "/o/r/issues/open/42/missing";
    let now = 10_000u64;
    let op_gen = harness.runtime.cache().current_generation();

    harness
        .runtime
        .apply_not_found_negative(&p(path), Some(&id), op_gen, now);

    assert!(
        harness
            .runtime
            .cache()
            .negative_for(&p(path), now + 100)
            .is_some()
    );

    assert!(
        harness
            .runtime
            .cache()
            .negative_for(&p(path), now + DYNAMIC_TTL_MILLIS + 1)
            .is_none(),
        "negative expires after TTL"
    );

    harness
        .runtime
        .apply_not_found_negative(&p(path), Some(&id), op_gen, now);
    assert!(
        harness
            .runtime
            .cache()
            .negative_for(&p(path), now + 100)
            .is_some()
    );

    let invalidate = Effects {
        invalidations: vec![Invalidation::Object(id)],
        ..Effects {
            canonical: Vec::new(),
            fs: Vec::new(),
            invalidations: Vec::new(),
        }
    };
    harness
        .runtime
        .apply_effects_for_test(&invalidate, harness.runtime.cache().current_generation());
    assert!(
        harness
            .runtime
            .cache()
            .negative_for(&p(path), now + 100)
            .is_none(),
        "object invalidation clears the negative immediately"
    );
}

#[tokio::test]
async fn negative_short_circuits_read_without_provider_dispatch() {
    let harness = make_initialized_runtime(CONFIG);
    let id = issue_id();
    let path = "/no/such/leaf";
    let now = 5_000u64;
    harness.runtime.apply_not_found_negative(
        &p(path),
        Some(&id),
        harness.runtime.cache().current_generation(),
        now,
    );

    let error = harness
        .runtime
        .namespace()
        .read_file(&p(path), "application/octet-stream".to_string())
        .await
        .expect_err("negative must surface as ENOENT");

    match error {
        EngineError::ProviderError(e) => assert_eq!(e.kind, ErrorKind::NotFound),
        other => panic!("expected provider NotFound, got {other:?}"),
    }
}
